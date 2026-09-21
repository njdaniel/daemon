# #1 — infra: workspace scaffold, every crate an empty lib with its `lib.rs` doc comment, `dmn --version`, `justfile`, CI green

Labels: `size:M, model:opus, m0, lane:infra`

## Goal
Every crate exists as an empty lib with a `lib.rs` doc comment stating input, output and owned channel; `dmn --version` runs; `justfile` and CI are green.
## Size / Model / Turns
M | Opus, interactive | —   (from docs/PROCESS.md; labels must match)
## Scope
`Cargo.toml` workspace, `crates/*`, `apps/dmn-cli`, `justfile`, `rust-toolchain.toml`, `.github/workflows/ci.yml`, `CLAUDE.md`, `docs/DEPENDENCIES.md`.
Out of scope: everything else, including later issues.
## Depends on
—
## Acceptance
- [ ] Ten crates listed in docs/SPEC.md § Architecture compile as empty libs
- [ ] `dmn --version` prints the workspace version
- [ ] CI runs fmt, check, clippy, test on Ubuntu 24.04
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
just check && just lint && just test
cargo run -p dmn-cli -- --version   # dmn 0.1.0
```
