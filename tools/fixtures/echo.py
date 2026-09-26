#!/usr/bin/env python3
"""Write the echo fixture testdata/echo/ from an existing fixture (#60).

The mic of a laptop without echo cancellation, or of a room where the interviewer's voice comes
out of a speaker, hears the call as well as the candidate. This fixture reproduces that: the
interviewer track is copied as is, and the candidate track becomes the candidate's own speech plus
the interviewer's, attenuated and delayed:

    candidate.wav = candidate + GAIN × interviewer(t − DELAY)

Everything else (questions.json, turns.json, script.md) is copied, so the fixture scores like its
source. The source WAVs come from the fixtures-v1 release (`just fixtures-fetch`), and this output
is derived deterministically from them, so it needs no release of its own; the WAVs stay out of git
like every fixture WAV. Stdlib only.

    python3 tools/fixtures/echo.py                         # from sysdesign, -9 dB, 100 ms
    python3 tools/fixtures/echo.py --source mixed --gain-db -12 --delay-ms 150
"""

from __future__ import annotations

import argparse
import array
import shutil
import sys
import wave
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
TESTDATA = ROOT / "testdata"
RATE = 16_000


def read(path: Path) -> array.array:
    with wave.open(str(path), "rb") as w:
        if (w.getframerate(), w.getnchannels(), w.getsampwidth()) != (RATE, 1, 2):
            sys.exit(f"{path}: expected 16 kHz mono 16-bit, got "
                     f"{w.getframerate()} Hz, {w.getnchannels()} ch, {8 * w.getsampwidth()} bit")
        samples = array.array("h")
        samples.frombytes(w.readframes(w.getnframes()))
    if sys.byteorder == "big":
        samples.byteswap()
    return samples


def write(path: Path, samples: array.array) -> None:
    data = array.array("h", samples)
    if sys.byteorder == "big":
        data.byteswap()
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(data.tobytes())


def mix(candidate: array.array, interviewer: array.array, gain: float, delay: int) -> array.array:
    out = array.array("h", candidate)
    for i in range(delay, len(out)):
        j = i - delay
        if j >= len(interviewer):
            break
        v = out[i] + gain * interviewer[j]
        out[i] = max(-32768, min(32767, int(round(v))))
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--source", default="sysdesign", help="fixture to derive from (default sysdesign)")
    p.add_argument("--gain-db", type=float, default=-9.0, help="echo level relative to the interviewer")
    p.add_argument("--delay-ms", type=int, default=100, help="echo delay behind the interviewer")
    p.add_argument("--out", default="echo", help="fixture name to write (default echo)")
    args = p.parse_args()

    src, dst = TESTDATA / args.source, TESTDATA / args.out
    for name in ("interviewer.wav", "candidate.wav"):
        if not (src / name).is_file():
            sys.exit(f"{src / name} is missing: run `just fixtures-fetch` first")
    dst.mkdir(parents=True, exist_ok=True)
    interviewer = read(src / "interviewer.wav")
    candidate = read(src / "candidate.wav")
    gain = 10 ** (args.gain_db / 20)
    delay = args.delay_ms * RATE // 1000
    shutil.copyfile(src / "interviewer.wav", dst / "interviewer.wav")
    write(dst / "candidate.wav", mix(candidate, interviewer, gain, delay))
    for name in ("questions.json", "turns.json", "script.md"):
        if (src / name).is_file():
            shutil.copyfile(src / name, dst / name)
    print(f"wrote {dst}/: candidate = {args.source} candidate + interviewer at {args.gain_db:+.1f} dB, "
          f"{args.delay_ms} ms late")
    return 0


if __name__ == "__main__":
    sys.exit(main())
