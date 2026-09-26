"""Generate the mock-interview fixtures in testdata/ from the scripts in tools/fixtures/scripts/.

For each script this synthesises every turn with Piper, lays the turns on one shared timeline and
writes, under testdata/<name>/:

  interviewer.wav  candidate.wav   16 kHz, mono, 16-bit PCM, identical length. Each holds only its
                                   speaker's audio; everywhere else is a -60 dBFS noise floor so a
                                   VAD sees realistic (not digital) silence.
  questions.json                   every turn marked is_question, with start/end from the audio
  turns.json                       every turn with its timing, for the eval harness (#10). Besides
                                   the clip window `t`/`end`, each turn carries `voiced_t` and
                                   `voiced_end`: where sound actually starts and stops inside that
                                   window. A Piper clip holds 20-60 ms of silence before the first
                                   word and, for the Norman voice, 150-250 ms after the last one,
                                   so `t`/`end` are not speech boundaries; see issue #27.
  script.md                        readable transcript with timestamps

Reproducibility: Piper's VITS models sample noise inside the ONNX graph, so plain synthesis differs
run to run. We seed ONNX Runtime from the fixture name before loading each voice and synthesise the
turns in script order, which makes the output byte-identical across runs on the same machine and
package versions. Editing one turn changes the audio of every later turn of that speaker.

Usage (from the repo root):  just fixtures            # all four
                             cd tools && uv run python fixtures/generate.py mixed
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
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
VOICED_DBFS = -40.0  # sound vs the -60 dBFS floor, for turns.json's voiced_t / voiced_end
SPLIT_MARK = "||"  # a mid-turn pause in a script's text; see synth_turn
DEFAULT_SPLIT_GAP_S = 0.9  # above the VAD's utterance close (500 ms since #16, 700 ms before)


@dataclass
class Placed:
    index: int
    speaker: str
    start: float
    end: float
    text: str
    is_question: bool
    kind: str | None
    voiced_start: float = 0.0
    voiced_end: float = 0.0


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


def spoken(text: str) -> str:
    """The turn's text as said: the `||` pause markers removed (see `synth_turn`)."""
    return " ".join(p.strip() for p in text.split(SPLIT_MARK))


def synth_turn(voice: PiperVoice, text: str, split_gap_s: float = DEFAULT_SPLIT_GAP_S) -> np.ndarray:
    """Synthesise one turn at 16 kHz, sentences joined by a short gap, levelled to TARGET_DBFS.

    A `||` in the text is a pause inside the turn: the speaker stops mid-sentence ("And if the
    limiter's state lived in Redis || and Redis went down, what then?") for `split_gap_s`, long
    enough that the VAD closes the utterance there. The turn is still one turn with one span, so
    a question split this way is one question in questions.json. Without a marker the audio is
    exactly what it was before markers existed.
    """
    parts: list[np.ndarray] = []
    src_rate = voice.config.sample_rate
    gap = np.zeros(int(SENTENCE_GAP_S * RATE), dtype=np.float32)
    split_gap = np.zeros(int(split_gap_s * RATE), dtype=np.float32)
    for n, piece in enumerate(text.split(SPLIT_MARK)):
        first = True
        for chunk in voice.synthesize(piece.strip()):
            audio = resample(chunk.audio_float_array, chunk.sample_rate or src_rate, RATE)
            if parts:
                parts.append(split_gap if n > 0 and first else gap)
            parts.append(audio)
            first = False
    if not parts:
        raise ValueError(f"Piper produced no audio for: {text[:60]!r}")
    x = np.concatenate(parts)
    gain = 10.0 ** ((TARGET_DBFS - active_rms_dbfs(x)) / 20.0)
    x = x * gain
    peak = float(np.max(np.abs(x)))
    if peak > PEAK_LIMIT:
        x = x * (PEAK_LIMIT / peak)
    return x.astype(np.float32)


def quantise(x: np.ndarray) -> np.ndarray:
    """The int16 samples that go into the WAV, so callers can measure exactly what consumers read."""
    return np.clip(np.round(x * 32767.0), -32768, 32767).astype("<i2")


def voiced_bounds(track: np.ndarray, start: float, end: float) -> tuple[float, float, bool]:
    """Where sound starts and stops inside the clip window [start, end) of one track.

    Returns `(voiced_t, voiced_end, found)`. `track` holds the samples as written, in [-1, 1);
    verify.py re-measures a WAV the same way.

    The definition issue #27 accepted for VAD accuracy, applied to every sample in the window: the
    first and last sample whose magnitude reaches VOICED_DBFS. The tracks' -60 dBFS Gaussian floor
    would need a 10-sigma excursion to reach -40 dBFS, so on these fixtures a crossing is always
    speech and no frame gate is needed.

    The bounds are widened to the enclosing millisecond, never narrowed, so the recorded pair always
    contains every sample that crossed the threshold and a one-sample turn still gets a real span
    rather than collapsing. `found` is False only when nothing in the window crosses at all; the
    window itself is then returned and verify.py warns, so a silent clip is noticed instead of
    being absorbed into a full-width "voiced" span.
    """
    lo, hi = int(round(start * RATE)), min(int(round(end * RATE)), len(track))
    win = np.abs(track[lo:hi].astype(np.float64))
    loud = np.flatnonzero(win >= 10.0 ** (VOICED_DBFS / 20.0))
    if not loud.size:
        return round(start, 3), round(end, 3), False
    first, last = (lo + int(loud[0])) / RATE, (lo + int(loud[-1]) + 1) / RATE
    t0 = max(start, math.floor(first * 1000.0) / 1000.0)
    t1 = min(end, math.ceil(last * 1000.0) / 1000.0)
    if t1 <= t0:  # a span shorter than the rounding grid: give it one VAD frame inside the clip
        t1 = min(end, round(t0 + FRAME / RATE, 3))
        t0 = max(start, round(t1 - FRAME / RATE, 3))
    return round(t0, 3), round(t1, 3), True


def write_wav(path: Path, x: np.ndarray) -> None:
    pcm = quantise(x)
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
        x = synth_turn(
            voices[speaker], turn["text"], float(turn.get("split_gap_s", DEFAULT_SPLIT_GAP_S))
        )
        start = cursor
        end = start + len(x) / RATE
        placed.append(
            Placed(
                index=i,
                speaker=speaker,
                start=round(start, 3),
                end=round(end, 3),
                text=spoken(turn["text"]),
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

    # Measured on the int16 samples that were just written, not the float mix, so turns.json
    # describes exactly what a consumer of the WAV sees.
    quantised = {role: quantise(x).astype(np.float64) / 32768.0 for role, x in tracks.items()}
    for p in placed:
        p.voiced_start, p.voiced_end, found = voiced_bounds(quantised[p.speaker], p.start, p.end)
        if not found:
            print(f"  warning: turn {p.index} ({p.speaker}) has no sample over {VOICED_DBFS} dBFS")

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
            "voiced_t": p.voiced_start,
            "voiced_end": p.voiced_end,
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
