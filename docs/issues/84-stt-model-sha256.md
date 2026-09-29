# #84 — stt: `dmn doctor --fetch` and `dmn doctor` verify each whisper model's SHA-256, not only its size

Labels: `size:S, model:sonnet, m2, lane:stt`

Proposed 2026-09-29, from testing #79. `ggml-medium.bin`, `ggml-large-v3.bin` and `ggml-large-v3-turbo.bin` on the dev box had exactly the right size and the wrong content. `medium` then returned `,`, `and` or `!!!!…` for every utterance in every language mode, on the GPU and the CPU, so the live transcript stayed empty with nothing in the log. `dmn doctor` passed ("whisper model ok") because it and `--fetch` compare the size only, and `--fetch` leaves any file of the right size alone, so rerunning it could not repair one. The corrupt `medium` differed from the good file in 21 bytes: five single-bit flips of bit 5 (0x20), each at an offset ending in hex 7, and one 16-byte-aligned block. That points to the machine's memory rather than the download, and that is exactly what a checksum catches. The SHA-256 of every model is already listed in `crates/dmn-stt/src/model.rs`'s doc table (from the Hugging Face LFS metadata); the code never uses it.

## Goal
A whisper model whose SHA-256 is not the published one is never used silently: `--fetch` checks the hash before the rename and replaces a bad file, and `dmn doctor` fails its whisper check with the fix.

## Size / Model / Turns
S | Sonnet | 30

## Scope
`crates/dmn-stt/src/model.rs`: a `sha256` per `Model` beside `size_bytes`, taken from the doc table. `download_file` hashes while it streams, and a mismatch is a new `SttError::ChecksumMismatch` naming both hashes. A present file of the right size is re-hashed rather than trusted, so `--fetch` repairs it. `apps/dmn-cli/src/doctor.rs`: the whisper-model check hashes the configured model (about 1 s per GB) and fails on a mismatch with `fix: dmn doctor --fetch <name>`. `sha2` joins `dmn-stt` (already in the tree for `dmn-vad`'s Silero check; add the line to docs/DEPENDENCIES.md). Out of scope: hashing at every `dmn run` start, and Silero (already hashed).

## Depends on
#8, #77 (merged)

## Acceptance
- [ ] Every `Model` has its SHA-256; a test pins the table in `model.rs`'s doc comment to the code
- [ ] `--fetch` of a file whose bytes are wrong (test server) fails with `ChecksumMismatch` and leaves no file in place
- [ ] A present file of the right size and wrong hash is replaced by `--fetch`, and reported by `dmn doctor` as failed with the `--fetch` fix
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-stt model
dmn doctor | grep "whisper model"     # ok, or failed: sha256 … is not the published …; fix: dmn doctor --fetch <name>
```
