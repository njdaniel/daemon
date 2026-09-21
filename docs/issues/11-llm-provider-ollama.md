# #11 — dmn-llm: `Provider` trait, Ollama client (NDJSON streaming, JSON-schema `format`), `MockProvider`

Labels: `size:M, model:opus, m1, lane:llm`

## Goal
Define the async streaming `Provider` trait and implement it for Ollama's `/api/chat` with the JSON schema passed in `format`, plus a `MockProvider` that replays canned responses for tests.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-llm`: `Provider` trait, `ollama` module, `MockProvider`; `reqwest`, `tokio`, `tokio-util` (cancellation), `serde_json`. Warm-up request and `keep_alive` on start. Send `think: false` on every request (qwen3.6 thinks by default and the thinking tokens alone exceed the fast-phase budget; measured 2026-09-21). No prompts yet.
Out of scope: everything else, including later issues.
## Depends on
#2
## Acceptance
- [ ] Streaming NDJSON is parsed incrementally and cancelled cleanly via `CancellationToken`
- [ ] Ollama unreachable surfaces as a typed error, not a panic
- [ ] Requests carry `think: false`; a test asserts the field is present in the request body
- [ ] Tests run against `MockProvider` only; an `#[ignore]` test hits the local Ollama
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-llm
cargo test -p dmn-llm -- --ignored   # dev machine with Ollama running
```
