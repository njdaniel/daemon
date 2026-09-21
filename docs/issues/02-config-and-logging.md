# #2 — infra: config TOML loader, XDG paths, `dmn doctor --init`, `tracing` JSON log

Labels: `size:S, model:sonnet, m0, lane:infra`

## Goal
Load `~/.config/dmn/config.toml` into typed structs with the defaults from docs/SPEC.md § Data, config and privacy; `dmn doctor --init` writes a commented config; structured JSON logs go to `~/.local/state/dmn/dmn.log`.
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
New crate-level module `config` (in `dmn-core` or a small `dmn-config` module inside it), `apps/dmn-cli` (`doctor --init`, `config show`), `directories`, `serde`, `toml`, `tracing`, `tracing-subscriber` (add to docs/DEPENDENCIES.md). API keys are read from the env var named in `api_key_env`, never stored.
Out of scope: everything else, including later issues.
## Depends on
#1
## Acceptance
- [ ] Every key in the SPEC config example round-trips through serde with the SPEC defaults
- [ ] `dmn doctor --init` creates the file with comments and refuses to overwrite without `--force`
- [ ] Log file rotates at 10 MB; log lines are JSON and never contain the API key
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- doctor --init && cat ~/.config/dmn/config.toml
cargo run -p dmn-cli -- config show   # prints effective config as TOML
cargo test -p dmn-core config
```
