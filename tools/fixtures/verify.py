"""Check the generated fixtures in testdata/ without playing anything.

For each fixture directory with a turns.json:
  * interviewer.wav and candidate.wav exist, are 16 kHz mono 16-bit, and have identical length
  * total duration is between 7 and 14 minutes (the spec asks for about 10)
  * turns are in time order, never overlap, and stay inside the audio
  * each turn's active-speech RMS on its own track is between -25 and -15 dBFS
  * the other track stays at the noise floor under every turn (speaker separation holds)
  * every turn carries voiced bounds with t <= voiced_t < voiced_end <= end, and they still match
    what the audio says, so JSON left stale against regenerated WAVs is caught
  * questions.json is exactly the is_question turns of turns.json

Exit status 0 when every check passes, 1 otherwise.
Usage: cd tools && uv run python fixtures/verify.py [names...]
       cd tools && uv run python fixtures/verify.py --self-test   # checks the checks
"""

from __future__ import annotations

import json
import sys
import wave
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from generate import (  # noqa: E402
    FRAME,
    RATE,
    ROOT,
    active_rms_dbfs,
    dbfs,
    voiced_bounds,
    write_wav,
)

MIN_S, MAX_S = 7 * 60, 14 * 60
RMS_LO, RMS_HI = -25.0, -15.0
CROSSTALK_MAX_DBFS = -50.0
VOICED_TOL_S = 0.002  # the recorded bounds are rounded to 3 decimals


def read_wav(path: Path) -> tuple[np.ndarray, dict]:
    with wave.open(str(path), "rb") as w:
        meta = {"rate": w.getframerate(), "channels": w.getnchannels(), "width": w.getsampwidth()}
        pcm = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2")
    return pcm.astype(np.float32) / 32768.0, meta


def check(fixture: Path, quiet: bool = False) -> list[str]:
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
        # Voiced bounds (issue #28): where sound is inside the clip window, the reference the VAD
        # accuracy tests score against. A clip's edges are not speech boundaries.
        if "voiced_t" not in t or "voiced_end" not in t:
            errors.append(f"turn {t['i']} has no voiced_t / voiced_end (regenerate with `just fixtures`)")
        elif not t["t"] <= t["voiced_t"] < t["voiced_end"] <= t["end"]:
            errors.append(
                f"turn {t['i']} voiced bounds {t['voiced_t']}..{t['voiced_end']} "
                f"outside the clip {t['t']}..{t['end']}"
            )
        else:
            want_t, want_end = voiced_bounds(tracks[t["speaker"]], t["t"], t["end"])
            if (
                abs(want_t - t["voiced_t"]) > VOICED_TOL_S
                or abs(want_end - t["voiced_end"]) > VOICED_TOL_S
            ):
                errors.append(
                    f"turn {t['i']} voiced bounds {t['voiced_t']}..{t['voiced_end']} do not match "
                    f"the audio ({want_t}..{want_end}); regenerate with `just fixtures`"
                )
        a, b = int(t["t"] * RATE), int(t["end"] * RATE)
        if b > len(tracks[t["speaker"]]):
            errors.append(
                f"turn {t['i']} ends at {t['end']:.3f} s, past the "
                f"{len(tracks[t['speaker']]) / RATE:.3f} s of {t['speaker']}.wav"
            )
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

    questions = json.loads((fixture / "questions.json").read_text())
    expected = [
        {"t": t["t"], "end": t["end"], "text": t["text"], "kind": t["kind"]} for t in turns if t["is_question"]
    ]
    if questions != expected:
        errors.append("questions.json does not match the is_question turns in turns.json")

    n_frames = len(tracks["interviewer"]) // FRAME
    if quiet:
        return errors
    print(
        f"{fixture.name:<11} {duration / 60:5.2f} min  {len(turns):2d} turns  {len(questions):2d} questions  "
        f"speech RMS {worst_lo:.1f}..{worst_hi:.1f} dBFS  worst crosstalk {worst_x:.1f} dBFS  "
        f"{n_frames} frames  {'OK' if not errors else 'FAIL'}"
    )
    return errors


def self_test() -> int:
    """Check the checks: build a small fixture in a temp dir and confirm each fault is reported.

    Guards issue #30 item 9: the out-of-range test used to compare `b` against `len(own) + a`,
    which is `b` again whenever Python truncated the slice, so a turn past the end of the WAV was
    never reported.
    """
    import tempfile

    def build(root: Path, turns: list[dict], seconds: float = 6.0) -> Path:
        fixture = root / "selftest"
        fixture.mkdir(parents=True, exist_ok=True)
        n = int(seconds * RATE)
        rng = np.random.default_rng(7)
        floor = 10.0 ** (-60.0 / 20.0)
        tone = 0.1 * np.sin(2.0 * np.pi * 220.0 * np.arange(n) / RATE).astype(np.float32)
        for role in ("interviewer", "candidate"):
            track = (rng.standard_normal(n) * floor).astype(np.float32)
            if role == "interviewer":
                track[RATE : 3 * RATE] += tone[RATE : 3 * RATE]
            write_wav(fixture / f"{role}.wav", track)
        (fixture / "turns.json").write_text(json.dumps(turns, indent=2) + "\n")
        (fixture / "questions.json").write_text(
            json.dumps(
                [{"t": t["t"], "end": t["end"], "text": t["text"], "kind": t["kind"]} for t in turns if t["is_question"]],
                indent=2,
            )
            + "\n"
        )
        return fixture

    def turn(**over) -> dict:
        base = {
            "i": 0,
            "speaker": "interviewer",
            "t": 1.0,
            "end": 3.0,
            "voiced_t": 1.0,
            "voiced_end": 3.0,
            "is_question": False,
            "kind": None,
            "text": "tone",
        }
        base.update(over)
        return base

    missing = turn()
    del missing["voiced_t"], missing["voiced_end"]
    cases = [
        ("clean", [turn()], None),
        ("turn past the end of the WAV", [turn(t=5.0, end=9.0, voiced_t=5.0, voiced_end=9.0)], "past the"),
        ("voiced end outside the clip", [turn(voiced_end=3.5)], "outside the clip"),
        ("voiced start before the clip", [turn(voiced_t=0.5)], "outside the clip"),
        ("voiced bounds stale against the audio", [turn(voiced_t=1.5)], "do not match the audio"),
        ("voiced bounds missing", [missing], "no voiced_t"),
    ]

    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        for name, turns, expect in cases:
            errors = check(build(Path(tmp), turns), quiet=True)
            hit = [e for e in errors if expect and expect in e]
            if expect is None:
                bad = [e for e in errors if "past the" in e or "voiced" in e]
                ok = not bad
                detail = f"unexpected: {bad}" if bad else "no boundary or voiced errors"
            else:
                ok = bool(hit)
                detail = hit[0] if hit else f"NOT REPORTED; got {errors}"
            failures += 0 if ok else 1
            print(f"  self-test {name:<32} {'OK  ' if ok else 'FAIL'} {detail}")
    print(f"self-test: {'all checks catch their fault' if not failures else f'{failures} broken check(s)'}")
    return 1 if failures else 0


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        return self_test()
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
