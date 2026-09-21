# #4 — dmn-audio: PipeWire main-loop thread, registry listener, live node table, `dmn devices`

Labels: `size:L, model:fable, m0, lane:audio`

## Goal
Run the PipeWire main loop on its own thread and keep a live table of capture sources (`Audio/Source`) and app output streams (`Stream/Output/Audio`) from registry events, exposed by `dmn devices`.
## Size / Model / Turns
L | Fable | 60   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-audio`: `pipewire` crate 0.10 (docs/DEPENDENCIES.md), `PipeWireContext` owning the loop thread, `Registry::add_listener` for global-added / global-removed, node table behind a `watch`; `apps/dmn-cli` `devices` subcommand. Tests that need a server are `#[ignore]`.
Out of scope: everything else, including later issues.
## Depends on
#3
## Acceptance
- [ ] `dmn devices` lists mics with `node.description` and app streams as `application.name — media.name`
- [ ] Starting or stopping an app that plays audio updates the table without polling
- [ ] CI (no PipeWire) still builds and passes; server tests run under `just probe`
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- devices
just probe   # on the dev machine, with PipeWire running
```
