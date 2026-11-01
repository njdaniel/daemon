# #136 — sessions: save readable transcripts locally after each session

Labels: `size:M, model:opus, m4, lane:infra`

## Goal
Keep a readable transcript locally after each Daemon session, ready to open or give to an assistant for summarization later.

Daemon already stores finalized transcript segments in `~/.local/share/dmn/dmn.sqlite` during live and replay sessions unless `--no-store` is set. The missing piece is an ordinary text file the user can find and read without querying SQLite or filtering a JSONL export.

## Size / Model / Turns
M | Opus | 45

## Proposed behavior
1. Keep the existing incremental SQLite persistence during a session.
2. When a live or replay session ends normally, drain the transcript/store writer, then automatically save `~/.local/share/dmn/sessions/<id>.txt` (respecting the existing XDG data path). This includes sessions with interview assistance turned off.
3. Include the session ID, label and start time in a short header, then every finalized transcript segment in timeline order with its session-relative timestamp and existing speaker label. Keep both speakers' overlapping turns as separate entries. Exclude partials, AI suggestions and UI status messages.

   Example:
   ```text
   Session #42 — interview
   Started: 2026-10-23 14:30:00 UTC

   [00:03] Them: How would you design a rate limiter?
   [00:08] You: I would start by clarifying the limits.
   ```
4. Print the saved transcript path after the TUI exits or the headless/replay command completes. If exporting fails, report the failure clearly and leave the stored transcript in SQLite for a retry.
5. Add `dmn sessions export <id> --format text` to export an older session or recover a session after a crash. Keep `dmn sessions export <id>` as JSONL for compatibility; allow explicit `--format jsonl` too. Recovery exports include every finalized segment currently in the database, even if a crash left the session marked open.
6. `--no-store` writes neither the database session nor a text transcript. `dmn sessions purge <id|all>` removes the text files alongside the existing rows and exports.

## Scope / implementation plan
- `crates/dmn-store`: render finalized segments as plain text in timestamp order, preserving their text and speaker attribution.
- `crates/dmn-core`: add readable export alongside the current JSONL export; write through a temporary file and rename only after flush/sync succeeds, so a failed retry preserves a previous complete export.
- `apps/dmn-cli`: add the export format option, trigger text export after the pipeline/store has drained, and report the path or an actionable export error.
- README and SPEC: document the saved file location, automatic saving, the manual recovery command, `--no-store` and purge behavior. Update the path documentation to mention text exports.
- Tests: use isolated databases populated with transcript actions or existing fixture transcript text; no microphone, PipeWire or live LLM.

## Out of scope
- Summary generation inside Daemon. The user can give the saved transcript to an assistant later.
- Audio recording, cloud storage/sync, new model requests, imported transcript files, UI redesign, or changes to the existing transcript retention configuration.
- Crash-time automatic text export. SQLite remains the incremental record, and the manual export command recovers its finalized segments.

## Depends on
#13 and #79 (merged).

## Acceptance
- [ ] Normal completion of live, replay and assistance-off sessions saves a readable `<id>.txt` after all finalized segments have been persisted, and reports its path.
- [ ] Text export includes the session header and every finalized segment with timestamp and speaker in timeline order, including turns from the end of a session; partials, suggestions and UI messages are absent.
- [ ] `dmn sessions export <id> --format text` works for existing and crash-interrupted sessions; missing IDs produce a clear error without creating a bogus export.
- [ ] The existing JSONL export remains the default, with `--format jsonl` equivalent to today's command.
- [ ] `--no-store` creates no transcript file; purge removes the text export and database rows.
- [ ] A failed export is reported, preserves the SQLite transcript and any previous complete text file, and can be retried manually.
- [ ] README/SPEC explain local storage and the commands; no new dependencies or model requests are needed.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
- `cargo test -p dmn-store transcript_export`: finalized-only filtering, both speakers, overlapping/tied timestamps, multiline/non-ASCII text and the final utterance.
- `cargo test -p dmn-core --test session_log`: readable export, missing-session errors, replacement failure handling and purge, using temporary directories.
- `cargo test -p dmn-cli`: CLI format parsing and pipeline completion coverage for storage on/off and assistance off, without audio capture or a live model.
- `just check`, `just test`, `just lint`.
