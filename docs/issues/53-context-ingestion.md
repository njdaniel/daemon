# #53 — dmn-context: ingest resume, job description, notes and stories (PDF/MD/TXT) into SQLite chunks; `dmn sources add/list/rm`

Labels: `size:M, model:opus, m2, lane:infra`

## Goal
Extract text from PDF, Markdown and plain-text sources, cut it into chunks of about 300 tokens with 50 overlap, and store them in SQLite with source, path and chunk index (SPEC § Context and LLM, Sources).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-context` (a skeleton `lib.rs` today): source kinds `resume | job_description | notes | stories` (resume and JD are single documents; notes and stories are folders, one file per story), text extraction, chunker. Token counts may be an approximation (e.g. whitespace words × 1.3), stated in the doc comment.
- `crates/dmn-store`: migration v4 (`SCHEMA_VERSION` is 3 today) adding `sources` and `chunks` tables, with a tested upgrade from v3; `Store` API to insert, list and delete by source.
- PDF text extraction through one pure-Rust crate (e.g. `pdf-extract` or `lopdf`), with a docs/DEPENDENCIES.md line. No C bindings and no OCR: a PDF that yields no text is reported as such.
- `apps/dmn-cli`: `dmn sources add <path> --kind <kind>`, `dmn sources list`, `dmn sources rm <id|path>`. Re-adding a path replaces its chunks.
- Fixtures under `testdata/context/`: a small resume (PDF and MD), a job description, two notes; generated or public domain, nothing personal.
- `dmn-context`'s `lib.rs` doc comment keeps stating its input, output and owned channel (CLAUDE.md).
Out of scope: everything else, including later issues.
## Depends on
#13, #16 (M1 merged)
## Acceptance
- [ ] Each format ingests: `dmn sources add` on the PDF, MD and TXT fixtures stores chunks of ≤ ~300 tokens with ~50 overlap; `dmn sources list` shows each source with its chunk count
- [ ] Unsupported or empty files are refused with a one-line reason, and nothing is stored
- [ ] `dmn sources rm` removes the source and all its chunks; `dmn sessions purge all` leaves sources alone
- [ ] Migration v3 → v4 is tested on a v3 database
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-context -p dmn-store
cargo run -p dmn-cli -- sources add testdata/context/resume.pdf --kind resume && cargo run -p dmn-cli -- sources list
```
