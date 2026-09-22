"""Check the generated fixtures in testdata/ without playing anything.

For each fixture directory with a turns.json:
  * interviewer.wav and candidate.wav exist, are 16 kHz mono 16-bit, and have identical length
  * total duration is between 7 and 14 minutes (the spec asks for about 10)
  * turns are in time order and never overlap
  * each turn's active-speech RMS on its own track is between -25 and -15 dBFS
  * the other track stays at the noise floor under every turn (speaker separation holds)
  * questions.json is exactly the is_question turns of turns.json

Exit status 0 when every check passes, 1 otherwise.
Usage: cd tools && uv run python fixtures/verify.py [names...]
"""

from __future__ import annotations

import json
import sys
import wave
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from generate import FRAME, RATE, ROOT, active_rms_dbfs, dbfs  # noqa: E402

MIN_S, MAX_S = 7 * 60, 14 * 60
RMS_LO, RMS_HI = -25.0, -15.0
CROSSTALK_MAX_DBFS = -50.0


def read_wav(path: Path) -> tuple[np.ndarray, dict]:
    with wave.open(str(path), "rb") as w:
        meta = {"rate": w.getframerate(), "channels": w.getnchannels(), "width": w.getsampwidth()}
        pcm = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
    return pcm.astype(np.float32) / 32768.0, meta


def check(fixture: Path) -> list[str]:
    errors: list[str] = []
    tracks: dict[str, np.ndarray] = {}
    for role in ("interviewer", "candidate"):
        path = fixture / f"{role}.wav"
        if not path.exists():
            return [f"missing {path.relative_to(ROOT)} (run `just fixtures` or `just fixtures-fetch`)"]
        x, meta = read_wav(path)
        if meta != {"rate": RATE, "channels": 1, "width": 2}:
            errors.append(f"{role}.wav is {meta}, want 16 kHz mono 16-bit")
        tracks[role] = x
    if len(tracks["interviewer"]) != len(tracks["candidate"]):
        errors.append(
            f"length mismatch: interviewer {len(tracks['interviewer'])} vs candidate {len(tracks['candidate'])} samples"
        )
    duration = len(tracks["interviewer"]) / RATE
    if not MIN_S <= duration <= MAX_S:
        errors.append(f"duration {duration:.1f} s outside {MIN_S}-{MAX_S} s")

    turns = json.loads((fixture / "turns.json").read_text())
    prev_end = 0.0
    worst_lo, worst_hi, worst_x = 0.0, -120.0, -120.0
    for t in turns:
        if t["t"] < prev_end:
            errors.append(f"turn {t['i']} starts at {t['t']} before previous end {prev_end}")
        if t["end"] <= t["t"]:
            errors.append(f"turn {t['i']} has non-positive length")
        prev_end = t["end"]
        a, b = int(t["t"] * RATE), int(t["end"] * RATE)
        own = tracks[t["speaker"]][a:b]
        other = tracks["candidate" if t["speaker"] == "interviewer" else "interviewer"][a:b]
        level = active_rms_dbfs(own)
        if not RMS_LO <= level <= RMS_HI:
            errors.append(f"turn {t['i']} ({t['speaker']}) speech RMS {level:.1f} dBFS outside {RMS_LO}..{RMS_HI}")
        worst_lo, worst_hi = min(worst_lo, level), max(worst_hi, level)
        cross = dbfs(other)
        worst_x = max(worst_x, cross)
        if cross > CROSSTALK_MAX_DBFS:
            errors.append(f"turn {t['i']}: other track at {cross:.1f} dBFS during this turn (crosstalk)")
        if b > len(own) + a:
            errors.append(f"turn {t['i']} runs past the end of the audio")

    questions = json.loads((fixture / "questions.json").read_text())
    expected = [
        {"t": t["t"], "end": t["end"], "text": t["text"], "kind": t["kind"]} for t in turns if t["is_question"]
    ]
    if questions != expected:
        errors.append("questions.json does not match the is_question turns in turns.json")

    n_frames = len(tracks["interviewer"]) // FRAME
    print(
        f"{fixture.name:<11} {duration / 60:5.2f} min  {len(turns):2d} turns  {len(questions):2d} questions  "
        f"speech RMS {worst_lo:.1f}..{worst_hi:.1f} dBFS  worst crosstalk {worst_x:.1f} dBFS  "
        f"{n_frames} frames  {'OK' if not errors else 'FAIL'}"
    )
    return errors


def main() -> int:
    wanted = set(sys.argv[1:])
    fixtures = sorted(p.parent for p in (ROOT / "testdata").glob("*/turns.json"))
    if wanted:
        fixtures = [f for f in fixtures if f.name in wanted]
    if not fixtures:
        print("no fixtures found under testdata/", file=sys.stderr)
        return 1
    failed = False
    for f in fixtures:
        for e in check(f):
            failed = True
            print(f"  {f.name}: {e}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
