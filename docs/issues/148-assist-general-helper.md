# #148 — assist: a general advice helper for everyday calls, beside the interview helper

Labels: `size:M, model:opus, m4, lane:llm, backlog`

Proposed 2026-11-05 by the owner, after the README was reframed (PR #147) around live calls rather than interviews only: "make an issue for the general, dming, dating advice modes". Daemon is now used mostly on ordinary Discord and Telegram calls, often translated, where nobody is interviewing anyone. With assistance on, every request still carries interview framing (question kinds coding / system design / behavioral, STAR stories, the resume and job description). The D&D helper is #140 (DM-facing, per the owner); dating advice is #149.

## Goal
`[assist].helper = "general"` (and `dmn run --helper general`): when something in the conversation calls for a response from the user (a question to them, a decision, a request for an opinion or for help), Daemon shows a short read of the moment and a few things the user could say or do, at a glance, in the same place the interview Approach goes today.

## Size / Model / Turns
M | Opus | 45 (docs/PROCESS.md). Same provider, two-phase pipeline, worker, limits and TUI; a new prompt set, output shape and detector routing.

## Scope
`crates/dmn-core/src/config.rs` and its template, `apps/dmn-cli` (flag, status, rendering), `crates/dmn-detect` (what triggers a request in this helper), `crates/dmn-llm` (`prompt.rs`, `prompts/general*.md`, `schema.rs`, `limits.rs`), `crates/dmn-session` (helper-scoped state), README, SPEC.

### Selecting the helper
- `[assist].helper` and `--helper` as proposed in #140. Whichever of #140, #148, #149 merges first adds the setting with `interview` (the default, unchanged behaviour) and its own value; the others add theirs. An unknown value is a config error that names the choices.
- Independent of `[assist].enabled`, Ctrl+Shift+A and `--no-assist`: off means no request in any helper. Transcription, translation, speech and storage are unchanged by the helper.
- The status line shows the active helper when it is not `interview` (e.g. `general`).

### When it answers
- Triggers: a question or request aimed at the user from the asking lane (`[detect].question_source`, Them by default), a decision point ("what do you think?", "should we…", "can you…"), and Enter on the last turn as today. Small talk, the user's own long turns and statements that need no reply do not replace what is on screen (reuse #93's small-talk veto where it fits).
- With `question_source = "you"` the user can ask Daemon directly ("how do I say no politely?"), as today.

### What it shows
- Fields (validated JSON, capped in `limits.rs`): `moment` (one line: what is being asked or decided), `say` (2–4 short options the user could say, in the user's voice, not scripted speeches), `consider` (optional, 0–2 lines: a fact to check, a tradeoff, something they said earlier that matters). The slow phase may add one deeper section; reuse the section grid with general labels, no STAR / coding / system-design sections.
- Context: the recent transcript, session memory (#57) and any sources the user added (#53). No resume or job-description framing in the prompt; if the user has interview sources loaded they are not used in this helper unless explicitly tagged general (decide in the PR; per-set sources are #76).
- Factual questions can use the web lookup (#102) when it is on, as the interview helper does.
- In a translated call the advice is in English (what the user reads); the user still speaks and `[speak]` translates as today.
- Honesty rules in the prompt: do not invent facts about the user or the other person, say when something is unclear, and suggest asking rather than guessing.

Out of scope: speaking the advice into the call, a chat window with the model, more helpers than the three issues name, and changing the interview helper.

## Depends on
None beyond current main. Coordinates with #140 and #149 on `[assist].helper` (whichever lands first adds it).

## Acceptance
- [ ] `[assist].helper = "general"` and `--helper general` parse; default stays `interview` with unchanged behaviour; an unknown helper is a config error naming the choices (`cargo test -p dmn-core config`)
- [ ] Assistance off makes no request in the general helper (`cargo test -p dmn-cli general`)
- [ ] Transcript fixtures: a question to the user, a decision prompt, a request for help, small talk, the user's own monologue and an Enter request; only the first three and Enter produce a request (`cargo test -p dmn-detect`)
- [ ] MockProvider tests: general requests carry no interview instructions, kinds, STAR or resume text; interview requests are unchanged (`cargo test -p dmn-llm`)
- [ ] `moment` / `say` / `consider` validated and capped in `limits.rs`, streamed and cancelled through the existing path, rendered with general labels; snapshots at 80×24, 100×30, 140×30
- [ ] One local-model replay of a casual-call fixture documented in the PR for a qualitative read
- [ ] README and SPEC: the helper, what triggers it, what it shows, its limits
- [ ] `just check`, `just test` and `just lint` pass

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-detect
cargo test -p dmn-llm
cargo test -p dmn-cli general
just check
just test
just lint
```
CI uses synthetic transcripts and MockProvider; no microphone, PipeWire or live model.
