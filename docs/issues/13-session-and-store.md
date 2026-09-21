# #13 — dmn-session + dmn-store: state reducers, `watch`/broadcast, SQLite schema + migrations; replay writes a session log

Labels: `size:M, model:opus, m1, lane:infra`

## Goal
Hold all live state in one `Session` behind `tokio::sync::watch` with pure reducers, and persist sessions, segments, questions and suggestions to SQLite with a migration runner.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-session` (state, actions, reducers), `crates/dmn-store` (`rusqlite` bundled, migrations, JSONL export), `dmn-core` wiring. `dmn sessions purge`.
Out of scope: everything else, including later issues.
## Depends on
#12
## Acceptance
- [ ] Reducers are deterministic: a recorded action log replays to an identical `Session`
- [ ] `dmn replay` writes the same session rows a live call would
- [ ] Purge removes rows and files; API keys never appear in the database
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-session -p dmn-store
cargo run -p dmn-cli -- replay ... && sqlite3 ~/.local/share/dmn/dmn.sqlite 'select count(*) from questions'
```
