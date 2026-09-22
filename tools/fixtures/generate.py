"""Generate the mock-interview fixtures in testdata/ from the scripts in tools/fixtures/scripts/.

For each script this synthesises every turn with Piper, lays the turns on one shared timeline and
writes, under testdata/<name>/:

  interviewer.wav  candidate.wav   16 kHz, mono, 16-bit PCM, identical length. Each holds only its
                                   speaker's audio; everywhere else is a -60 dBFS noise floor so a
                                   VAD sees realistic (not digital) silence.
  questions.json                   every turn marked is_question, with start/end from the audio
  turns.json                       every turn with its timing, for the eval harness (#10)
  script.md                        readable transcript with timestamps

Reproducibility: Piper's VITS models sample noise inside the ONNX graph, so plain synthesis differs
run to run. We seed ONNX Runtime from the fixture name before loading each voice and synthesise the
turns in script order, which makes the output byte-identical across runs on the same machine and
package versions. Editing one turn changes the audio of every later turn of that speaker.

Usage (from the repo root):  just fixtures            # all three
                             cd tools && uv run python fixtures/generate.py mixed
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import wave
import zlib
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import onnxruntime as ort
from piper import PiperVoice
from piper.download_voices import download_voice

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = Path(__file__).resolve().parent / "scripts"
VOICE_CACHE = Path.home() / ".cache" / "dmn" / "piper"

RATE = 16_000
TARGET_DBFS = -20.0  # active-speech RMS per turn; verify.py accepts -25 .. -15
PEAK_LIMIT = 0.95
FLOOR_DBFS = -60.0
SENTENCE_GAP_S = 0.25  # between sentences inside one turn (below the 300 ms VAD pause)
LEAD_IN_S = 1.0
TAIL_S = 1.5
DEFAULT_PAUSE_S = 0.8
FRAME = 320  # 20 ms at 16 kHz, same frame the VAD uses


@dataclass
class Placed:
    index: int
    speaker: str
    start: float
    end: float
    text: str
    is_question: bool
    kind: str | None


def dbfs(x: np.ndarray) -> float:
    rms = float(np.sqrt(np.mean(np.square(x, dtype=np.float64)))) if x.size else 0.0
    return 20.0 * np.log10(rms) if rms > 0 else -120.0


def active_rms_dbfs(x: np.ndarray, gate_dbfs: float = -45.0) -> float:
    """RMS over 20 ms frames louder than the gate, so pauses inside a turn don't dilute it."""
    n = len(x) // FRAME
    if n == 0:
        return dbfs(x)
    frames = x[: n * FRAME].reshape(n, FRAME)
    frame_db = 10.0 * np.log10(np.mean(np.square(frames, dtype=np.float64), axis=1) + 1e-20)
    loud = frames[frame_db > gate_dbfs]
    return dbfs(loud.reshape(-1)) if loud.size else -120.0


def resample(x: np.ndarray, src: int, dst: int) -> np.ndarray:
    """Band-limited FFT resampling of one turn. Zero-padding both ends avoids wrap-around clicks."""
    if src == dst:
        return x.astype(np.float32)
    pad = src // 10
    padded = np.concatenate([np.zeros(pad), x.astype(np.float64), np.zeros(pad)])
    n_out = int(round(len(padded) * dst / src))
    spec = np.fft.rfft(padded)
    keep = n_out // 2 + 1
    out_spec = np.zeros(keep, dtype=np.complex128)
    m = min(keep, len(spec))
    out_spec[:m] = spec[:m]
    y = np.fft.irfft(out_spec, n=n_out) * (n_out / len(padded))
    trim = int(round(pad * dst / src))
    return y[trim : n_out - trim].astype(np.float32)


def seed_for(name: str, role: str) -> int:
    return zlib.crc32(f"dmn-fixture:{name}:{role}".encode()) & 0x7FFFFFFF


def ensure_voice(voice: str) -> Path:
    model = VOICE_CACHE / f"{voice}.onnx"
    if not (model.exists() and model.with_suffix(".onnx.json").exists()):
        VOICE_CACHE.mkdir(parents=True, exist_ok=True)
        print(f"  downloading voice {voice} -> {VOICE_CACHE}")
        download_voice(voice, VOICE_CACHE)
    return model


def load_voice(voice: str, seed: int) -> PiperVoice:
    model = ensure_voice(voice)
    ort.set_seed(seed)
    loaded = PiperVoice.load(model)
    ort.set_seed(seed)
    return loaded


def synth_turn(voice: PiperVoice, text: str) -> np.ndarray:
    """Synthesise one turn at 16 kHz, sentences joined by a short gap, levelled to TARGET_DBFS."""
    parts: list[np.ndarray] = []
    src_rate = voice.config.sample_rate
    gap = np.zeros(int(SENTENCE_GAP_S * RATE), dtype=np.float32)
    for chunk in voice.synthesize(text):
        audio = resample(chunk.audio_float_array, chunk.sample_rate or src_rate, RATE)
        if parts:
            parts.append(gap)
        parts.append(audio)
    if not parts:
        raise ValueError(f"Piper produced no audio for: {text[:60]!r}")
    x = np.concatenate(parts)
    gain = 10.0 ** ((TARGET_DBFS - active_rms_dbfs(x)) / 20.0)
    x = x * gain
    peak = float(np.max(np.abs(x)))
    if peak > PEAK_LIMIT:
        x = x * (PEAK_LIMIT / peak)
    return x.astype(np.float32)


def write_wav(path: Path, x: np.ndarray) -> None:
    pcm = np.clip(np.round(x * 32767.0), -32768, 32767).astype("<i2")
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(pcm.tobytes())


def fmt_ts(t: float) -> str:
    m, s = divmod(t, 60.0)
    return f"{int(m):02d}:{s:05.2f}"


def generate(script_path: Path, out_root: Path) -> dict:
    spec = json.loads(script_path.read_text())
    name = spec["name"]
    print(f"{name}: synthesising {len(spec['turns'])} turns")
    voices = {
        "interviewer": load_voice(spec["interviewer_voice"], seed_for(name, "interviewer")),
        "candidate": load_voice(spec["candidate_voice"], seed_for(name, "candidate")),
    }

    placed: list[Placed] = []
    audio: list[tuple[str, int, np.ndarray]] = []
    cursor = LEAD_IN_S
    for i, turn in enumerate(spec["turns"]):
        speaker = turn["speaker"]
        if speaker not in voices:
            raise ValueError(f"{name} turn {i}: unknown speaker {speaker!r}")
        x = synth_turn(voices[speaker], turn["text"])
        start = cursor
        end = start + len(x) / RATE
        placed.append(
            Placed(
                index=i,
                speaker=speaker,
                start=round(start, 3),
                end=round(end, 3),
                text=turn["text"],
                is_question=bool(turn.get("is_question", False)),
                kind=turn.get("kind"),
            )
        )
        audio.append((speaker, int(round(start * RATE)), x))
        cursor = end + float(turn.get("pause_after_s", DEFAULT_PAUSE_S))

    total = int(round((cursor + TAIL_S) * RATE))
    rng = np.random.default_rng(seed_for(name, "floor"))
    floor_amp = 10.0 ** (FLOOR_DBFS / 20.0)
    tracks = {
        role: (rng.standard_normal(total) * floor_amp).astype(np.float32) for role in voices
    }
    for speaker, offset, x in audio:
        tracks[speaker][offset : offset + len(x)] += x

    out = out_root / name
    out.mkdir(parents=True, exist_ok=True)
    write_wav(out / "interviewer.wav", tracks["interviewer"])
    write_wav(out / "candidate.wav", tracks["candidate"])

    questions = [
        {"t": p.start, "end": p.end, "text": p.text, "kind": p.kind}
        for p in placed
        if p.is_question
    ]
    turns = [
        {
            "i": p.index,
            "speaker": p.speaker,
            "t": p.start,
            "end": p.end,
            "is_question": p.is_question,
            "kind": p.kind,
            "text": p.text,
        }
        for p in placed
    ]
    (out / "questions.json").write_text(json.dumps(questions, indent=2, ensure_ascii=False) + "\n")
    (out / "turns.json").write_text(json.dumps(turns, indent=2, ensure_ascii=False) + "\n")

    duration = total / RATE
    lines = [
        f"# {spec.get('title', name)}",
        "",
        f"Generated by `tools/fixtures/generate.py` from `tools/fixtures/scripts/{script_path.name}`. "
        f"Duration {fmt_ts(duration)}; {len(questions)} real questions (marked **Q**). "
        f"Voices: interviewer `{spec['interviewer_voice']}`, candidate `{spec['candidate_voice']}`.",
        "",
    ]
    for p in placed:
        who = "Interviewer" if p.speaker == "interviewer" else "Candidate"
        tag = f" **Q** ({p.kind})" if p.is_question else (f" _({p.kind})_" if p.kind else "")
        lines.append(f"- `{fmt_ts(p.start)}–{fmt_ts(p.end)}` **{who}**{tag}: {p.text}")
    (out / "script.md").write_text("\n".join(lines) + "\n")

    digests = {
        f: hashlib.sha256((out / f).read_bytes()).hexdigest()[:16]
        for f in ("interviewer.wav", "candidate.wav")
    }
    summary = {
        "name": name,
        "duration_s": round(duration, 2),
        "turns": len(placed),
        "questions": len(questions),
        "sha256": digests,
    }
    print(f"  {name}: {fmt_ts(duration)}, {len(placed)} turns, {len(questions)} questions, {digests}")
    return summary


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("names", nargs="*", help="fixture names (default: every script)")
    ap.add_argument("--out", type=Path, default=ROOT / "testdata", help="output root")
    args = ap.parse_args()

    scripts = sorted(SCRIPTS.glob("*.json"))
    if args.names:
        wanted = set(args.names)
        scripts = [s for s in scripts if s.stem in wanted]
        missing = wanted - {s.stem for s in scripts}
        if missing:
            print(f"no script for: {', '.join(sorted(missing))}", file=sys.stderr)
            return 2
    for s in scripts:
        generate(s, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
