# #115 — speak: a picker switches the translation language during a session and saves it to the config

Labels: `size:M, model:opus, m4, lane:ui`

Proposed 2026-10-07, from the user's notes after the first live Discord tests: "want a settings i can go into and switch the language to translate into". Today the language is `[speak].target` in the config file, read once at start-up; changing it means editing the file and restarting, and the enrolled voice's language must match it (#109's start-up check), so each language needs its own enrolled voice. **Decided** with the user: an in-session picker over the TUI plus the config as the place for the list and the default.

## Goal
A key opens a small picker listing the languages the user has set up; choosing one makes it the translation target at once, the status tag shows it, and the choice is saved to the config so the next run starts with it.

## Size / Model / Turns
M | Opus | 45

## Scope

### Config (`crates/dmn-core/src/config.rs`)
- `[speak].languages`: a list of entries, each `{ code = "de", voice = "voice-de" }` (the whisper language code and the enrolled voice name for it). `[speak].target` stays the current/default one and must be one of the list's codes (or the list is absent and `target` + the single `voice_ref` behave as today).
- `dmn speak enroll --language <code>` adds or updates that language's entry (voice name defaults to `voice-<code>`), so enrolling German and Spanish gives a two-entry list without hand-editing.
- #109's start-up check runs per entry: every listed voice must be in the server's library with the matching `language`; a missing one is a config error naming the enroll command.

### Picker (`apps/dmn-cli/src/tui`, `run.rs`, `pipeline.rs`)
- A key (free one; say which in the PR) opens a centred overlay listing the configured languages with their codes and voice names, the current one marked; `↑`/`↓` move, Enter chooses, Esc closes. The overlay is drawn over the three areas without changing their layout (the layout rule in CLAUDE.md is about the areas; a transient overlay is acceptable and is stated in the SPEC). Snapshots at 100 × 30 and 80 × 24.
- Choosing dispatches an action the reducer owns (`Session::speak_target`); the speaker uses it for the next utterance (an utterance in flight finishes in its old language). The status tag from #114 (`voice: → es`) and the transcript marker update.
- The choice is written to `[speak].target` in the config with the same in-place line edit #107 uses for `voice_ref` (comments and other sections untouched); a write failure is a 5 s status notice, not a crash.
- With one configured language the key shows the picker with one entry (so the user learns the key does something) and a hint to enroll more.

### M3 note
- docs/design/ Settings view gets the same list and picker; this issue only adds one line to SPEC § UI saying so. No Tauri work here.

### Docs
- README § Speak: enrolling several languages, the picker key, that the choice is saved. SPEC § Speak + § UI (the overlay).

Out of scope: auto-detecting the interviewer's language to pick the target, translating into more than one language at once, per-call profiles (#76 is the home for that), the M3 window.

## Depends on
#114 (merged; the status tag and transcript marker it adds are what the picker updates)

## Acceptance
- [ ] `[speak].languages` parses; `target` not in the list is a config error naming both; `enroll --language xx` adds/updates the entry (test on a config with comments and other sections, only those lines change)
- [ ] Start-up check covers every listed voice; one missing names the enroll command
- [ ] The picker opens, moves, chooses and closes with the keys above; snapshots at 100 × 30 and 80 × 24 (closed, open with three entries, open with one entry + hint)
- [ ] Choosing updates `speak_target` in the session, the status tag and the transcript marker; the next translate request names the new language; an in-flight one keeps the old
- [ ] The choice is written to `[speak].target` in place; a read-only config file gives the 5 s notice
- [ ] README and SPEC updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-cli tui
cargo test -p dmn-cli pipeline
# live: two enrolled voices (de, es), dmn run, open the picker, switch, press t: the call hears the new language
```
