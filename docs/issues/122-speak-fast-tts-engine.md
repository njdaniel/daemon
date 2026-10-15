# #122 — speak: a faster voice engine — the translated sentence starts playing in a few hundred ms instead of 1–1.5 s

Labels: `size:L, model:opus, m4, lane:audio`

Proposed 2026-10-14, after Chinese was added to the user's Chatterbox server (zh is commented out upstream for a missing `pkuseg`; enabled locally with a mounted `mtl.py`). The user asked for a faster way to speak in another language: "whatever's fastest", free. Paid cloud TTS (ElevenLabs Flash v2.5, ~75 ms advertised plus network) is set aside for later.

Where the time goes today, from the #114 live test (Russian, Discord): end of speech → first audio 1.3–2.5 s. About 0.4 s is `vad_close_ms`, ~0.3 s whisper, 0.16–0.23 s the translator (`llama3.1:8b`), and the rest, 1–1.5 s, is Chatterbox before its first sound. The voice engine is the step to replace; a bigger translator would not make it faster.

Candidates (free, local, voice cloning; figures are from secondary write-ups and have not been measured here):
- **Qwen3-TTS** (Alibaba, Apache-2.0, 0.6B / 1.7B): Chinese, Russian, German among 10 languages; clones from ~3 s of audio, cross-lingual (one clip speaks every language); ~97 ms to first streamed audio reported.
- **CosyVoice 3** (Alibaba, Apache-2.0): Chinese, Russian, German among 9 languages; bi-streaming, ~150 ms reported. (CosyVoice 2 has no Russian or German.)
- Kept as the fallback: Chatterbox, which is what `dmn-tts` speaks to today.

## Measured 2026-10-14 (step 1)
RTX 4090, the user's clip (`voice-ru.wav`, English speech, 20 s), four sentences each in zh/ru/de, one request per sentence as dmn sends them. "Correct" = Whisper large-v3-turbo transcribes the audio back to the line (minor spelling aside). Scripts and audio stayed in the session scratchpad; listening samples in the owner's `~/dmn-tests/2026-10-14-tts-122/`.

| Engine | First audio | Whole sentence | Correct (zh / ru / de) | VRAM |
|---|---|---|---|---|
| Chatterbox (chatterbox-tts-api, `/v1/audio/speech/stream`) — baseline | 1.0–2.0 s (no audio until the sentence is done) | 1.0–2.0 s | 3 / 4 / 3 of 4 | 4.9 GB |
| Qwen3-TTS 1.7B Base, `qwen-tts` Python package (no streaming) | = whole sentence | 1.6–3.7 s (RTF 0.73) | 1 / 4 / 4 (zh ran away to 14.8 s once) | 4.6 GB |
| **Qwen3-TTS 1.7B Base, vLLM-Omni 0.30.0, streamed PCM, `x_vector_only_mode`** | **0.04–0.08 s** | 0.3–0.8 s (RTF ~0.13) | **4 / 4 / 4** | ~9 GB at the default 0.3 + 0.3 `gpu_memory_utilization` |
| Qwen3-TTS 1.7B Base, vLLM-Omni, ICL (`ref_text` = the clip's English transcript) | 0.07 s | 0.3–0.8 s | 0–1 / 4 / 4 (zh garbled or looping) | same |
| Fun-CosyVoice3 0.5B, vLLM-Omni, ICL | 0.55–0.58 s | 0.8–1.9 s | 1–2 / 1 / 1 | ~14 GB |

Findings: Qwen3-TTS through vLLM-Omni is the engine; with an English reference clip it must run in `x_vector_only_mode` (speaker embedding only), because ICL mode continues from the English transcript and derails Chinese. Not yet judged: how close the x-vector voice sounds to the user (the README says cloning quality "may be reduced" without `ref_text`). vLLM-Omni needed `VLLM_USE_FLASHINFER_SAMPLER=0` here (FlashInfer JIT-compiles its sampler and there is no `ninja`/`nvcc` on the box). CosyVoice 3 may need a prompt format the test did not give it; not pursued, as Qwen3-TTS is ahead on both speed and accuracy.

## Goal
With `[speak].engine` set to the new engine, a translated sentence starts playing in `dmn-voice` within ~1 s of the end of the user's speech (today 1.3–2.5 s) for zh, ru and de, in the user's cloned voice; Chatterbox still works when selected.

## Size / Model / Turns
L | Opus | 60

## Scope

### 1. Measure first (the PR reports the table before any client code)
- Run Qwen3-TTS (1.7B, and 0.6B if the 1.7B misses) and CosyVoice 3 on the 4090 as Docker sidecars on localhost, each cloned from `~/.local/share/dmn/voice-ru.wav`.
- For one short and one three-sentence line in zh, ru and de: time to first audio byte, real-time factor, VRAM, and a by-ear note on the voice and pronunciation. Same lines through Chatterbox for the baseline.
- Pick the engine with the lowest first-audio time that sounds right in all three languages; if neither beats Chatterbox by ≥ 0.5 s, stop and report.

### 2. Sidecar
- The chosen engine behind an HTTP server on 127.0.0.1: an existing OpenAI-compatible server if one serves it with streaming and voice cloning; otherwise a small wrapper kept under `sidecars/<engine>/` (Dockerfile + compose, localhost only) with the same three routes `dmn-tts` already uses: synthesize (streamed), list voices, enroll.
- Its license and why it was chosen go in docs/DEPENDENCIES.md.

### 3. Client (`crates/dmn-tts`, `crates/dmn-core/src/config.rs`)
- `[speak].engine = "chatterbox" | "<new>"`, default stays `chatterbox` until the live test passes.
- A provider for the new server implementing `TtsProvider` (synthesize streamed in chunks, `voices`, `enroll`); sentence splitting and the silence cut (`tail.rs`) apply only where the engine needs them.
- A cross-lingual engine needs one clip for every language: `enroll` without `--language` registers a voice the start-up check accepts for any `[speak].languages` code; per-language voices still work.
- `dmn doctor`: the engine, its URL, and its device.

### Docs
- README § Speak: choosing the engine, running its sidecar. SPEC § Speak: the engine setting. docs/issues/README.md line.

Out of scope: paid/cloud TTS (ElevenLabs; later, as its own issue), phrase-by-phrase translation before the sentence ends, lowering `vad_close_ms`, a different translator model.

## Depends on
#115 (merged; `[speak].languages`)

## Acceptance
- [ ] The measurement table (engine × language × short/long: first audio, RTF, VRAM, by-ear note) is in the PR body, with the Chatterbox baseline
- [ ] `[speak].engine` parses; an unknown value is a config error naming the choices
- [ ] The new provider's synthesize/voices/enroll are tested against a mock server in `testserver.rs` (no live server in CI)
- [ ] With a cross-lingual engine one enrolled voice serves zh, ru and de; the start-up check passes
- [ ] Live, by hand on Discord: first audio within ~1 s of the end of speech for zh, ru and de, timings in the PR body
- [ ] Chatterbox still works with `engine = "chatterbox"`
- [ ] README, SPEC and DEPENDENCIES updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-tts
cargo test -p dmn-core config
# live: [speak].engine = "<new>", dmn run --no-assist --app "", press l, speak a sentence: time to first audio in dmn-voice for zh, ru, de
```
