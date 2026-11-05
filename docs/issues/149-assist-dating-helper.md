# #149 — assist: a dating helper — conversation advice on a call or video date

Labels: `size:M, model:opus, m4, lane:llm, backlog`

Proposed 2026-11-05 by the owner: "make an issue for the general, dming, dating advice modes". The general helper is #148 and the D&D helper is #140. This one is for a voice or video date (Discord, Telegram, a dating app's call on the desktop): help keep the conversation flowing and remember what the other person said, at a glance, without a wall of text.

## Goal
`[assist].helper = "dating"` (and `--helper dating`): Daemon follows the call and, at natural moments, shows a short read of the conversation and a few follow-ups built on what the other person actually said, plus the details worth remembering (names, places, interests they mentioned).

## Size / Model / Turns
M | Opus | 45 (docs/PROCESS.md). Builds on #148's output shape and helper wiring; a new prompt set, trigger rules and one extra field.

## Scope
`crates/dmn-core/src/config.rs` and template, `crates/dmn-detect` (triggers), `crates/dmn-llm` (`prompts/dating*.md`, `schema.rs`, `limits.rs`), `crates/dmn-session` (the remembered details), `apps/dmn-cli` (rendering, status), README, SPEC.

### When it answers
- A question to the user (answering it is theirs; the helper may suggest a follow-up question back), a lull (the other side has not spoken for a configurable few seconds after the user's turn ends), a topic that ran out, and Enter as today. Not on every line: a request at most every N seconds unless Enter is pressed.
- The user's own turns are context, not triggers.

### What it shows
- `moment` (one line: where the conversation is), `ask` (2–3 follow-up questions or remarks that build on something they said, quoting the hook briefly), `remember` (a running list, capped, of details they shared: names, places, plans, likes; kept in session state and shown in a section, not re-derived on every request).
- Optional `note` (0–1 line), only when useful: they asked something the user has not answered yet; the user has been talking for a long stretch, ask about them; they mentioned something earlier worth returning to.

### Guardrails (in the prompt and in tests)
- The advice aims at an honest, mutual conversation: be curious, listen, be yourself. No manipulation or pickup tactics (negging, pressure, false urgency, fake claims), no suggesting the user misrepresent themselves, no scripts to push past a "no".
- When the other person signals disinterest, discomfort or a boundary ("I'd rather not talk about that", wants to end the call), the advice is to respect it: change topic or wrap up kindly.
- No ratings or judgements of the other person's looks, worth or "interest score"; no guessing at private traits. Sexual content is not suggested.
- It does not speak into the call, and nothing changes in `[speak]`: the user says what they choose.

### Privacy
- The other person did not agree to software listening. README states this plainly: everything stays on this machine with a local model; with a remote provider their words leave it. The doctor/status warns once when the dating helper runs with a remote provider.
- Transcripts are saved as in every helper (#136) unless `--no-store`. Open choice for the PR: whether the dating helper defaults to not storing (`[assist].dating_store = false`) — decide with the owner.
- Audio is never written unless `privacy.record_audio` is true, as everywhere.

Out of scope: profile or message writing for dating apps, analysing photos, text chat, speaking suggestions aloud, and storing anything about the other person beyond the session.

## Depends on
#148 (helper selection and the general output shape). #140 and #148 coordinate on `[assist].helper`.

## Acceptance
- [ ] `[assist].helper = "dating"` and `--helper dating` parse; other helpers unchanged (`cargo test -p dmn-core config`)
- [ ] Transcript fixtures: a question to the user, a lull, a topic running out, small talk mid-flow, a boundary ("I don't really want to talk about my ex"), and Enter; the rate limit holds between automatic requests (`cargo test -p dmn-detect`)
- [ ] MockProvider tests: dating requests carry the dating prompt and the remembered details, no interview or general instructions (`cargo test -p dmn-llm`)
- [ ] Prompt tests pin the guardrails text (no manipulation tactics, respect boundaries, no ratings); a boundary fixture's request includes the instruction to respect it (`cargo test -p dmn-llm dating`)
- [ ] `moment` / `ask` / `remember` / `note` validated and capped in `limits.rs`; `remember` deduplicated and capped across the session; rendered with dating labels; snapshots at 80×24, 100×30, 140×30
- [ ] A remote provider with the dating helper warns once (status line and log)
- [ ] One local-model replay of a date-conversation fixture in the PR for a qualitative read, including the boundary case
- [ ] README and SPEC: the helper, its guardrails, the privacy note
- [ ] `just check`, `just test` and `just lint` pass

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-detect
cargo test -p dmn-llm dating
cargo test -p dmn-cli dating
just check
just test
just lint
```
CI uses synthetic transcripts and MockProvider; no microphone, PipeWire or live model.
