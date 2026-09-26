# #76 — dmn-context: per-interview context sets — `dmn sources use <name> [--from <folder>]`, scoped sources, one active interview

Labels: `size:M, model:opus, m2, lane:infra, backlog`

Proposed 2026-09-26 as a backlog item. Prompted by the M2 live-run prep: the store keeps one job description, so preparing for several companies means overwriting it before each interview and losing the previous one, and every company's notes share one retrieval pool. Decisions taken with the user: folder convention for ingestion, stories global by default, the command stays under `dmn sources`. The M3 Context view's `[COMPANY]` subtitle and per-source "Use" checkboxes (docs/design/Context.dc.html) build on this model.

## Goal
Keep one named set of sources per company/interview (its job description, notes and, optionally, its own stories), switch between them with one command, and let a running session follow the switch between questions (SPEC § Context and LLM, Sources; § Data, config and privacy).

## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match). At the top of M: if the budget is tight, split along the crate line into (1) store + context (M, shippable alone: a store with no interviews behaves as today) and (2) CLI + docs (S).

## Scope
- `crates/dmn-store`: schema v8 — `interviews(id, name UNIQUE COLLATE NOCASE, created_at)`, `settings(key, value)` holding `active_interview`, `sources.interview_id` (NULL = global, `ON DELETE CASCADE`), `sources.hash` (FNV-1a of the extracted text, so re-syncs skip unchanged files); data step moves existing non-resume sources into an interview `default` and activates it (a database with only a resume gets no interview). `path` stays globally UNIQUE: a file lives in exactly one scope and re-adding it elsewhere moves it (a `UNIQUE(interview_id, path)` rebuild is a SQLite trap: NULLs are distinct, and `DROP TABLE sources` cascades into `chunks` under `foreign_keys = ON`). API: `interviews() / interview_by_name / create_interview / delete_interview / active_interview / set_active_interview`, `chunks_in_scope(active)` replacing `all_chunks`, `source_by_path`, `sources_signature` carries the active id (so `Library::is_stale` fires on a switch), `put_jd_summary` keeps the 16 newest hashes instead of one (switching back is free), `open_read_only` on an old schema returns `StoreError::Schema { found, expected }` naming the fix instead of `None`.
- `crates/dmn-context`: new `interview.rs` with `Scope { Global, Interview(id) }`, `scope_for(kind, flag, active)` — resume always global (`--interview` is an error); job description always the active interview (none active, or `--global`, is an error naming `dmn sources use`); notes default to the active interview, `--global` opts out; stories default global, `--interview` opts in — and `sync_folder(store, folder, interview, now)` for the layout `job-description.*` (into the interview), `resume.*` (global), `notes/`, `stories/` (into the interview); strays refused with one line each; two job descriptions or two resumes is an error before any write; re-runs re-add changed files, keep unchanged ids, remove sources whose file is gone from the folder, and leave sources added from outside the folder alone. `ingest(store, path, kind, scope, now)` skips an unchanged file by hash. `Library::load` reads the active scope (`chunks_in_scope(signature.active)`); `from_chunks`, `is_stale`, `retrieve*` unchanged.
- `apps/dmn-cli`: `dmn sources use <name> [--from <folder>]` (creates if missing, activates, syncs; prints `created interview acme (#2): 0 source(s)` / `interview acme (#2): 3 source(s)` and the per-file `added / re-added / unchanged / removed / refused` lines), `dmn sources interviews` (`* acme (#2) 3 source(s)`, the active one starred), `add --global | --interview`, `list [--all]` with an `interview: acme (#2)` header and a `scope` column (`global` or the name), `rm --interview <name>` (removes the set, clears the active id if it was active and says so). `print_context` and replay's context line name the interview; JSON gains `"interview"`. Interview names trimmed, non-empty, ≤ 64 chars.
- Mid-session: `dmn sources use` in another terminal changes the signature → the next question reloads the library in the new scope and reads that job description's cached requirements; a job description never summarised stays without requirements until the next session start (the existing "never summarise on a question's path" rule). `--no-store` reads `settings` on its read-only connection.
- `docs/SPEC.md` § Sources gains an **Interviews** paragraph (scope rules, `use` and the folder layout, retrieval follows the active interview); the data table row for `dmn.sqlite` adds "interviews, the active interview". `docs/issues/README.md` line.
Out of scope: the M3 Context view, copying sources under `~/.local/share/dmn/sources/`, summarising a newly switched-to job description mid-session, a file shared by two interviews, everything else including later issues.

## Depends on
#53, #54, #58 (all merged)

## Acceptance
- [ ] A v7 database opens at v8 with its non-resume sources in an active interview `default` and its resume global; a v7 database with only a resume gets no interview (`cargo test -p dmn-store`)
- [ ] `dmn sources use acme --from <folder>` ingests the layout, refuses strays with one line each, and a second run re-adds changed files, keeps unchanged ids and removes files gone from the folder (`cargo test -p dmn-context -p dmn`)
- [ ] Two interviews each keep their own job description; `dmn sources list` shows the active one plus global sources, `--all` shows every set; `rm --interview` removes a set and clears the active id if it was active
- [ ] Notes default to the active interview and stories to global; `--global` / `--interview` override; a resume with `--interview`, a job description with `--global`, and a job description with no active interview are one-line errors naming the fix
- [ ] Switching the active interview between two questions reloads the library and reads that job description's cached requirements without a second summary call (`cargo test -p dmn context::`)
- [ ] `dmn run --no-store` on an old-schema database prints `no context: … schema v7 …` rather than running silently without context
- [ ] `just check`, `just test` and `just lint` green

## Verify
```
cargo test -p dmn-store -p dmn-context -p dmn
mkdir -p /tmp/acme/notes && cp testdata/context/job-description.txt /tmp/acme/ && cp testdata/context/notes/* /tmp/acme/notes/
cargo run -p dmn-cli -- sources use acme --from /tmp/acme        # "interview acme (#1) from /tmp/acme", three "added" lines, "interview acme (#1): 3 source(s)"
cargo run -p dmn-cli -- sources add testdata/context/stories --kind stories   # "(global)" on each line
cargo run -p dmn-cli -- sources use globex                       # "created interview globex (#2): 0 source(s)"
cargo run -p dmn-cli -- sources list                             # "interview: globex (#2)" and only the global stories
cargo run -p dmn-cli -- sources interviews                       # "* globex", "  acme"
```

## Tests to add (for the run)
- dmn-store: v7 → v8 upgrade (default interview, resume global, `chunks_in_scope` counts, AUTOINCREMENT kept); resume-only database gets no interview; interviews round trip, NOCASE duplicate, deleting the active one clears it and cascades; the single-document rule is per interview and a path moves between scopes; the signature follows the active interview; JD summaries keep 16 hashes; read-only open on an old file is `StoreError::Schema`.
- dmn-context: `scope_for` table over kind × flag; `sync_folder` ingests the layout and refuses strays; a second sync is idempotent and follows the folder (fixtures copied, not symlinked, into a tempdir); two job descriptions is an error that writes nothing; a folder of strays only is nothing ingested; re-adding an unchanged file keeps its id; `Library::load` reads the active interview plus global sources and a switch makes it stale.
- dmn-cli: `use --from` creates and re-syncs; scope flags and their three errors; `rm --interview`; the existing add/list/rm test uses `use` first; context.rs: switching mid-session swaps the job description and keeps both caches with one mock summary call; `--no-store` on an old schema surfaces the schema error.
