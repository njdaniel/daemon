# #85 — stt: re-measure #77's `medium` and `large-v3` numbers with checksum-verified model files

Labels: `size:S, model:sonnet, m2, lane:stt`

Proposed 2026-09-29. The `medium`, `large-v3` and `large-v3-turbo` files used for #77's 2026-09-28 measurements were found corrupt on 2026-09-29 (right size, wrong SHA-256; see #84), and were re-downloaded and verified that day. The numbers in docs/SPEC.md § Transcription and the README were taken with those files. `medium`: recall 1.00 / precision 1.00 on `es/`, WER 18–27 % on short clips, 2.6 s per utterance on the CPU, 0.13 s on the 4090. `large-v3`: 1.00 / 1.00, WER 22 %, 0.21 s on the GPU, 5.6 s on the CPU. `large-v3-turbo` "returned Spanish for the Spanish fixture" under the translate task, which is why PR #78 refuses it for a language other than `en`. When the damage happened is not known, so each number, and the turbo refusal, needs a re-run on good files.

## Goal
Every #77 figure for `medium`, `large-v3` and `large-v3-turbo` is re-measured with files whose SHA-256 matches the published one, and SPEC / README say what was re-measured and what changed.

## Size / Model / Turns
S | Sonnet | 30

## Scope
Measurement and docs only: `just eval` on `es/` and the English fixtures under `language = "auto"`, `dmn bench --fixture es` on the CPU and the GPU (Vulkan build), for `medium`, `large-v3` and `large-v3-turbo`. Update docs/SPEC.md § Transcription, the README multilingual section, and the `large-v3-turbo` refusal in `crates/dmn-stt/src/model.rs` only if turbo does translate on a good file (then as its own commit, with the evidence). Out of scope: default model changes.

## Depends on
#77 (merged); ideally #84, so the files are verified by the tool itself

## Acceptance
- [ ] `sha256sum ~/.cache/dmn/models/ggml-{medium,large-v3,large-v3-turbo}.bin` matches the published hashes before any run, recorded in the PR
- [ ] recall / precision on `es/` and the English fixtures, per-utterance time on CPU and GPU, and `es/` WER for each of the three models, in the PR and in SPEC
- [ ] whether `large-v3-turbo` translates under the translate task, with its output for the first three `es/` turns
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
for m in medium large-v3 large-v3-turbo; do dmn bench --model $m --device gpu --language auto --fixture es --turns 5; done
just eval
```
