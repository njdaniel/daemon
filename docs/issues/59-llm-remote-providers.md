# #59 — dmn-llm: OpenAI-compatible and Anthropic providers behind `Provider`; provider switch by config reload without restart

Labels: `size:M, model:opus, m2, lane:llm`

## Goal
Add the OpenAI-compatible (`response_format` json_schema) and Anthropic (tool use with the schema) providers, streaming, behind the existing `Provider` trait, and switch providers by reloading the config without restarting `dmn run` (SPEC § Context and LLM, Providers; M2 done-criteria).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-llm`: `openai.rs` (covers OpenRouter, vLLM, LM Studio, OpenAI) and `anthropic.rs`, both streaming into the incremental JSON parser the fast phase uses; tests against recorded SSE bodies on a local mock server, no network in CI.
- API keys only via `[llm].api_key_env` (the env var's name); never logged or stored (#13's database-scan test extends to both providers).
- `dmn config`: it has `show` today; add a reload path — `dmn run` watches the config file and swaps the provider between questions, never mid-request.
- Privacy: when the provider is not `ollama`, `dmn run` and `dmn config show` say in words that transcript text leaves the machine (SPEC § Providers), and the model dot names the provider.
- Two remote APIs in one issue is a judgement call: split into one issue per provider if the first run does not finish.
Out of scope: everything else, including later issues.
## Depends on
#12, #15
## Acceptance
- [ ] Both providers pass the fast-phase stream tests against recorded responses: question before the Approach, lines one by one, limits applied
- [ ] Changing `[llm].provider` while `dmn run --replay` runs takes effect on the next question without a restart (test with two configured mock providers)
- [ ] The API key never appears in logs, the database or exports
- [ ] Non-Ollama providers show the "transcript leaves this machine" notice
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-llm -p dmn-core
cargo run -p dmn-cli -- config show
```
