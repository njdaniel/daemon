# tools/

Offline developer tooling, run with `uv` from this directory. Nothing here is on the audio path or shipped in the `dmn` binary.

| Path | What | Run |
| --- | --- | --- |
| `fixtures/scripts/*.json` | The three mock-interview scripts. Each turn has `speaker`, `text`, `is_question`, `kind` and `pause_after_s`. | edit, then `just fixtures` |
| `fixtures/generate.py` | Synthesises each script with Piper into `testdata/<name>/`: WAVs, `questions.json`, `turns.json`, `script.md` | `just fixtures [name…]` |
| `fixtures/verify.py` | Checks format, equal lengths, 7–14 min duration, no overlaps, speech RMS -25..-15 dBFS, no crosstalk, `questions.json` consistency | runs after `just fixtures` |
| `fixtures/echo.py` | Derives `testdata/echo/` from a fetched fixture: candidate + interviewer at -9 dB, 100 ms late, for the echo guard (#60). Stdlib only | `python3 tools/fixtures/echo.py` |

Voices download on first use into `~/.cache/dmn/piper/`. See `testdata/README.md` for voice licences and the release tarball.
