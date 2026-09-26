# Daemon — Development Process

Every change enters the repo as a sized issue, is implemented by one agent run under a turn and dollar budget matched to its size, is reviewed by a different model, and merges only when CI and the reviewer agree. This file is the contract every issue, run and PR follows. `just agent <n>` and `just review <pr>` read it through the issue labels; if this file and a label disagree, fix the label.

Spec: `docs/SPEC.md`. Design: `docs/design/`. Build rules: `CLAUDE.md`.

## 1. Sizes

An issue gets exactly one size. The size sets the model, the budget and the reviewer.

| Size | Fits when | Model | Max turns | Budget (USD) | Reviewed by |
| --- | --- | --- | --- | --- | --- |
| S | One file or module, known crates, no threads, no FFI, tests trivial or already present | Sonnet | 30 | 5 | Opus |
| M | One crate of pure Rust with new tests on fixtures, or one async client | Opus | 45 | 15 | Opus, fresh session |
| L | FFI, `unsafe`, real-time or threading constraints, or one unfamiliar C-backed API | Fable | 60, two runs planned | 40 | Fable |
| XL | Anything larger | not allowed — split it | — | — | — |

The caps above are provisional (set 2026-09-21 before any run); replace each with measured cost × 1.5 after the first run of that size lands. Until a cap is recorded, `just agent` refuses to start a run of that size without an explicit `--budget`.

## 2. Breaking work down

Rules for whoever writes issues, human or agent:

- One issue introduces at most one new crate or one unfamiliar API. Two of {FFI, threads, UI} in one issue means it is two issues.
- Every issue is verifiable by a command in its Verify section without a live call: fixtures in `testdata/`, `dmn replay`, `MockProvider`, snapshot tests.
- Every issue leaves `main` green and shippable. No half-wired feature flags, no "finished in the next issue".
- `Depends on` lists merged prerequisites only; nothing references a later issue.
- If the estimate exceeds the L budget, split along the issue's own bullet list before opening it.
- Title: `<crate or area>: <outcome>`.
- Labels, all required: `size:S|M|L`, `model:sonnet|opus|fable`, `m0|m1|m2|m3|m4`, `lane:audio|stt|llm|ui|infra`.

## 3. Issue template

```markdown
## Goal
One sentence from the spec section.
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
Crates/files touched. Out of scope: everything else, including later issues.
## Depends on
#n, #m (all merged)
## Acceptance
- [ ] criterion copied from docs/SPEC.md Milestones
- [ ] `just check`, `just test` and `just lint` green
## Verify
Exact command(s) and expected output, runnable without a live call.
```

## 4. Running an issue — `just agent <n>`

1. The dispatcher (cron or by hand) picks the lowest-numbered open issue whose `Depends on` are all merged and whose lane has no run in flight.
2. It calls `claude -p` with the kickoff prompt plus the issue body, `--allowedTools` from the size, and `--max-turns` / `--max-budget-usd` from the labels.
   - S and M: file edits, `just check`, `just test`, `just lint`.
   - L: the above plus `cargo run -p dmn-cli -- replay …` and `just probe`.
   - Never: network beyond crates.io, `git push --force`, anything under `~/.config/dmn/`.
3. Commit-only contract: the run works on branch `issue/<n>-<slug>`, commits, and opens a PR whose body is the issue's acceptance checklist with the command used to verify each line. It never merges.
4. CI runs `just check`, `just test`, `just lint`; a red CI is the run's own problem to fix within its budget.

**Kickoff prompt** (prepended to every issue body; the number changes):

```
Read CLAUDE.md, docs/PROCESS.md and docs/SPEC.md in full before writing code.
Implement issue #N exactly as scoped; do not start later issues or refactor beyond it.
Run `just check` after every edit, and `just test` + `just lint` before you finish.
Open a PR whose body lists each acceptance criterion with the command you ran to verify it.
If the spec and the issue disagree, the issue wins; say so in the PR.
```

## 5. Review — `just review <pr>`

- One review is enough, and it starts at once. `just review <pr>` runs a fresh session of Gemini 3.1 Pro through the `agy` CLI: a different model family from every author model, and no Claude usage. `--via claude` uses the reviewer from the size table instead (Opus reviews Sonnet and Opus PRs; Fable reviews every L PR regardless of author); `--model` overrides either. The reviewer gets the PR diff, the issue, `docs/SPEC.md` and this prompt:

  ```
  List every acceptance criterion from the issue and state whether the diff meets it, with evidence from the diff.
  List blocking findings: correctness, safety (unsafe, RT callback rules, audio on disk), spec violations.
  Do not restyle, do not suggest scope beyond the issue.
  End with exactly one line: BLOCKING: <n> findings.
  ```

- GitHub Copilot reviews every PR on its own schedule, and nothing waits for it. If its review exists when `just review` runs, every Copilot finding (overview and inline, marked still-anchored or outdated) goes into the prompt and the reviewer marks each one VALID, FIXED or FALSE with evidence, summed on a `COPILOT:` line; a VALID finding about correctness, safety or a CLAUDE.md / SPEC rule counts as blocking. If Copilot has not reviewed yet, the review proceeds without it and the prompt says so. `--copilot-wait <s>` waits that long first; `--no-copilot` leaves Copilot's findings out even when present. History: a hard gate from 2026-09-22 (10-minute wait, exit 3 without a Copilot review) after Copilot findings on PRs #17–#25 were merged untriaged (cleanup in #30); relaxed 2026-09-26 in #46 after reviews stalled on the wait and PRs #40 and #41 were each reviewed twice.
- Prompt size: `agy` drops input past about 192 KB and Gemini reviews what arrived without saying so (measured 2026-09-26 with numbered lines; PR #48's 217 KB prompt lost its last 9 files and the Copilot findings and still printed `BLOCKING: 0 findings`; PRs #67 and #71 were over the limit too). `just review` caps the prompt at 160 KB (`--max-prompt-bytes`, 0 for none) and, over the cap, `scripts/review_passes.py` splits the diff by file into passes that each carry the issue, the PR, the SPEC and the Copilot findings anchored in their files. The review file ends with the total over all passes, and a pass that ends without a `BLOCKING:` line fails the review instead of passing it.
- Blocking findings go back as one fix run of the original issue with the review appended to the prompt. A second failure escalates to a human.

## 6. Merge

A PR merges when all of these hold:

1. CI green.
2. One review passes (§ 5): `just review` ends with `BLOCKING: 0 findings`, its `COPILOT:` line covering every Copilot finding that existed when it ran; or, without a `just review` run, Copilot's review with every finding triaged on the PR (fixed, or answered as false with a reason). Either one is enough, and neither waits for the other.
3. Human approval at the checkpoint issues (#5, #10, #14, #16) — see §8.
4. Squash-merge; commit title `#<n>: <issue title>`.

## 7. When a run fails

- Budget exhausted, tests green, acceptance unmet: split the remainder into a new issue, close the original with a pointer, merge nothing.
- Budget exhausted, tests red: discard the branch; re-run once with the failure log appended to the prompt.
- An L issue that fails twice becomes an interactive session; notes from that session update `docs/SPEC.md` before any further headless run touches that crate.
- A review that flags a spec problem rather than a code problem: open a spec issue (`lane:infra`, `size:S`), fix the spec, then re-run.

## 8. Human checkpoints

| After | Question a human answers |
| --- | --- |
| #5 | Does per-app capture work on the target PipeWire (`dmn doctor --probe` on the dev machine)? |
| #10 | Are the detector weights sane on the fixtures, or overfit to them? |
| #14 | Does the TUI read in 2–5 seconds in a 100 × 30 pane? |
| #16 | First live call: latency, echo, hallucinations — record numbers in the spec. |

## 9. Definition of done, every PR

- `just check`, `just test`, `just lint` green.
- Every acceptance criterion ticked with its verify command in the PR body.
- No new dependency without a line in `docs/DEPENDENCIES.md`.
- No `unsafe` outside `dmn-audio` and `dmn-stt`; every `unsafe` block carries a `// SAFETY:` comment.
- No audio written to disk unless `privacy.record_audio` is true.
- Squash-merged with the issue number in the commit title.

## 10. Issues for M0 and M1

Acceptance criteria for each come from the Milestones table in `docs/SPEC.md`. `Depends on` is what must be merged first.

| # | Title | Milestone | Depends on | Size | Model | Turns |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | infra: workspace scaffold, every crate an empty lib with its `lib.rs` doc comment, `dmn --version`, `justfile`, CI green | M0 | — | M | Opus, interactive | — |
| 2 | infra: config TOML loader, XDG paths, `dmn doctor --init`, `tracing` JSON log | M0 | 1 | S | Sonnet | 30 |
| 3 | dmn-audio: `AudioSource` trait, `WavSource`, `ringbuf` plumbing, resampler; unit tests on fixtures | M0 | 2 | M | Opus | 45 |
| 4 | dmn-audio: PipeWire main-loop thread, registry listener, live node table, `dmn devices` | M0 | 3 | L | Fable | 60 |
| 5 | dmn-audio: `PipeWireSource` capture stream with `target.object`, sink-monitor fallback, `dmn doctor --probe` | M0 | 4 | L | Fable | 60 × 2 |
| 6 | infra: `dmn doctor` full checks (libpipewire, model file, Ollama, GPU backend) + CI system deps and cached whisper.cpp build | M0 | 2 | S | Sonnet | 30 |
| 7 | dmn-vad: energy VAD + utterance segmenter, tests on fixtures | M1 | 3 | M | Opus | 45 |
| 8 | dmn-stt: `whisper-rs` wrapper, model lookup/download, `dmn bench` real-time factor | M1 | 6, 7 | M | Opus | 45 |
| 9 | dmn-stt: per-source worker thread, partials, hallucination filter; `dmn replay` prints labelled segments | M1 | 8 | L | Fable | 60 × 2 |
| 10 | dmn-detect: rule scorer; `eval/run.py` recall/precision on fixtures | M1 | 9 | M | Opus | 45 |
| 11 | dmn-llm: `Provider` trait, Ollama client (NDJSON streaming, JSON-schema `format`), `MockProvider` | M1 | 2 | M | Opus | 45 |
| 12 | dmn-llm: fast-phase prompt + schema + `limits.rs`; replay prints question + approach JSON | M1 | 10, 11 | M | Opus | 45 |
| 13 | dmn-session + dmn-store: state reducers, `watch`/broadcast, SQLite schema + migrations; replay writes a session log | M1 | 12 | M | Opus | 45 |
| 14 | ui: static ratatui layout rendered from a saved `Session` JSON fixture; snapshot tests | M1 | 13 | S | Sonnet | 30 |
| 15 | ui: live wiring, keys `p r m q`, status line, `dmn run --replay` | M1 | 14 | M | Opus | 45 |
| 16 | infra: live-call tuning of VAD thresholds and detector weights, `dmn bench --live` report | M1 | 15 | M | Opus, interactive | — |

Lanes: audio (#3–#5, then #7–#10) and LLM (#11) share only #2, so two agents can run in parallel. Issues #1 and #16 are interactive by design: #1 sets the conventions every later run copies; #16 needs a human on a live call. Running #4 and #5 as one interactive session with a human watching is the cheapest way to cut Fable spend.

M2, M3 and M4 issues are opened after #16 lands, from the Milestones table, sized by §1.
