# #6 — infra: `dmn doctor` full checks (libpipewire, model file, Ollama, GPU backend) + CI system deps and cached whisper.cpp build

Labels: `size:S, model:sonnet, m0, lane:infra`

## Goal
`dmn doctor` checks and prints a fix for each of: libpipewire headers, PipeWire server and version, whisper model file, Ollama reachability and model, GPU backend; CI installs system deps and caches the whisper.cpp build.
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
`apps/dmn-cli` `doctor`; `.github/workflows/ci.yml`. Each check is a small function returning `Check { name, ok, detail, fix }`; output is one line per check, exit code 1 if any required check fails.
Out of scope: everything else, including later issues.
## Depends on
#2
## Acceptance
- [ ] `dmn doctor` passes on the dev machine once system deps are installed
- [ ] Missing Ollama or model prints the exact command to fix it
- [ ] Doctor prints the LLM provider in use and whether transcript text leaves the machine
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- doctor
```
