# #54 — dmn-context: BM25 retrieval of the top 6 chunks (≤ 1,500 tokens) and a job-description summary of ≤ 8 bullets, fed into the fast phase

Labels: `size:M, model:opus, m2, lane:infra`

## Goal
Retrieve the best chunks per question with pure-Rust BM25 over the stored chunks (question + focus as the query), and summarise the job description once at load into ≤ 8 requirement bullets that every prompt carries (SPEC § Context and LLM).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-context`: a BM25 index built from the `chunks` table at session start (rebuilt when sources change); top 6 chunks, capped at 1,500 tokens.
- JD summary: one LLM call at load through the existing `dmn_llm::Provider` (MockProvider in tests), cached in SQLite by JD content hash, ≤ 8 bullets, limits in `crates/dmn-llm/src/limits.rs`.
- Wiring: `dmn_llm::FastRequest` already has `job_requirements` and `chunks` (empty today); fill them per candidate in `dmn-core` / `apps/dmn-cli/src/suggest.rs`. Retrieval must fit the 2.0 s budget: measure it and keep it in the tens of ms.
- `dmn replay --json` records which chunks went with each candidate, so eval and review can see them.
Out of scope: everything else, including later issues.
## Depends on
#53
## Acceptance
- [ ] On the context fixtures, a question about a skill in the resume retrieves a chunk containing it in the top 3 (a test over several scripted questions)
- [ ] The JD summary has ≤ 8 bullets, is computed once per JD content and reused from the cache, and appears in every fast-phase prompt (asserted on MockProvider's recorded requests)
- [ ] Retrieval adds ≤ 50 ms per candidate (`dmn replay` summary line); fixture p50 question-end → Approach still ≤ 2.0 s at 1× (`dmn run --replay … --headless`)
- [ ] With no sources loaded, prompts are the same as in M1
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-context -p dmn-llm
cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav --speed 8 --json | grep -c '"chunks"'
```
