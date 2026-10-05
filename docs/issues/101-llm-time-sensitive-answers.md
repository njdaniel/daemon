# #101 — llm: the prompts know today's date, and an answer that depends on what is current says so and names what to look up

Labels: `size:M, model:opus, m2, lane:llm`

Proposed 2026-10-05, from a live test (the ChatGPT desktop app asking). Asked "What is the latest stable major version of Go?", the fast phase answered with full confidence and wrongly: Approach "State Go 1.24 as the latest major version." / "Mention it was released in August 2025." (Go 1.24 came out in February 2025 and is several releases old by now). The slow phase built on it ("What are the key changes in Go 1.24?"). Found against `main` (79ab10a):

- No prompt carries a date. `crates/dmn-llm/prompts/*.md` and `prompt.rs` `user_message()` give the model the transcript and the candidate's material and nothing about when "now" is, so the model answers as of its training data and cannot know how old that is.
- Nothing asks the model to tell a stable fact ("how does a B-tree split") from one that goes stale ("the latest version of X", "the current LTS", "is Y still maintained").
- The fast schema (`schema.rs` `fast_schema_base()`) has no field for "this needs checking", so nothing downstream can act on it.

This issue makes the answer honest with no network. #102 uses the field added here to fetch the current fact.

## Goal
An answer that depends on what is current no longer states a remembered version or date as fact: the Approach says what the model knows and that it may be out of date, and the reply names what would need looking up.

## Size / Model / Turns
M | Opus | 60

## Scope
`crates/dmn-llm` (prompts, `prompt.rs`, `schema.rs`, `fast.rs`, `slow.rs`, `limits.rs`), `crates/dmn-session` and `apps/dmn-cli/src/tui/` for the mark, `apps/dmn-cli/src/suggest.rs` to pass the date, docs/SPEC.md § Context and LLM.

- **Decided**: the fast and slow user messages start with `## Today` and the date (`2026-10-05`), taken from the clock by the caller and passed in, so `dmn-llm` stays a pure function of its request and the tests pin a date.
- **Decided**: the fast schema gains `lookup`, a required string: empty when the answer does not depend on what is current, otherwise a web search query of at most 80 characters that would settle it ("latest stable Go release"). The limit is enforced in `limits.rs`. The query is written from the question, never copied from the transcript.
- The base prompts get one rule: when the answer depends on the current state of the world (latest version, current release, newest standard, who maintains something now), do not state a remembered version or date as fact; say what you know, say it may be out of date, and fill `lookup`.
- **Decided**: when `lookup` is not empty the Approach block shows a dim `may be out of date` tag (the place the `slow` tag uses), and the store keeps the query with the suggestion.
- The slow phase gets the same date and rule; its sections must not build on a version the fast phase flagged.

Out of scope: any network request (#102), a user-kept facts file, changing models, the M3 window.

## Depends on
#12, #55 (merged)

## Acceptance
- [ ] Every fast and slow request carries `## Today` with the date the caller passed; a prompt test pins it
- [ ] With MockProvider replies: a reply with a non-empty `lookup` shows the `may be out of date` tag (snapshot at 100 × 30 and 80 × 24) and stores the query; an empty `lookup` leaves every existing snapshot unchanged
- [ ] `lookup` longer than 80 characters or spanning lines is cut by `limits.rs` and flagged like any other limit
- [ ] Against the local model (`#[ignore]`, run by hand, result in the PR body): "What is the latest stable major version of Go?" returns a non-empty `lookup` and an Approach that does not state a version as current fact; "How does a B-tree split a full node?" returns an empty `lookup`
- [ ] `just eval`: recall, precision and p50 latency on the four fixtures no worse than `main` beyond run-to-run noise; the numbers are in the PR body
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-llm
cargo test -p dmn-session
cargo test -p dmn-cli tui
cargo test -p dmn-llm -- --ignored time_sensitive    # needs Ollama; by hand
just eval
```
