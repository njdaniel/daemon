# #102 — llm: a live web lookup corrects an answer that depends on what is current (`[lookup]`, off by default)

Labels: `size:M, model:opus, m2, lane:llm`

Proposed 2026-10-05, from the same live test as #101: asked for the latest Go version, Daemon said 1.24. #101 makes that answer honest ("may be out of date") and has the model name what to look up; this issue looks it up. The user chose a live web lookup during the call over a facts file refreshed beforehand or kept by hand, and put it this way: "I want to use local first but to search with web when necessary". That is the shape here: the local model answers every question first, and a search runs only for a question the model itself flags as depending on what is current.

What that choice costs, so the issue states it once:
- The search query leaves the machine. SPEC's promise is conditional, not absolute: "Transcript text leaves only when the user explicitly selects a remote model" (Principles) and "With `provider=ollama` nothing leaves the machine" (§ Privacy rules). With the lookup on and the local model, the second sentence is no longer true, so the lookup is a second explicit choice: off until `[lookup].enabled = true`, and the status line says when it is on.
- A search does not fit the 2 s budget for the first Approach line. The first answer still arrives on budget from the model alone, flagged by #101; the corrected one replaces it when the search returns.

## Goal
With `[lookup].enabled = true`, a question whose answer depends on what is current gets a second Approach built on web search results a few seconds after the first, with the source named.

## Size / Model / Turns
M | Opus | 60

## Scope
`crates/dmn-llm` (a search client, the request with facts), `apps/dmn-cli/src/suggest.rs` (running it), `crates/dmn-session` and `apps/dmn-cli/src/tui/` (state and tags), `crates/dmn-core/src/config.rs`, docs/SPEC.md § Context and LLM and § Privacy rules, README, docs/DEPENDENCIES.md only if a crate is added (the existing HTTP client should do).

- **Decided**: the search goes to Ollama's hosted web search, `POST https://ollama.com/api/web_search` with `{"query", "max_results"}` and a bearer key, behind a small trait so another provider is one more implementation. Config: `[lookup] enabled = false`, `provider = "ollama"`, `api_key_env = "OLLAMA_API_KEY"` (the name of the variable, never the key), `timeout_ms = 3000`, `max_results = 3`. `dmn doctor` reports the lookup as off, ready, or on with the variable unset.
- **Decided** 2026-10-05, on PR #104: a second provider, the user's own SearXNG. The owner's words: "for weblookup can we do self web search with searxeg?" (local first; SearXNG runs on their machine and needs no account or key). `[lookup].provider = "ollama" | "searxng"` (`ollama` stays the default value; the lookup stays off by default). For `searxng`: `GET <base_url>/search?q=<query>&format=json`, no Authorization header, `api_key_env` ignored; `base_url` unset or still Ollama's default means `http://localhost:8888`, and `timeout_ms` left at 3000 means 6000 (a stock SearXNG waits up to 3 s on slow engines itself). A 403 (JSON format off in a stock instance) is reported as "add `json` to `search.formats` in its settings.yml". `dmn doctor` probes the instance once when the lookup is on and warns on unreachable or JSON disabled. The query goes to the user's instance, which forwards it to the engines it is configured with.
- When the fast phase returns a non-empty `lookup` (#101) and the lookup is on, the suggester sends the query while the first answer is on screen. Only that query is sent: no transcript, no candidate material.
- **Decided**: when results arrive, the fast phase runs once more for the same question with a `## Fresh facts` section (each result's title, a snippet cut to 300 characters, and its domain) and the rule "prefer these to what you remember; if they do not answer the question, say so". Its reply replaces the question's Approach in place, as a regenerate does, and the `may be out of date` tag becomes `checked · <domain>`. The slow phase for that question starts after it and gets the same section.
- **Decided**: the search and the second request obey the existing rules: a newer unpinned question cancels both; a pin does not. On a timeout, an error, no results or an unset key, the first answer and its tag stay and the status line says `lookup failed` for 5 s; nothing is retried.
- While the lookup is on, the status line carries `web` next to the model dot. SPEC's Principles line and § Privacy rules are reworded to say what leaves the machine and when: audio never; transcript text only with a remote model; a search query only with the lookup on.

Out of scope: fetching and reading result pages (`web_fetch`), lookups for questions the model did not flag, a lookup key, caching results between sessions, other search providers, the M3 window.

## Depends on
#101 (merged), #59 (merged)

## Acceptance
- [ ] Off (the default): no request leaves for a flagged question; `dmn config show` prints the `[lookup]` keys; every existing snapshot is unchanged
- [ ] On, against a local test server (the pattern of `crates/dmn-llm/src/testserver.rs`): a flagged question sends exactly one search request whose body holds the query and nothing from the transcript; the second fast request carries `## Fresh facts`; the Approach is replaced in place and tagged `checked · go.dev` (snapshots at 100 × 30 and 80 × 24)
- [ ] A search that times out, errors, returns nothing, or has no key leaves the first answer with `may be out of date`, shows `lookup failed` for 5 s, and sends no second fast request
- [ ] A newer question arriving during the search or the second request cancels them; while pinned they complete
- [ ] `dmn doctor` reports the three lookup states; the status line shows `web` only while it is on
- [ ] By hand, with a real key, result in the PR body: "What is the latest stable major version of Go?" ends on the version go.dev lists that day, with the time from question to corrected Approach
- [ ] docs/SPEC.md § Privacy rules and the README say what is sent, to whom, and that it is off by default
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-llm lookup
cargo test -p dmn-cli suggest
cargo test -p dmn-cli tui
cargo test -p dmn-core config
# by hand: export OLLAMA_API_KEY=…, set [lookup] enabled = true, then ask for the latest Go version
dmn run
```
