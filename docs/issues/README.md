# Issues

One file per issue in the template from `docs/PROCESS.md` § 3. The first line is the GitHub issue title; the `Labels:` line lists the four required labels. `just issues-push` creates the labels and issues on GitHub (skipping titles that already exist); after that, GitHub is the source of truth and these files are the archive.

Lanes: audio (#3–#5, #7), stt (#8–#9), llm (#10–#12), ui (#14–#15), infra (the rest). Human checkpoints after #5, #10, #14 and #16 (`docs/PROCESS.md` § 8). #1 was done interactively on 2026-09-21. M1 finished with #16 on 2026-09-25.

M2 (Context), opened 2026-09-25 as #53–#63. Context first, then accuracy:
- context / infra: #53 ingestion → #54 retrieval + JD summary
- llm: #55 slow phase (after #54) → #57 session memory; #58 stories + STAR (after #54, #56); #59 remote providers
- ui: #56 six sections (after #55)
- stt / audio: #60 echo guard, #61 GPU whisper, #62 Silero VAD → #63 recall and precision ≥ 0.9 (after #60, #62)

Proposed for the next iteration (2026-09-26, `backlog` label): #76 per-interview context sets (`dmn sources use <name> [--from <folder>]`; the M3 Context view builds on it) and #77 multilingual interviews (`[stt].language = "auto"`, a multilingual whisper model translating both lanes to English; Spanish fixture in `fixtures-v3`). #79 (assist toggle: `[assist].enabled`, key `a`, transcribe/translate mode that skips the detector and the LLM) was written on GitHub on 2026-09-26 and is mirrored here with the 2026-09-28 GPU additions: a start-up switch, unloading Ollama so whisper gets the card, and what the runtime toggle cannot move. #82 (infra: `just build` includes Vulkan GPU whisper when `libvulkan-dev` and `glslc` are installed, `just build cpu` / `gpu` to force) came out of testing #79 on the dev box (2026-09-29).

Created on GitHub only, with no file here: #34 (replay `--speed`), #39 (whisper `no_context`), #50 (coding-solutions switch), and the process issues #44, #45, #46, which carry the `backlog` label.
