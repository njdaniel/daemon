# #113 — speak: translation-only mode — a small translator model instead of the answer model, and the TTS on the GPU, so a sentence reaches the call in a few seconds

Labels: `size:M, model:opus, m4, lane:llm`

Proposed 2026-10-07, after the first live Discord test of #107–#109 (German, in the user's cloned voice; it worked). The user's words: "i just want translation so no ai helper", asked what the delay was. Measured that session, from the key press: translation 0.4–0.6 s; CPU TTS first sound after 8 s (a short line), 13 s (one sentence), 62 s (a three-line turn), finishing 2–20 s later. The server renders at about 4–5× slower than real time on the CPU, so the wait grows with the sentence. The cause is what the box's GPU is spent on: `qwen3.6:35b-a3b` holds ~22 of 24 GB VRAM so that the interview helper answers in under 2 s, which a translation-only session never uses.

Found against `main` (a137b99):
- With `[speak].enabled` the run loads the `[llm]` model at start even with assistance off (#109 changed the #79 behaviour that unloaded it), because translation goes through the same `Provider`. The 21 GB answer model is therefore resident for a 0.5 s job a 5 GB model does.
- `llama3.1:8b` (4.6 GB) is already on the box; translation quality of small models for en→de/es is untested here.
- The TTS server runs on the CPU only because Docker on the dev box has no NVIDIA runtime (`nvidia-container-toolkit` is not installed; the host has the driver and CUDA 12.8). travisvn/chatterbox-tts-api ships `docker/docker-compose.gpu.yml`; its speed on a 4090 is unmeasured here.

## Goal
With assistance off and `[speak].enabled`, `dmn run` is a translator: it keeps the big answer model out of VRAM, translates with a small model, and the TTS server has the GPU, so the interviewer hears a one-sentence translation within a few seconds of the key press.

## Size / Model / Turns
M | Opus | 45

## Scope
- `crates/dmn-core/src/config.rs`: `[speak].model` (optional): the Ollama model used for translation; when unset, translation uses `[llm].model` as today. Template comment recommends a small one.
- `apps/dmn-cli/src/speaker.rs` / `suggest.rs`: the translator uses `[speak].model` when set. With assistance off at start-up and `[speak].model` set, the run does not warm or load `[llm].model` at all (the #79 unload behaviour returns for that case); Ctrl+Shift+A loads it then, as #79 does today. With assistance on, both models are resident and the `llm partly on cpu` warning applies as before.
- `dmn doctor`: reports the translator model and whether it is present; reports the TTS server's `device` from its `/health` (`cpu` or `cuda`) with a one-line hint when it is `cpu`.
- Measurement, in the PR body: translation quality on 10 en→de sentences for `[llm].model` vs the chosen small model (verbatim, no additions, judged by reading), translation latency, and key-to-first-audio with the TTS on the GPU vs the CPU.
- README § Speak: the translation-only setup (config lines, the small model, the GPU server with its one-time host setup: `nvidia-container-toolkit`, then `docker compose -f docker/docker-compose.gpu.yml …`), and the measured times. SPEC § Speak: the two modes and what is resident in each.

Out of scope: a smaller answer model, changing `[llm]` for the helper, auto-speak, the M3 window.

## Depends on
#109 (merged)

## Acceptance
- [ ] `[speak].model = "<small>"` parses; unset means `[llm].model`; `dmn config show` prints it; the template round-trips
- [ ] Assistance off at start with `[speak].model` set: no request to load `[llm].model` is made (test against the Ollama test server); a translation request names `[speak].model`; Ctrl+Shift+A then loads `[llm].model` as today
- [ ] Assistance on: unchanged behaviour, every existing suggest/pipeline test passes
- [ ] `dmn doctor` names the translator model and the TTS device
- [ ] By hand on the dev box, in the PR body: en→de quality table (10 sentences, both models), translation p50 for each, and key-to-first-audio for one sentence with the TTS on the CPU and on the GPU
- [ ] README and SPEC updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-cli suggest
cargo test -p dmn-cli pipeline
# by hand: [speak].model = "llama3.1:8b" (or the small model chosen), dmn run --no-assist, say a sentence, press t
```
