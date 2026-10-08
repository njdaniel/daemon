# Daemon — Design Spec & Build Plan

2026-09-21 · @Someone

## Overview

Daemon is a Linux-native floating copilot that listens to a live interview call, detects the interviewer's questions, and shows a five-line answer scaffold within about 2 seconds — never more on screen than can be read in 5 seconds.

It is for a software engineer interviewing over Google Meet, Zoom or Teams on a Linux desktop (Wayland first, X11 supported) who has prepared a resume, a job description, notes and STAR stories, and wants a quiet, local-first aid. The MVP is a terminal program (`dmn run`) that runs in a pane beside the call; the final product adds the floating window on the [Daemon canvas](https://claude.ai/artifact/Kya3g5rdSzQ7k9tGPqZV2B): 440 px wide, three areas — Live Question, AI Assistance, Transcript & status — plus Context, Settings and a collapsed pill.

**Principles**

- Glanceable: every default view fits in 2–5 seconds of reading. Nothing auto-expands; one section open at a time.
- Local-first: audio never leaves the machine. Transcript text leaves only when the user explicitly selects a remote model. A web search query (a few words the model writes, never transcript text) leaves only when the user turns the web lookup on (#102), to Ollama's web search or to the user's own SearXNG, which forwards it to its engines; it is off by default.
- Deterministic where possible, AI where needed: capture, segmentation, speaker attribution and question detection are code (Tier-C); interpretation and answers are the LLM (Tier-A); the user decides what to say (Tier-H).
- Latency budget: end of the interviewer's question to visible suggestion ≤ 2.0 s p50 on a mid-range laptop.
- Honest tool: helps in practice and where AI assistance is permitted. No screen-share evasion, no hidden windows, no typing on the user's behalf. Daemon never speaks unprompted and never speaks words the user did not say or ask for: with `[speak]` on (#107, #108) it speaks the user's own words, or their translation (#109), in the user's own cloned voice, only when the user asks (`dmn speak`; the key in #109).

**Non-goals for v1**

- Windows or macOS builds
- Joining calls as a bot, or reading the calling app's own UI
- Auto-answering, code execution, or pasting into the interview
- Accounts, cloud sync, telemetry

## Decisions

Rust is the language: one binary, one process, native PipeWire and whisper.cpp bindings, no GC and no subprocesses on the audio path. Python is used only for the offline eval harness. Rust costs more agent turns per issue than Go would; the handoff section budgets for that.

| Area | Choice | Why | Also considered |
| --- | --- | --- | --- |
| Language | Rust, stable toolchain, edition 2024; Cargo workspace | Lowest end-to-end latency: audio callbacks, VAD and whisper all in-process on dedicated threads; first-party PipeWire binding; single static-ish binary | Go (faster to write, but no maintained PipeWire binding and a beta webview shell); Python (eval scripts only) |
| UI shell | M1–M2: terminal TUI with `ratatui` + `crossterm` (`dmn run`). M3+: Tauri 2 (stable) on wry/WebKitGTK 4.1, GTK3, as a second surface over the same `Session` state | The TUI ships the pipeline weeks earlier, runs in tmux beside the call, and proves the 2–5 s readability rule with zero GUI dependencies; the web front end then ports the canvas design 1:1 with Tauri commands + events | iced or egui (pure Rust, no WebKitGTK dependency, lowest render latency, but the Linear-style design would be hand-built); gtk4-rs + libadwaita (native, wrong aesthetic) |
| Front end | M3: Vite + TypeScript, no framework; `@tauri-apps/api` invoke + listen | Three views and one state object; a framework costs more than it saves | Svelte or Solid if state grows |
| Audio | [`pipewire` crate](https://docs.rs/crate/pipewire/latest) (official pipewire-rs) 0.10: main loop on its own thread, one capture `Stream` per source, registry listener for discovery, `target.object` to capture one app's output node | Native, per-app capture, hot-plug events, 10 ms quantum; no PCM over pipes | `pw-record` subprocess (kept only as the `dmn doctor` sanity check and emergency fallback); cpal (no per-app targeting); PulseAudio shim |
| Virtual microphone (#108) | One more `pipewire` `Stream` on the same loop thread, `Direction::Output` with `media.class = Audio/Source`, `node.name = dmn-voice`, F32 mono at the graph rate (`clock.rate` from the `settings` metadata, 48 kHz on the dev box; the writer's thread resamples the TTS server's 24 kHz by linear interpolation), no `AUTOCONNECT` so nothing links it to a sink. Its process callback pops from a lock-free SPSC ring and zero-fills on underrun; the writer waits when the ring is full (backpressure reaches the HTTP body read through `dmn-tts`'s bounded channel), never drops. The node lives from `VirtualMic::open` to `close`/drop: `dmn speak` opens it before the request and closes it at exit, so nothing lingers in the audio settings; a session that speaks repeatedly (#109) keeps one open | Calls list it as a microphone with no module, no config file and no `pw-cli` step; it is gone when the process ends, on error too, because the server destroys a client's nodes with its connection | `module-null-sink` + loopback via `pw-cli`/`pactl` (leaves devices behind, needs the user's shell); `pw-play --target` subprocess (no `Audio/Source` class, so not a microphone); `module-filter-chain` (config files) |
| STT | `whisper-rs` (whisper.cpp via cmake; features `vulkan`, `cuda`, `openblas`); `ggml-small.en` default, `base.en` on weak CPUs, `large-v3-turbo` with GPU; the multilingual `small` (or `medium`, `large-v3`) for a call in another language (#77; `large-v3-turbo` does not translate) | Same engine as before, first-class Rust API, GPU backends by feature flag | `whisper-server` sidecar (chunk-per-request loses context); faster-whisper sidecar (Python; kept as an optional `stt.engine` if GPU throughput matters) |
| VAD | M1: energy + silence-gap VAD in Rust. M2: Silero via `ort` (ONNX Runtime) | Enough to segment utterances for the first model | Silero from day 1 (adds ORT to M1) |
| Async & threads | tokio for HTTP, SQLite access and UI events; `std::thread` for the PipeWire loop and each whisper worker; `ringbuf` SPSC ring buffers from capture to VAD; `tokio::sync::broadcast` for session state | Real-time capture never touches the async runtime | async-std; a single-threaded design |
| LLM | `Provider` trait (async, streaming) with three implementations: Ollama (`/api/chat`, NDJSON stream, JSON-schema `format`), OpenAI-compatible (`/v1/chat/completions`, SSE), Anthropic Messages API. Default: Ollama, model from config (`qwen3.6:35b-a3b`, chosen 2026-09-21 by measurement over `llama3.1:8b` and `qwen3.6:27b`). `reqwest` + `serde` | Local by default; remote is one config change; structured output keeps the UI deterministic | Framework crates (wrong layer) |
| Storage | `rusqlite` (`bundled`) with a small migration runner; `serde_json` for exports | Session memory needs queries; bundled SQLite avoids distro version drift | sqlx (async, but overkill); JSONL only |
| Config & CLI | TOML via `serde` + `toml` at `~/.config/dmn/config.toml` (`directories` crate); binary `dmn` via `clap` with `run`, `doctor`, `devices`, `sources add`, `replay`, `bench` | Fits terminal-first use; `doctor`, `replay` and `bench` make Claude Code's work testable without a live call | GUI-only settings |
| Packaging | M1: `cargo tauri build` (AppImage + deb from Tauri's bundler). M3: Flatpak (needs the PipeWire socket permission) | AppImage carries WebKitGTK cleanly | Snap (no) |
| Always-on-top | X11/XWayland: `set_always_on_top` (GTK keep-above). Wayland: no xdg-shell API; try compositor support (Hyprland, sway, KDE), else a setting that launches under XWayland (`GDK_BACKEND=x11`) | Wayland-first without lying about what compositors allow | wlr-layer-shell via `gtk-layer-shell` (not on GNOME; M3 option for tiling WMs) |

## Architecture

One Rust process, one thread per real-time stage, a tokio runtime for everything that talks to the network or the window, and a single `Session` state struct that the UI mirrors over Tauri events. Every stage can be fed from a WAV file instead of PipeWire, so the whole pipeline runs headless in tests and in `dmn replay`.

```mermaid
flowchart LR
  MIC[PipeWire Stream<br/>mic] --> SEG1[VAD + segmenter<br/>source=you]
  SYS[PipeWire Stream<br/>app output node] --> SEG2[VAD + segmenter<br/>source=interviewer]
  SEG1 --> STT[whisper-rs workers<br/>one per source]
  SEG2 --> STT
  STT --> BUS[Transcript bus]
  BUS --> DET[Question detector<br/>Tier-C rules]
  DET --> CTX[Context assembler<br/>sources + session memory]
  CTX --> LLM[LLM provider<br/>fast + slow phase]
  LLM --> STATE[Session state]
  BUS --> STATE
  STATE --> TUI[ratatui TUI<br/>M1]
  STATE --> GUI[Tauri window<br/>M3]
  STATE --> DB[(SQLite)]
```

Audio enters as two labelled streams, so speaker attribution is a property of the source, not an inference. The detector only proposes; the LLM's fast phase confirms and extracts the question; the UI shows the fast phase within the latency budget and fills expandable sections from the slow phase afterwards.

**Interview assistance is a layer, not the pipeline (#79).** Everything up to the transcript bus (capture, VAD, whisper, translation under `[stt].language`, the echo guard, the session and the store) always runs. The detector, the context assembler and the LLM sit on top and can be switched off: `[assist].enabled = false`, `dmn run --no-assist`, or Ctrl+Shift+A during a call (a chord, so a stray key cannot switch it; terminals without the kitty keyboard protocol send it as Ctrl+A, which counts too). Off, every segment still reaches the session, the store and the transcript, but none is scored, no context is assembled and no fast- or slow-phase request is made; `r` and `w` ask nothing. Turning it off forgets the line the detector holds and the question being asked, so nothing said before the gap completes a question after it; on again, detection resumes with the next interviewer final, and questions asked while it was off are not answered retroactively. A run that starts with assistance off never starts the suggester (no warm-up either) and, with a local Ollama, sends it one `keep_alive: 0` request before whisper loads, since Ollama otherwise holds the model 30 minutes after its last request: whisper can then take the GPU (`medium` at 0.13 s per utterance there against 2.6 s on the CPU). The first Ctrl+Shift+A starts the suggester, which loads the model into what whisper left; when Ollama's `/api/ps` shows 10 % or more of it in system memory the status line says `llm partly on cpu` (answers come, slower; the default model normally sits at 4 % on the dev 4090, which says nothing). The toggle never moves whisper between devices, and turning assistance off mid-call does not unload the model.

**Repo layout** (`daemon` repo, one Cargo workspace, binary `dmn`)

| Path | Holds |
| --- | --- |
| `crates/dmn-audio/` | `AudioSource` trait; `PipeWireSource` (registry discovery, capture streams, hot-plug), `WavSource`; ring buffers, resampler; `VirtualMic` (#108), the `dmn-voice` playback stream the call hears |
| `crates/dmn-vad/` | Energy/silence VAD (M1), Silero via `ort` (M2); emits `Utterance { source, start, end, samples }` |
| `crates/dmn-stt/` | `whisper-rs` wrapper, per-source worker thread, sliding-window partials, hallucination filter |
| `crates/dmn-transcript/` | `Segment` types, bus, punctuation/merge rules |
| `crates/dmn-detect/` | Question detector: cue rules, scoring, pin logic |
| `crates/dmn-context/` | Source ingestion (PDF via `pdf-extract`, MD, TXT), chunking, BM25 retrieval, session summariser |
| `crates/dmn-llm/` | `Provider` trait; `ollama`, `openai`, `anthropic` modules; prompt templates; JSON schema + `limits.rs` |
| `crates/dmn-tts/` | `TtsProvider` trait; the chatterbox-tts-api client (#107): voice enrollment, synthesis, the WAV body decoded into mono f32 chunks on a bounded channel |
| `crates/dmn-session/` | `Session` state, reducers, `broadcast` fan-out |
| `crates/dmn-store/` | `rusqlite` schema + migrations |
| `crates/dmn-core/` | Wires the pipeline; the only crate that knows every stage; used by both binaries |
| `apps/dmn-cli/` | The `dmn` binary: `run` (TUI, `--replay`, later `--gui`), `doctor`, `devices`, `sources`, `config`, `replay`, `bench`; `tui/` module with one widget per area |
| `apps/dmn-desktop/` | M3: Tauri 2 app (`src-tauri/`): window options, commands, event bridge |
| `frontend/` | M3: Vite + TS; `src/views/{live,context,settings,collapsed}`; `src/tokens.css` |
| `eval/` | Python harness: fixture WAVs, scripted question lists, precision/recall + latency report |
| `testdata/` | Mock-interview WAVs and expected transcripts |

**Threading rules**

- The PipeWire main loop owns one OS thread; its process callback only copies samples into a lock-free `ringbuf` producer and returns. No allocation, no logging, no locks in the callback. The virtual microphone's playback callback (#108) is the mirror image: it pops from a lock-free ring into the output buffer, zero-fills what the ring could not supply (an underrun is silence, never a stall), and returns; the thread that writes speech into the ring resamples and waits for room, never the callback.
- VAD runs on its own thread per source, draining the ring buffer in 20 ms frames; a full buffer drops the incoming 20 ms frame (the lock-free producer never touches the consumer's read index) and increments a counter the status bar reads.
- One `whisper-rs` `WhisperState` per source on a dedicated worker thread; inference is serialised per state. Workers receive `Utterance`s over a bounded `crossbeam` channel and publish `Segment`s to the transcript bus.
- The detector, context assembler and LLM stage run as tokio tasks. The LLM task holds a `CancellationToken` per request and cancels the in-flight one when a newer unpinned question arrives.
- `Session` lives behind a `tokio::sync::watch`; the Tauri bridge coalesces emits to at most 30 per second; transcript text arrives as deltas, suggestions as whole objects.
- No `unwrap`/`expect` in pipeline crates; errors are `thiserror` enums and reach the UI as status-dot states.

## Audio capture on PipeWire

Two capture streams run for the whole session: the microphone (labelled **You**) and the calling app's playback stream (labelled **Interviewer**). Because they arrive on separate pipes, speaker separation is free in the common case; diarization is only a fallback.

**Discovery** — `dmn devices` and the Settings picker both use a registry listener on the PipeWire thread (the `pipewire` crate's `Registry::add_listener`) and keep a live node table:

- Capture sources: nodes with `media.class = Audio/Source` (mic) and their `node.description`.
- App streams: nodes with `media.class = Stream/Output/Audio`, shown as `application.name` + `media.name` (for example "Firefox — Google Meet", "zoom", "teams-for-linux"). Global-added and global-removed events update the table instantly; no polling. `pw-dump` is used only by `dmn doctor` to cross-check the table.

**Capture (M1)** — one `pipewire::stream::Stream` per source, connected as `Direction::Input`, requesting `F32LE`, 16 kHz, mono (PipeWire resamples and downmixes for us), with `Stream::connect` flags `AUTOCONNECT | MAP_BUFFERS | RT_PROCESS`. Properties:

```
media.type=Audio  media.category=Capture  media.role=Communication
node.name=dmn.<source>  node.latency=160/16000   # 10 ms quantum
target.object=<node.id or node.name of the chosen source or app stream>
stream.capture.sink=true                                   # only for the sink-monitor fallback
```

- Mic: `target.object` = the chosen `Audio/Source` node (default: PipeWire's default source, resolved from the metadata object).
- Interviewer: `target.object` = the chosen app's `Stream/Output/Audio` node, so only that app's audio is captured, not the user's own music or notifications. Fallback when no app stream is selected or found: the default sink with `stream.capture.sink=true`, and a visible "capturing all system audio" badge.
- If the app stream disappears (call ends, app restarts), the registry's global-removed event marks the interviewer source `disconnected`; the mic keeps running; when a stream with the same `application.name` reappears, a new capture stream is connected automatically.

**Speaker attribution**

1. By source (default): mic → You, loopback → Interviewer.
2. Echo guard: when both sources are active in the same 300 ms and the mic segment's text is a near-duplicate of the loopback segment (ratio ≥ 0.8), drop the mic copy. This handles laptops without echo cancellation.
3. Single-stream fallback (system audio off, or an in-person interview): all audio is "Unknown" until diarization lands (M3/M4; in-person interviews are out of scope for v1, decided 2026-09-25); question detection still runs on all text.

**Verify at M0** (`dmn doctor` checks these and prints a fix for each): `libpipewire-0.3` headers and library present; PipeWire server running and its version; a capture stream with `target.object` set to a `Stream/Output/Audio` node actually receives that app's audio on the user's PipeWire version (`doctor --probe` plays a tone through `pw-play` and captures it back); `pw-record` on PATH for the emergency fallback; mic permission under Flatpak later.

**Latency budget for this stage**: 10 ms PipeWire quantum, 20 ms VAD frames, VAD decision on 300 ms of trailing silence for a pause and 400 ms for utterance end (`[stt].vad_close_ms`; 700 ms until #16's tuning missed the 2.0 s p50 live, 500 ms until #63 chose Silero and bought its later utterance ends back), so an utterance reaches STT within about 0.5 s of the speaker stopping.

## Transcription and question detection

STT runs per utterance, not per fixed chunk: the VAD (Silero by default since #63, energy as the no-model alternative) closes an utterance after 400 ms of silence (or at 12 s, whichever comes first) and whisper transcribes it whole, with the previous utterance's text passed as prompt for continuity. Partial results every 1 s keep the transcript strip moving while a long utterance is still open.

**STT worker**

- Input: 16 kHz mono f32; `whisper-rs` `FullParams` with `SamplingStrategy::Greedy`, `language=Some("en")`, `no_context=true`, `single_segment=true`, `temperature=0`, `suppress_blank=true`, `n_threads` from config. With `[stt].language` other than `en` (#77) the params change to whisper's translate task: `auto` passes `language=None` on an utterance's first inference so whisper detects the language from the encoder output, and the worker pins what it detected for that utterance's later partials and final (detection is paid once per utterance, not once per partial); a code such as `es` passes it fixed. Either way the decoder emits English, so the detector, the prompts, the six sections and the echo guard run on English text and are unchanged. The one exception is the You lane while `[speak]` is on (#109): it detects its language the same way (once per utterance, pinned) but runs whisper's transcribe task, not translate, so `t` translates the user's own words rather than whisper's English rendering of them; with `language = "en"` nothing changes. A mic that hears the call (no headphones) then puts the interviewer's untranslated words on the You lane, where the echo guard, which compares them against the English loopback, cannot match them. Only the multilingual `tiny`, `base`, `small`, `medium` and `large-v3` can do this: a `.en` model, or `large-v3-turbo` (trained without translation data; under the translate task it returned Spanish for the Spanish fixture on 2026-09-28, and again on a verified file on 2026-09-29: 21 of 21 interviewer finals in Spanish, #85's `large_v3_turbo_under_the_translate_task_keeps_the_spanish`), with a language other than `en` is refused by `dmn doctor` and at start, naming the fix. Measured 2026-09-26 on the dev box (Ryzen 7700X, 8 threads, CPU, `small`, Silero at 400 ms): the Spanish fixture `es/` scores recall 0.92 / precision 1.00 (12 of 13 questions, no false positives; the miss is a translation limit, "feature flags … release" arriving as "Fleurs … rice"), question-end → Approach p50 1.72 s / p95 1.99 s at 1× with the LLM, whisper median RTF 0.18 and mean WER 33.8 % against the English reference (the detector needs the shape of the question, not every word); 22 of 23 interviewer finals are detected as `es` (a one-word "Muy bien." is not). The English fixtures under `auto` with `small` score recall 0.94 / precision 0.94 against 0.96 / 0.94 with `small.en`, every final but one detected as `en`, so `en` with `small.en` stays the default. `medium` translates better: recall 1.00 / precision 1.00 on `es/` (13 of 13, no false positives), mean WER 30.0 % on 15 `es/` turns against 34.7 % for `small` on the same turns (the 33.8 % above is #77's run over its own turn set), and recall 0.96 / precision 0.94 on the four English fixtures under `auto`. It costs a median 3.00 s per utterance on the CPU against 0.92 s for `small`, which alone breaks the 2.0 s budget; on the 4090 it takes 0.14 s against 0.07 s, and both lanes hold about 4.8 GB of VRAM that the local LLM does not leave, so it is the choice when the LLM is remote or small, or with assistance off (#79). `large-v3` also scores 1.00 / 1.00 on `es/` and 0.96 / 0.94 on the English fixtures, with the best text (mean WER 23.7 %; "the cache", "feature flags" and "URLs" survive), takes 0.23 s per utterance on the 4090 and 5.81 s on the CPU, and holds about 3.7 GB of VRAM per lane (7.4 GB for both). `large-v3-turbo`, which cannot translate, transcribes English at 0.08 s per utterance on the 4090 and 2.59 s on the CPU (15 `sysdesign` turns); one 1.7 s utterance there ("How many characters do you need?") came back repeated about 30 times, a whisper repetition loop the hallucination filter does not catch, and the other 14 turns average about 9.5 % WER. All of these were re-measured 2026-09-29 (#85) on model files whose SHA-256 matches the published one, after the files #77 measured on 2026-09-28 were found corrupt (#84); the new figures agree with #77's within a few points, so the damage came after those runs. #77's WER on short French, German and Portuguese clips (`medium` 18–27 %, `large-v3` 22–31 %, `small` 32–43 %) came from scratch recordings that were not kept, and has not been re-measured. Technical loanwords said with a foreign accent ("cache", "rate limiter") are the weak spot of both. The previous final goes in explicitly as prompt tokens; `no_context=true` stops whisper.cpp also prepending its own last decode, which is usually a partial of the same utterance and made the final drop the words that partial covered (#39). Only a final with sentence punctuation (`.`, `?`, `!`) becomes the next prompt: whisper copies its prompt's style, so one unpunctuated final used as the prompt left every later final on that lane unpunctuated, and the detector lost its `?` cues (#63).
- Sliding window for partials: the last 8 s of the open utterance, re-run every 1 s; the final pass replaces all partials for that utterance.
- Output: `Segment { source, start, end, text, is_final, avg_logprob, language }`; `language` is the ISO 639-1 code whisper detected (`None` on the English-only path) and rides through the session action, the store (`segments.language`, schema v8), the JSONL export and replay `--json`. The transcript line and the status line tag it when it is not `en` (`Interviewer (es→en):`, `es→en` beside the STT device). Segments with `avg_logprob < -1.0`, or fewer than 3 words that match whisper's known hallucination phrases ("Thank you.", "Thanks for watching."), are dropped.
- Model files live in `~/.cache/dmn/models/`; `dmn doctor --fetch small.en` downloads (`--fetch small` the multilingual one). Default `small.en`; `dmn bench` measures real-time factor on the target machine at M0 and records it in `doctor` output. GPU: build with `--features vulkan` (or `cuda`) and the same code path applies.

**Question detector (Tier-C, runs on every final segment of the lanes `[detect].question_source` selects; the Interviewer lane by default)**

Score = sum of matched cues; a segment becomes a candidate at score ≥ 2:

| Cue | Points |
| --- | --- |
| Ends with `?` from whisper punctuation | 2 |
| Starts with a wh-word or aux verb (how, what, why, when, which, can, could, would, tell me, walk me through, describe, explain, imagine, suppose) | 2 |
| Contains a hand-off phrase ("let's switch gears", "next question", "I'd like to hear", "talk me through") | 1 |
| Source is Interviewer | 1 |
| Longer than 6 words | 1 |
| Segment followed by ≥ 1.5 s of silence or by a segment from the other lane (for an Interviewer segment: a You segment) | 1 |
| Ends with "...right?" or "you know?" (rhetorical) | −2 |
| Mostly small talk vocabulary (weather, weekend, coffee, can you hear me) | −2 |

**Question source (#90).** `[detect].question_source = "interviewer" | "you" | "both"` (default `interviewer`; `--question-source` on `dmn run` and `dmn replay` overrides it for one run) chooses whose finals are scored and answered. It selects lanes, not voices: the mic is always labelled You and the call's output always Interviewer, and the transcript, the routing and the stored segments are the same under all three. `you` lets the user ask Daemon directly through the microphone, and lets a mock interview played from a phone into the mic drive a session; in that case the mock interviewer and the user share the You lane, so the user's own clarifying questions trigger answers too. The source is fixed for the session. The cue table is unchanged: the "Source is Interviewer" point stays the Interviewer lane's, so a You line needs one more point than the same words from the Interviewer (at `strict` a bare "Why?" from the mic is not a candidate), and the question-form gate and the rhetorical and small-talk vetoes apply to both lanes. A split question (below) continues only on its own lane; in `both`, a question on the other lane ends it and is a candidate of its own. The echo guard runs before the detector, so in `both` a mic copy of the interviewer's question is dropped and answered once; it also holds a You final up to 1.5 s (4 s while an Interviewer utterance is open), so a You question's candidate is that much later. The prompt names the lane (`## You just said`, with base rules worded for the mic lane; the Interviewer prompt is byte-identical to before), the status line shows `asks: you` or `asks: both` (nothing for the default or while assistance is off), and the listening screen waits for "the interviewer", "your question" or "a question". With assistance off nothing is detected whatever the source.

**Split questions (#63).** With a 500 ms utterance close, a question with a mid-sentence pause ("And if the limiter's state lived in Redis … and Redis went down, what then?"), a setup followed by the ask ("Imagine a delivery app. Orders change state every few seconds. How would you…?") or a 12 s forced split arrives as two or three finals, and each part that scores became a candidate of its own: two questions on screen for one. A final that starts within 1.5 s (the hand-back silence above) of the question still being asked on the same lane, with no speech from the other lane in between (judged on audio time; with the default source, an Interviewer final with no You speech in between) and within 30 s of that question's start, continues it: the detector emits the combined text (earlier candidate, the lines between, this final) as one candidate marked `continues`, the session replaces the first part's question where it is (live, pinned or queued) instead of adding or queueing a second, and the fast phase asks it without the first part as the current question, so the model does not read the whole question as a clarification of its first half. Empty finals (an utterance whisper heard nothing in) neither score nor break a question. `just eval`, energy VAD at 500 ms: precision 0.83 → 0.98, recall 0.97 → 0.96 with `continuous/` added; Silero at 500 ms: 1.00 / 1.00. The shorter 400 ms close that `[stt]` now ships splits more, and this is what keeps its precision up: Silero at 400 ms scores recall 0.96 / precision 0.94.

**Asking by key (#96).** The detector only proposes, and it can miss: a question with no question form, or a rule nobody has found yet. `r` reaches only the question already on screen, so in `dmn run` Enter makes the last thing said on the asking lane (the interviewer's, by default) the question. It sends that lane's last turn to the fast phase as one candidate that the detector never scored (score 0, no cues): that lane's most recent finals with no final from the other lane between them (judged on audio time, as for a split question: a "mm-hm" inside a line does not cut the turn; and an empty final, as there, is no speech, so it neither cuts the turn nor is one of its lines), joined in spoken order, going back at most 30 s from the turn's end. The fast phase already reduces what was just said to one question, so a turn that opens with remarks and ends with the question needs no rule of its own. The asking lane follows `[detect].question_source`; with `both` it is whichever lane spoke last. From there the candidate takes the path every candidate takes: the session, the last 60 s of transcript, what was answered before it for session memory, the slow phase, the store; under a pin it queues and the badge counts it. Enter sends nothing while assistance is off, before the asking lane has said anything, or when the turn's last line already has its question (on screen, which is what `r` is for; queued behind a pin; or still on its way from the detector). A second press on the same turn sends nothing while the first is in flight, once it was shown, or after the model declined it (the same prompt at temperature 0 gets the same answer); it sends the turn again when the first one's request failed (a timeout, an unreachable model, a malformed reply), so the key is never dead until the interviewer speaks again. A key press gets an answer either way: when the model replies `is_question=false` or `kind=small_talk` the status line says `not a question` for 5 s and the screen keeps what it had. The session records which candidates the key sent (`asked`), the notice (`not_a_question`) and a key press whose request failed (`ask_failed`, which is how the pipeline knows to send again); the pipeline thread, which has a clock, ends the notice with an action, so the reducer stays pure and the screen is still a function of session state. Typing or editing a question, choosing an older line, and the M3 window's version of the key are not part of it.

**Extraction (Tier-A, the LLM fast phase)** receives the last 60 s of transcript with speaker labels and the candidate, and returns `{is_question, question, kind, focus}` where `kind` is one of `system_design | coding | behavioral | knowledge | clarification | small_talk`. Rules:

- `is_question=false` or `kind=small_talk` → nothing changes on screen; the candidate is logged with its score for the eval harness. A candidate Enter sent (#96) is the exception: the status line says `not a question` for 5 s.
- Multi-part questions ("how would you design X, and how would you test it?") are kept as one question with both parts; the Approach covers both.
- A follow-up that only refines the current question ("and what if Redis goes down?") gets `kind=clarification` and is shown as a sub-line under the current question instead of replacing it.
- Pinned: a new question never replaces a pinned one; it is queued and shown as a small "1 new" badge on the Live Question header.

**Targets** (measured by the eval harness on scripted mock interviews): question recall ≥ 0.85, precision ≥ 0.8 at M1; recall ≥ 0.9, precision ≥ 0.9 at M2. Detection latency from end of utterance to candidate ≤ 100 ms; STT dominates.

## Context and LLM

Every suggestion is one structured JSON object produced in two phases: a fast phase that fills the Live Question and the five-line Approach, and a slow phase that fills the expandable sections. The UI never renders free text from the model; it renders fields.

**Sources** (`dmn sources add <path>` or the Context view)

- Accepted: PDF, Markdown, plain text. Resume and job description are single documents; notes and stories are folders, one file per story.
- Ingestion: text extraction → chunks of about 300 tokens with 50 overlap → stored in SQLite with source, path, chunk index. Stories additionally get a tag line (`tags: conflict, ownership`) parsed from front matter or the first line.
- Retrieval per question: BM25 over chunks (pure Rust, no vector DB in v1) with the question + focus as query; top 6 chunks, capped at 1,500 tokens. Stories are matched by tag overlap with the question `kind=behavioral` themes first, BM25 second.
- Job description is always summarised once at load into ≤ 8 requirement bullets and included in every prompt (cheap, high value).

**Session memory**

- Rolling summary: after every answered question, the slow phase appends one line ("14:31 rate limiter — covered token bucket, Redis, fail-open; promised to return to clock skew").
- The prompt carries the last 8 summary lines plus the last 60 s of raw transcript, not the whole transcript.
- "Promised to return to" items surface in Things I Should Mention on later questions until mentioned.

**Prompt contract**

System prompt (fixed per `kind`): role, the five-line rule, the JSON schema, the "no paragraphs" rule. User message: today's date (#101), job requirements, retrieved chunks, session summary, recent transcript, the extracted question. Both phases request JSON; Ollama gets the schema in `format`, OpenAI-compatible gets `response_format`, Anthropic gets a tool with the schema.

```json
{
  "question": "How would you design a rate limiter for a globally distributed API?",
  "kind": "system_design",
  "focus": "architecture, consistency tradeoffs, Redis/distributed state, failure handling",
  "lookup": "",
  "approach": ["Clarify requirements and rate-limit scope.", "...", "...", "...", "..."],
  "deeper": [{"label": "Algorithm", "text": "..."}],
  "followups": ["..."],
  "tradeoffs": [{"option": "Centralized Redis", "pro": "exact", "con": "one hop, SPOF"}],
  "example": {"language": "text", "code": "..."},
  "mention": ["..."],
  "resume_context": [{"source": "resume.pdf", "text": "..."}],
  "star": {"situation": "...", "task": "...", "action": "...", "result": "...", "story": "stories/x.md"}
}
```

Fast phase returns `question`, `kind`, `focus`, `lookup`, `approach` (and `star` for behavioral); slow phase returns the rest. Field limits are enforced in Rust after parsing: approach 3–5 items of ≤ 90 characters, `focus` ≤ 120 characters, every list ≤ 5 items, `deeper` ≤ 4 entries of ≤ 240 characters. Anything over is truncated at a sentence boundary and flagged in the debug log, never shown as a wall.

**Answers that depend on what is current (#101).** A model answers as of its training data and cannot tell how old that is: asked live for "the latest stable major version of Go", the fast phase stated a release several versions old as the current one, with a wrong date, and the slow phase built on it. So every fast and slow user message opens with `## Today` and the local date (`2026-10-05`). The suggester takes it from the clock (`date +%z` once per run for the UTC offset, so it is local time; UTC if that fails) and passes it in `FastRequest::today`; `dmn-llm` reads no clock, and its tests pin a date. Both base prompts (the Interviewer's and the mic lane's) carry one rule: when the answer depends on the current state of the world (the latest or current version or release of something, the newest standard, a license or policy now, what is supported or maintained now, who owns or runs something now), never present a remembered version, release, license or date as the current one, and fill `lookup`. The Approach is what the candidate says out loud, so the hedge is in their voice: the most recent version they have worked with, which may have moved on, and that they would confirm the current one. Naming the words to avoid ("knowledge cutoff", "training") in the prompt made the model write them (8 Approach lines across 20 flagged replies), so the rule describes the voice instead. The fast schema's `lookup` is a required string between `focus` and `approach`: empty for nearly every question (a handful of tokens before the first Approach line), otherwise a web search query written from the question that would settle it ("latest stable Go release"). Before the Approach, the model has decided before it writes the lines, and #102 can start a search while they stream. `limits.rs` keeps its first line, at most 80 characters cut at a word (no `…`: it is a query, not text on screen), and flags any cut; a reply without the field (an older model, a provider that ignores the schema) is shown as before with `lookup` empty. A non-empty query is published as soon as it closes (`Action::Lookup`), stored with the suggestion (the fast `suggestions.payload` JSON carries it; an empty one is left out, so payloads without it are unchanged), and printed by `dmn replay`. While the question on screen has one, the Approach block's border carries a dim `may be out of date` tag where `…` and `thinking…` go (`thinking…` still wins while a newer reply is on its way). A regenerate or Swap starts the question afresh, so the tag follows the newest reply; a clarification's query stays in its payload and tags nothing. The slow phase shares the system prompt, so it gets the same date and rule; `slow.md` repeats the rule for the sections, and for a question the fast phase flagged the request says so under the Approach, so no section builds on the flagged version. Measured 2026-10-05 against `qwen3.6:35b-a3b` (five runs per question, each with a different lead-in): the Go question 5/5 flagged with a hedged Approach (two of them still state a wrong remembered release date); "How does a B-tree split a full node?" 5/5 unflagged; Kubernetes, Python, Node LTS and Redis's license 5/5 flagged each, with the Approach hedged 5, 4, 5 and 0 times out of 5 (Redis's license is still stated as remembered; a clause for licenses and owners raised it to 2/5 but cost the Go, Kubernetes and Python results, so it was dropped); no flagged Approach says "cutoff", "training" or "lookup"; four stable questions 20/20 unflagged; first Approach line p50 0.94 s over those 50 requests. Looking the query up is #102, below.

**The web lookup (#102).** The user's choice, in their words: "local first but to search with web when necessary". With `[lookup].enabled = true` (off by default), a fast reply with a non-empty `lookup` has its query sent to a web search the moment the field closes, while the Approach streams: the local model answers every question first, on budget, and only a question it flagged is searched. The search runs as a task on the suggester's own current-thread runtime, so it makes progress while the Approach streams and while any later request runs, and it never sits in front of a fast phase: the thread takes a waiting job first, then acts on a finished search, waits for one still running (woken at once when a job arrives), and only then runs slow phases, so the model is never kept idle by a search for longer than `[lookup].timeout_ms`. The query is the only thing sent: `POST <base_url>/api/web_search` with `{"query": "<lookup>", "max_results": N}` and `Authorization: Bearer <key>` (Ollama's hosted web search, <https://docs.ollama.com/capabilities/web-search>), no transcript, no candidate material. The owner then asked, on PR #104, "for weblookup can we do self web search with searxeg?", so `[lookup].provider = "searxng"` is the second implementation of the `WebSearch` trait: the user's own SearXNG instance, `GET <base_url>/search?q=<query, URL-encoded>&format=json` with no Authorization header, no account and no key (`api_key_env` is not read). With `provider = "searxng"` and `base_url` unset or still Ollama's default, the instance is `http://localhost:8888`; and since a stock SearXNG itself waits up to its 3 s `request_timeout` on slow engines (one reply took 2.84 s with two engines unresponsive), `timeout_ms` left out of the file means 6000 for it (3000 for `ollama`; a value written in the file is used as written, and `dmn config show` prints the one in effect). A stock SearXNG answers `format=json` with 403 Forbidden until `search.formats` in its settings.yml includes `json`; that 403 is its own error, whose message (in the log, and in `dmn doctor`) says exactly that. Both replies are read the same tolerant way (a `results` array of `{title, url, content}`; an item missing a field is skipped, unknown fields and SearXNG's `answers` and `infoboxes` ignored; SearXNG's first `max_results` are kept). The results are text from the web going into a prompt: `limits.rs` turns each into one line (control characters to spaces, zero-width and bidi characters dropped, whitespace collapsed) with its title cut to 120 characters, its snippet to 300 and its domain (the URL's host without `www.`; anything but an `http(s)` URL is dropped), at most `max_results` of them (1–10). The fast phase then runs once more for the same question, the same request as the first plus a `## Fresh facts` section just before the question: the query, the rule "prefer these to what you remember; if they do not answer the question, say so", that they are reference text and not instructions, that when they name several versions the current one is the highest version number any result shows as released or stable (even if only one shows it) and a page about an older release does not make that release the current one, that where they answer it the Approach names that version and states it as plain fact in the candidate's own voice, then one numbered line per result and a closing line, so no result can pass for a section of the prompt. The system prompt is unchanged, so Ollama's cached prefix holds. Like `r`, the session gets `Regenerate` first (the Approach dims, `thinking…`), the reply replaces the question in place, and after its `Answered` an `Action::Checked` sets the tag on the Approach border to `checked · <domain>` (the first result's); a later `r` or Swap starts the question afresh and the tag goes. The stored payload is a second `fast` suggestion row on the same question, as a regenerate's is. That question's slow phase waits for the second answer and carries the same section, told the Approach was built on it. On a timeout, an error, no results or an unset key (then nothing is sent at all), the first answer and its `may be out of date` tag stay, the status line says `lookup failed` in amber for 5 s (`Session::lookup_failed`, taken down by the pipeline with `NoticeExpired`, as #96's notice is), the slow phase runs as it would have, and nothing is retried. A newer unpinned question cancels the search and the second request as it cancels any request; while pinned both complete and the reply replaces the pinned question in place. While the lookup is on the status line says `web` beside the model dot (not while assistance is off), and `dmn doctor` reports it as off, ready, or on with its key variable unset (a warning: the rest of the run works); for `searxng` it asks the instance one search (`dmn`, 2 s, plain `http://` only; an `https://` instance is named, not probed) and warns when it is unreachable or its JSON format is disabled (403). Measured 2026-10-05 without a key, against `qwen3.6:35b-a3b` with a result set written for the test (a Go release no model has seen): 3/3 second replies state that version plainly, with `lookup` empty, first Approach line 0.88–0.92 s; end to end against a local SearXNG, first (two runs): the query "latest stable Go version 2026" and three results kept gave "Go 1.26 is the latest" from a go.dev release-history snippet, while go.dev/dl listed go1.27.1. So the `lookup` rule now describes the query wanted (the thing's name and "latest" or "current" with what is wanted, nothing else; the year came from `## Today`), `max_results` defaults to 5, and the Fresh facts rule gained the sentence on several versions. Measured after that (2026-10-05, five runs per question, each with a different lead-in, `qwen3.6:35b-a3b`, the truth read from each project's own page that day; scored on the first version the corrected Approach names): Go (go.dev/dl: go1.27.1) 4/5, Kubernetes (dl.k8s.io stable: v1.37.1) 4/5, Node LTS (nodejs.org: v24, Krypton) 5/5, Python (python.org: 3.14.8) 5/5; queries "latest stable Go version", "current stable Kubernetes release version", "current Node.js LTS version", "latest stable Python release", none with a year; tags `go.dev`, `kubernetes.io`, `nodejs.org`, `devguide.python.org`; question to the corrected Approach's first line p50 2.1 s (Go, Kubernetes) and 4.2–4.7 s (Python, Node, whose searches took 2.6–3.0 s). The misses: once in five the Go answer still calls 1.26 the latest and 1.27 "in development", with go.dev/dl's "Stable versions · go1.27.1" in its results; once in five the Kubernetes answer names no version. The second request's prompt is about 6,300 characters with five results, well inside `num_ctx` 8192. Ollama's hosted search has not been called from this machine (no key).

**Speaking your words in the interview's language (#109).** `dmn speak` (#107, #108) speaks given text in the user's cloned voice into `dmn-voice`; in `dmn run`, `t` does it with what the user just said. With `[speak].enabled` and `[speak].target` (a code such as `es`; no default, and `enabled = true` without it is a config error naming the key), a press takes the You lane's last turn (its finals since the Interviewer last spoke, at most 30 s back, as Enter's turn is built for the asking lane, #96), translates it, synthesizes it and plays it. **Flow.** The pipeline thread dispatches `SpeakStarted` and hands the turn to the session's speaker, a worker thread with its own runtime, so a press neither waits on nor holds up the fast and slow phases. The translation is one request through the `[llm]` provider (the same model as the answers unless `[speak].model` names one, #113; `think: false`, temperature 0): the system prompt says "translate the user's message verbatim into Spanish (es); output only the translation" and the user message is the turn and nothing else; the reply is read whole, within the fast phase's timeout (4 s), and `limits.rs` makes it one line of at most 600 characters (`TRANSLATION_CHARS`, cut at a sentence end, else a word with `…`). Then `SpeakTranslated` carries it to the screen, and the TTS server synthesizes it on its streaming route in the enrolled voice, with `target` as the request's language; the speech plays into `dmn-voice` as it arrives, and `SpeakDone` ends the press when the last sample has played. A failure at any step (a translation that timed out or failed, a TTS server that went away, a voice no longer enrolled, speech nothing pulled) ends the press with `SpeakFailed` and its reason, shown in amber in front of the keys for 5 s and taken down with `SpeakNoticeExpired` by the pipeline thread, which has the clock; it never panics and nothing is retried. **Limits.** One press at a time: `t` while one is in flight does nothing. Before the user has said anything it says `nothing to speak` and sends nothing. It does nothing while `[speak]` is off, and works with assistance off (it is a speech feature, not an answer feature; with `[speak]` on and no `[speak].model`, a run that starts with assistance off does not unload the local model, which the speaker warms at start; #113 below gives translation a model of its own). **Screen.** The keys end with `t speak` while `[speak]` is on; the status line says `translating…` and then `speaking…`, each with the whole seconds since the key (`SpeakElapsed`, from the pipeline thread's clock), in the You lane's colour; and the translation is one dim line (`» …`) at the foot of the question block while it is spoken (above the status line on the transcript-first screen), the question giving up a row for it. All of it is session state, so snapshots pin it. **Node.** The speaker opens `dmn-voice` when the run starts and keeps that one node for the session, so the call can select it before the first press and keeps it between presses; a press whose speech nothing pulled says so and reopens the node, so unheard speech is not left queued for the next press. **Echo window.** From the first sample PipeWire pulls to 300 ms after the last, the echo guard drops what either lane heard, on audio time, belt and braces: a virtual source is neither the mic nor the call's output. **Start-up.** Before any audio moves, `dmn run` checks that the TTS server is on this machine and answering, that the enrolled voice is in its library, that whisper knows `target`, and that the voice was enrolled in `target` (`GET /v1/voices` lists each voice's `language`; the server speaks a voice only in that language and ignores the request's): otherwise it refuses to start, naming both languages and the fix (`dmn speak enroll --wav <clip> --language es`). One enrolled voice per language. **Measured** 2026-10-06 on the dev box (Ollama `qwen3.6:35b-a3b` on the 4090, chatterbox-tts-api on the CPU): translation alone 0.35–0.49 s for five one-sentence turns (p50 0.37 s), each verbatim with nothing added; end to end for "I would start with a token bucket for each API key and keep the counters in Redis.", key to translation 0.36–0.40 s, key to the first sound in `dmn-voice` 18.0–18.7 s, key to the last 24.5–25.5 s (6.5 s of speech). The wait is the CPU TTS server's.

**Translation only, and the TTS on the GPU (#113).** The user's words after the first live Discord test: "i just want translation so no ai helper". Two modes, by what is resident: **assistance on** holds the answer model (`[llm].model`, `qwen3.6:35b-a3b`, about 22 GB) and, with `[speak].model` set, the translator too, whisper and the TTS server taking what is left (with the TTS server on the 4090 the answer model sits 74 % on the GPU and the status line says `llm partly on cpu`); **translation only** (assistance off at start and `[speak].model` set, `llama3.1:8b` on the dev box) holds just the translator (5.4 GB with `num_ctx` 8192), whisper and the TTS server (about 4 GB): the run asks a local Ollama to drop `[llm].model` before whisper loads, as #79 does, and never warms it; Ctrl+Shift+A starts the suggester, which loads it then. `[speak].model` is optional and is only the model name: the translator uses `[llm]`'s provider, endpoint, key, `num_ctx` and fast-phase timeout (`Config::translator_llm`); left out or blank, or equal to `[llm].model`, translation runs on `[llm].model` as in #109, and nothing changes. With whisper on the GPU every wanted model is loaded before whisper (#61). **Warm-up.** The speaker loads the translator when the run starts and, on a thread of its own so the first press never queues behind it, asks the TTS server for one word (`OK.`, in the voice, target and knobs a press would use) whose audio is discarded; the time goes to the log. The first request after the GPU server starts took 8.9 s against 1.9 s warm. **Sentence by sentence.** `dmn-tts` asks the server for one sentence at a time (the server's own split, after `.`, `!`, `?` or `…` and whitespace, a piece under 20 characters joined to a neighbour), all on the streaming route into one stream, so a long turn's first sentence plays while the rest render; for a three-sentence German turn the first sound came 1.2–1.5 s after the request against 4.1–4.2 s for the whole turn in one. **The trailing-silence rule.** Chatterbox sometimes ends a generation with a second or more of silence and then a loud burst (its end-of-text hallucination). In each sentence's decoded stream, once 0.2 s of 20 ms windows peaking at or over 0.02 have been heard (speech), 0.8 s of windows under 0.02 ends that sentence: 0.3 s of the silence is kept, everything after it is discarded and the rest of the body is not read; a pause is held back, not dropped, and goes out unchanged when speech resumes. Measured 2026-10-08 on the GPU server (German in a test voice, Russian in the user's): the server's silence floor is 0.002–0.005 and speech peaks 0.2–1.0; inside single sentences the longest pause before the end was 0.64 s, while longer silences (0.82–3.06 s) were all tails, followed by nothing or noise up to 0.12; but inside one generation of two sentences the model paused up to 0.98 s between them, which is why the rule runs per sentence. The rule is in `dmn-tts`, so `dmn speak --out` and the mock get it too. `[speak].temperature` (0.6 by default, the server's own being 0.8) is sent with every speech request, and `cfg_weight` and `exaggeration` when set, within the server's ranges (0.05–5.0, 0.0–1.0, 0.25–2.0; outside them the config is refused). **`dmn-voice` across a restart.** Each run creates a new node (same `node.name`, new `object.serial`). WirePlumber relinks a stream to it only when the stream names its target by name in its own properties (`target.object = "dmn-voice"`: `pw-record --target`, a PulseAudio app opening the device by name), and only if the graph stayed quiet meanwhile: the stream waits on the default source, and the first rescan (any stream that comes or goes) marks it to follow the default from then on, as PulseAudio did (`lutils.checkFollowDefault`); a stream with `node.dont-reconnect` (a PulseAudio app that asked not to be moved, which Chromium-based apps such as Discord do for a chosen device) is never linked again once handled (`linking/prepare-link.lua`), and a stream moved by hand is kept in the `default` metadata by serial. A fixed `object.path`, `node.virtual`, `device.description` or `node.link-group` changed nothing (`just probe`: `live_a_reopened_dmn_voice_is_not_reliably_relinked`), and keeping the node in a long-lived helper would leave a device behind, which #108 ruled out. So 2 s after the node opens, if nothing is linked to it, the status line says `Discord: re-select dmn-voice as the microphone` for 5 s (a `SpeakFailed` under id 0, which no press has), and a press nothing pulled says the same. `dmn doctor` names the translator and whether Ollama has it, and the TTS server's `device` from its `/health`, with the way to the GPU when it is `cpu`. **Measured** 2026-10-08 (TTS server on the 4090, `[speak].model = "llama3.1:8b"`, `[llm].model` not loaded): "I would start with a token bucket for each API key and keep the counters in Redis." into German, key to translation 0.34–0.40 s, key to the first sound in `dmn-voice` 2.5–2.8 s, key to the last 8.1–9.1 s; a three-sentence turn 1.8–2.1 s to the first sound. Translation alone on 10 sentences, p50: `llama3.1:8b` 0.30 s, `qwen3.6:35b-a3b` 0.68 s (beside the TTS server, 74 % on the GPU); the small model's German had grammar slips in about half the sentences and its Russian two meaning errors, the large one's none in German and one added phrase in Russian.

**Providers**

| Provider | Endpoint | Streaming | Structured output | Notes |
| --- | --- | --- | --- | --- |
| Ollama (default) | `http://localhost:11434/api/chat` | yes | `format` = JSON schema | `keep_alive` set so the model stays loaded for the session; warm-up request at start |
| OpenAI-compatible | `<base_url>/v1/chat/completions` | yes | `response_format` json\_schema | Covers OpenRouter, vLLM, LM Studio, OpenAI; API key from config or env |
| Anthropic | `https://api.anthropic.com/v1/messages` | yes | tool use with the schema | API key from env |

Timeouts: fast phase 4 s, slow phase 20 s; on timeout the UI keeps the previous section content and shows a small "slow" tag. A remote provider is used only when `[llm].provider` is not `ollama`, and Settings states in words that transcript text leaves the machine.

## UI spec

The window is 440 px wide, 760 px tall by default, frameless with a 36 px custom title bar, always-on-top where the compositor allows, and it never grows on its own. The [canvas](https://claude.ai/artifact/Kya3g5rdSzQ7k9tGPqZV2B) is the visual source of truth; this section states the behavior.

**Terminal MVP (M1–M2)** renders the same three areas in a ratatui TUI, sized for a 100 × 30 pane beside the call: question + focus in a top block (2–4 lines), the numbered Approach in the middle, the last two transcript lines and the mic / system / model status in a one-line footer. On a pane of 120 columns or more (#88) the transcript is a right-hand column instead, 40 % of the width and the full height above the footer, every line of the last 60 s wrapped with the newest at the bottom, while the question, the Approach and the sections keep the left; the footer stays full-width, and narrower panes keep the stacked layout. Keys: `p` pin, `r` regenerate, `1`–`6` toggle one section, `m` mute mic, `s` coding solutions hints ↔ full, `w` swap the STAR story (#58), Ctrl+Shift+A interview assistance on ↔ off (#79), Enter ask about what the asking lane last said (the interviewer, by default), for a question the detector missed (#96; § Question detector, Asking by key), `←` / `→` step back to an earlier question and forward again, `↑` / `↓` and PgUp / PgDn scroll the open section (#94), `t` speak what you last said in the interview's language in your own voice (#109, while `[speak]` is on; § Context and LLM, Speaking your words), `q` quit. While assistance is on the footer's keys end with `⏎ ask`; for 5 s after the model answered a key press with "not a question" they are led by `not a question` in amber, and the rest of the screen keeps what it had; for 5 s after a web lookup came to nothing (#102) they are led by `lookup failed` the same way. While the web lookup is on, `web` sits dim beside the model dot. On a pane too narrow for both, `⏎ ask` gives its cells to a status note that would otherwise be cut (an error's reason, or "transcript leaves this machine", which #59 keeps whole at 80 columns); `not a question` never does, and for its 5 s the other keys go as well if the note needs them. The session keeps every question it has shown (#94), in order, each as it stood when the next one replaced it: its focus, Approach, STAR cards, clarifications and sections. The history is in memory only and the store is unchanged; a question that was live behind an earlier one is kept, one that queued behind a pin and was dropped at unpin is not. `←` shows the question before the one on screen and `→` the one after, which from the newest earlier one is the live question again. An earlier question holds the screen the way a pin does: the title says where it is (`QUESTION 2 of 5`), questions that arrive meanwhile are answered behind it and the footer's keys are led by their count in amber (`2 new`), and the newest is on screen when the user returns to live, where a pin and its queue are as they were. It is read-only: `1`–`6` open its sections, `p`, `r` and `w` act on the live question only and do nothing, and a section whose slow phase the next question cancelled shows what had arrived and is not requested again. Enter still asks about what was just said; its answer arrives behind the earlier question like any other. An open section taller than its rows scrolls: `↑` / `↓` by a row, PgUp / PgDn by the rows on screen, stopping at both ends, and its last row says where it is (`↓ 6 more`, `↑ 3 · ↓ 3 more`, `↑ 6`) in place of the `…` a cut body used to end with. The offset goes back to the top when the section closes or another question takes the screen, and a body that fits is drawn as before. The mouse works too (#95, `[ui].mouse`, on by default): the wheel scrolls the open section three rows a notch while the pointer is on the left column (the question, the Approach and the sections), and a left click on a section row (`1 ▸ Deeper Answer` …) does what its number key does, on the question on screen, an earlier one included; clicks elsewhere, drag, right click, hover and the wheel with no section open do nothing, and there are no click targets for history or the transcript. Capture takes text selection away from the terminal (WezTerm still selects with Shift+drag), so `mouse = false` enables no capture and reads no mouse event. Capture is released on every exit path (`q`, Ctrl+C, an error, a panic). Inside the body every item wraps onto as many rows as it needs, so scrolling reaches the last word of a 240-character item at any width; only a row that cannot wrap is cut to its width with `…` (a line of code, which is not re-wrapped, and a code block's `Name · language` line). The footer names these keys when they apply: `← earlier` before `⏎ ask` once there is an earlier question (the next after `⏎ ask` to give its cells to a status note), `↑↓ scroll` in place of the open section's close key while its body is cut, and on an earlier question `← earlier · → later` (`→ live` from the newest), then the sections' key and `⏎ ask`. The same density rules apply: no paragraph over 3 sentences, one section open at a time, nothing auto-expands. With assistance off the TUI has a second layout, transcript first: no question block and no Approach, the transcript wrapped over every row above the status line (the session's last 60 s, newest line at the bottom), and the status line says `assist off` beside the STT device and any `es→en` tag, with `ctrl+shift+a assist · q quit` as its keys. Everything below describes the M3 window.

**Window**

- Min size 380 × 520; the AI Assistance area is the only scrolling region.
- Remembers position and size per monitor layout in config.
- Collapsed pill (⌥H): 440 × 92, shows the current question in one line and which Approach point the user is on ("3/5") based on the last You segment matched against approach items.
- Opacity setting 70–100 %, default 100 %.
- Wayland: GTK window with `xdg-toplevel`; X11: `keep_above` + `skip_taskbar`.

**1. Live Question**

- Label states: `LIVE QUESTION` (teal dot) / `PINNED QUESTION` (amber) / `LISTENING` (no question yet, shows the last interviewer sentence in muted text).
- Question text: 17 px, at most 3 lines; longer questions are ellipsised with the full text on hover and in the transcript.
- Focus line: ≤ 120 characters, hidden when the model returns none.
- Pin: click or ⌥Space; while pinned, new questions queue and a "1 new" badge appears; unpin shows the newest.
- Clarification follow-ups render as a second 13 px line under the question with a ↳ glyph.

**2. AI Assistance**

- Approach: 3–5 numbered lines, each ≤ 90 characters, visible within 2.0 s of the question ending. While loading, the previous Approach dims to 50 % rather than clearing.
- Six expandable rows in fixed order: Deeper Answer, Follow-up Questions, Tradeoffs, Example, Things I Should Mention, Resume Context. Shortcuts ⌥1–⌥6. Exactly one open at a time; opening another closes the first. Rows whose slow-phase content is not yet available show a subtle "…" hint; rows with no content for this question are hidden.
- Behavioral questions replace the Approach block with the STAR block (S/T/A/R cards, Action card emphasised, matched story path in the header, Swap picks another story).
- Coding questions show Approach + Example (language from the job description or config). What Deeper Answer holds follows the coding-solutions mode (#50): **hints** (the default) gives the key insight, the data structure, edge cases and pitfalls, and time/space complexity, with no complete code; **full** adds a worked solution in the job's language, capped in `limits.rs` (≈ 40 lines). Either way it stays collapsed until opened. The mode comes from `[ui].coding_solutions`, `dmn run --solutions <hints|full>` overrides it for one run, and `s` toggles it during a call; the footer shows `s: hints` / `s: full` while a coding question is on screen, and each stored question records the mode it was detected under.
- Redo regenerates with `temperature=0.7` and the note "user asked for a different angle".

**3. Transcript & status**

- Last two segments, one line each, speaker label in mono; the open utterance shows partial text with a trailing cursor.
- Status dots: mic, system audio (with app name), model. Red dot + tooltip on failure; the dots are the only place errors appear during a call.
- Footer shows the two most useful shortcuts for the current state.

**Keyboard**

| Shortcut | Action |
| --- | --- |
| ⌥Space | Pin / unpin |
| ⌥1 … ⌥6 | Toggle section 1–6 |
| ⌥H | Collapse to pill / expand |
| ⌥R | Regenerate |
| ⌥M | Mute mic capture (still transcribes interviewer) |
| ⌥, | Settings |
| ⌥C | Context |

Shortcuts are registered inside the window (WebKit key events), not globally; a global hotkey for ⌥H is an M3 item because it is compositor-specific on Wayland.

**Density rules** (enforced in the front end, not left to the model)

- No paragraph longer than 3 sentences anywhere in the live view.
- No section content taller than the window: overflow scrolls inside the section.
- Type scale: 17 / 13.5 / 12.5 / 11 / 10.5 px; mono for labels, timestamps, shortcuts and code only.
- Colors: graphite ground `#121417`, panel `#15181c`, border `#22262d`, text `#e8eaed`, muted `#9aa3ad`, accent teal `#5fd3a6`, pinned amber `#e8b04f`. Contrast ≥ 4.5:1 for all text.
- No toasts, no modals, no animations longer than 150 ms during a call.

## Data, config and privacy

Everything lives under XDG paths, the call's audio is never written to disk unless the user turns on session recording (`dmn speak` writes only the files its own command line names, below), and the only network calls are to the configured LLM endpoint, to model downloads the user starts, with the web lookup on (#102) the search query to `[lookup].base_url`, and with `dmn speak` (#107) the text and the enrolled clip to the TTS server on this machine at `[speak].tts_url`; `t` (#109) sends the user's last turn to the configured LLM endpoint for translation and the translation to that TTS server.

| Path | Contents |
| --- | --- |
| `~/.config/dmn/config.toml` | All settings; created by `dmn doctor --init` with comments |
| `~/.local/share/dmn/sources/` | Copies of ingested resume, JD, notes, stories (originals untouched) |
| `~/.local/share/dmn/dmn.sqlite` | Sessions, segments (with the detected language, #77), questions, suggestions, chunks, summaries |
| `~/.local/share/dmn/sessions/<id>.jsonl` | Optional export of a session's transcript + suggestions |
| `~/.local/share/dmn/<name>.wav` | The voice clip `dmn speak enroll --wav` registered (#107; `voice.wav` by default), named by `[speak].voice_ref` |
| `~/.cache/dmn/models/` | whisper ggml files |
| `~/.local/state/dmn/dmn.log` | Structured log (tracing, JSON), rotated at 10 MB |

```toml
[audio]
mic = "default"                      # pw node.name or "default"
system_audio = true
app_stream = "Firefox"               # application.name match; "" = sink monitor
echo_guard = true

[stt]
engine = "whisper.cpp"
model = "small.en"
threads = 8                          # per lane; 4 missed the 2 s p50 (#15)
vad = "silero"                       # silero | energy (#63: fixtures and keyboard noise)
vad_close_ms = 400                   # silence that ends an utterance; shorter = faster, more splits
device = "cpu"                       # cpu | gpu | auto: where whisper runs (gpu needs --features vulkan|cuda)
language = "en"                      # en | auto | es, de, ...: auto detects and translates to English; needs a multilingual model (small)

[detect]
sensitivity = "balanced"             # eager | balanced | strict
question_source = "interviewer"      # interviewer | you | both: whose questions are detected and answered

[llm]
provider = "ollama"                  # ollama | openai | anthropic
model = "qwen3.6:35b-a3b"           # measured 2026-09-21 on the dev 4090; see CLAUDE.md
base_url = "http://localhost:11434"
api_key_env = ""                     # name of an env var; never the key itself
answer_length = "concise"            # concise (5) | minimal (3)
fast_timeout_ms = 4000
slow_timeout_ms = 20000              # 0 turns the slow phase off
num_ctx = 0                          # Ollama context window to ask for; 0 = the server default

[lookup]
enabled = false                      # true = search the web when an answer depends on what is current
provider = "ollama"                  # ollama (hosted, key) | searxng (your own instance, no key)
base_url = "https://ollama.com"      # searxng: left as is, http://localhost:8888
api_key_env = "OLLAMA_API_KEY"       # name of an env var; never the key itself
# timeout_ms = 3000                  # left out: 3000 for ollama, 6000 for searxng
max_results = 5

[assist]
enabled = true                       # false = transcribe / translate only (Ctrl+Shift+A toggles)

[speak]
enabled = false                      # `dmn speak` (#107): clone-voice TTS through a local server
tts_url = "http://localhost:4123"    # chatterbox-tts-api (OpenAI-compatible); this machine only
voice_ref = ""                       # enrolled reference clip; `dmn speak enroll --wav <clip>` sets it
device = "cpu"                       # cpu | cuda: a hint for the server setup; check nvidia-smi first
target = ""                          # `t` (#109) speaks in this language (es, de, ...): the voice's enroll --language
# model = "llama3.1:8b"              # `t`'s translator (#113); left out: [llm].model
temperature = 0.6                    # TTS sampling temperature (#113); lower = fewer noise bursts
# cfg_weight = 0.5                   # TTS pace, 0.0-1.0; left out: the server's
# exaggeration = 0.5                 # TTS emotion, 0.25-2.0; left out: the server's
mode = "passthrough"                 # passthrough (your voice live into dmn-voice) | translate (every turn translated; `l` toggles, #114)

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above
opacity = 1.0
never_auto_expand = true
coding_solutions = "hints"           # hints | full (worked solutions in Deeper Answer)
mouse = true                         # dmn run: wheel scrolls an open section, a click opens one

[privacy]
record_audio = false                 # write WAVs of the session
keep_transcripts = true
```

**Privacy rules**

- Audio buffers are in memory only; `record_audio=true` writes WAVs under `sessions/` and shows a red REC dot. That rule is about the call. `dmn speak` (#107) writes audio only where its own command line asks: `dmn speak "text" --out <file>` writes the synthesized speech to that file, and `dmn speak enroll --wav <clip>` keeps a copy of the user's own clip at `~/.local/share/dmn/<name>.wav`. Neither records anything from a call, and without `--out` nothing is written: the speech goes to the `dmn-voice` virtual microphone (#108) in memory, and the node is gone when the command ends.
- Synthesized text goes only to a TTS server on this machine: `dmn speak` and `dmn run` with `[speak]` on refuse a `[speak].tts_url` whose host is not a loopback name or address. `t` (#109) sends the user's last turn to the `[llm]` provider to translate (Ollama on this machine by default; with a remote provider the utterance leaves the machine exactly as the transcript window already does), and its translation to that TTS server. `dmn speak` sends that server the text; `dmn speak enroll` uploads the clip once, and the server (chatterbox-tts-api) keeps its own copy in its voice library until the user deletes it there (`DELETE /v1/voices/<name>`).
- What leaves the machine, and when: audio never. Transcript text only with a remote model: with any provider but `ollama` on this machine, the exact payload (transcript window, retrieved chunks, summary) goes to that provider; Settings says so in one sentence and `dmn doctor` prints the provider in use. A web search query only with the web lookup on (`[lookup].enabled`, off by default, #102): the few words of the fast phase's `lookup` (at most 80 characters, written by the model from the question); nothing from the transcript, the candidate's material or the session goes with it. With `provider = "ollama"` it goes, with the number of results and the key from `[lookup].api_key_env`, to Ollama's web search (`https://ollama.com` by default). With `provider = "searxng"` it goes to the user's own SearXNG instance (`http://localhost:8888` by default), with no account or key, and that instance forwards it to the search engines it is configured with. With `provider=ollama` and the lookup off, nothing leaves the machine. While the lookup is on the status line says `web`, `dmn run --headless`, `dmn replay` and `dmn config show` say what is sent and where, and `dmn doctor`'s header says so.
- API keys (the remote model's, and the web lookup's) are read from the named environment variable at start and never written to config, logs or the database.
- `dmn sessions purge <id|all>` deletes rows and files; there is no cloud copy to purge.
- No telemetry, no update checks; `dmn version` compares against GitHub releases only when asked.

## Milestones

The MVP is M1: a terminal program (`dmn run`, a small ratatui TUI) that takes a live call in and puts a five-line Approach out, within budget, on real PipeWire audio. The desktop window from the canvas is M3. Every milestone ships a usable tool; the pipeline crates never change shape between the terminal and the window, only the surface does.

| Milestone | Surface | Scope | Done when |
| --- | --- | --- | --- |
| M0 — Scaffold | CLI | Cargo workspace, config loader, `dmn doctor` (libpipewire, PipeWire server + probe, whisper model, Ollama reachability, GPU backend), `dmn devices`, `dmn bench`, fixtures in `testdata/`, CI running `cargo test` on Ubuntu 24.04 | `dmn doctor` passes on the dev machine; CI green |
| M1 — MVP | Terminal TUI | Mic + app-stream capture via `pipewire` streams, energy VAD, `whisper-rs` workers, rule detector, LLM fast phase (Ollama), SQLite session log, `dmn replay <wav> <wav>`; `dmn run` draws the three areas in the terminal: question + focus (top), Approach (middle), last two transcript lines + status (bottom); keys: `p` pin, `r` regenerate, `q` quit | On the 3 fixture interviews: recall ≥ 0.85, precision ≥ 0.8, p50 question-end → Approach ≤ 2.0 s, p95 ≤ 3.5 s with `small.en`; 60-minute live Meet call in a terminal beside the call without crash or ring-buffer overrun; CPU average ≤ 1 core while idle-listening |
| M2 — Context | Terminal TUI | Slow phase + six expandable sections (keys `1`–`6`, one open at a time), sources ingestion (PDF/MD/TXT), BM25 retrieval, session memory summariser, STAR mode with story matching, OpenAI-compatible + Anthropic providers, Silero VAD via `ort`, echo guard; `dmn sources` and `dmn config` subcommands stand in for the Context and Settings views | All six sections fill within 20 s; a behavioral fixture question yields a STAR block with a matched story; provider switch via config reload without restart; recall ≥ 0.9, precision ≥ 0.9 |
| M3 — Desktop window | Tauri 2 | The canvas design: floating 440 px window with Live Question, AI Assistance, Transcript & status; Context and Settings views; collapsed pill; keyboard map from the UI spec; X11 keep-above; `dmn run --gui` and a `.desktop` entry. The TUI stays as `dmn run --tui` | Window matches the canvas; every M2 feature reachable in the window; same eval numbers as M2 |
| M4 — Polish and ship | Both | Always-on-top per compositor + `force_x11` + optional `gtk-layer-shell`, app-stream hot re-attach, diarization fallback, global ⌥H hotkey where possible, opacity, AppImage + deb, Flatpak manifest, eval report in CI, README with a 5-minute setup | AppImage runs on a clean Ubuntu 24.04 and Fedora 42 VM; eval report published per commit |

**Fixtures for M1** (created in M0, checked into `testdata/`, generated with two TTS voices so they can be redistributed): three 10-minute mock interviews — system design, behavioral, mixed with small talk — each as two WAVs (interviewer, candidate) plus a `questions.json` listing every real question with its timestamp. `dmn replay` feeds both WAVs through the real pipeline at 1× or faster and writes the same session log a live call would.

**Order inside M1** (each step leaves `main` working)

1. `dmn-audio`: `WavSource` + `PipeWireSource` behind `AudioSource`; `dmn devices` lists nodes from the registry.
2. `dmn-vad` + `dmn-stt`: `dmn replay` prints timestamped, labelled segments to stdout.
3. `dmn-detect`: replay prints candidates with scores; eval script reports recall/precision.
4. `dmn-llm` Ollama fast phase; replay prints question + approach JSON.
5. `dmn-session` + the ratatui TUI: `dmn run` updates live; pin, regenerate and quit work; `dmn run --replay` drives the TUI from fixtures at 1× for demos.
6. Live run against a real Meet call; tune VAD thresholds and detector weights from the log.

## Claude Code handoff

Hand off four things, in this order: the repo skeleton, `docs/SPEC.md` (this doc), the fixtures, and the issues. An issue is the unit of work: one issue = one headless `claude -p` run (or one interactive session) that leaves `main` green. The earlier 10-issue list is split into 16 below so that no issue introduces more than one new crate or one unfamiliar API at a time; that is the granularity a Rust agent needs.

**What Claude Code receives**

1. A repo with the workspace scaffold, `docs/SPEC.md`, `docs/design/` (the canvas HTML), `CLAUDE.md`, `justfile`, `rust-toolchain.toml` and CI — the bootstrap checklist below.
2. `testdata/` fixtures — the one input only a human can produce.
3. Issues #1–#16 from the table below, each filled with the issue template, labelled `m0` or `m1`, with `Depends on` set as listed.
4. The kickoff prompt below for the first run; every later run gets "Implement issue #N" plus the issue body.

**Repo bootstrap (human, 15 minutes)**

- [ ] Create the `daemon` repo; `cargo new --lib` each crate listed in Architecture and `cargo new --bin apps/dmn-cli`; commit the canvas HTML as `docs/design/` for M3
- [ ] Add `docs/SPEC.md` (this doc, exported), `CLAUDE.md`, `justfile`, `rust-toolchain.toml`, `.github/workflows/ci.yml`
- [ ] Install system deps once and record the exact package list in `CLAUDE.md`: `libpipewire-0.3-dev`, `cmake`, `clang`, plus `libvulkan-dev` or CUDA if GPU inference is wanted (`libwebkit2gtk-4.1-dev` and `libgtk-3-dev` only at M3)
- [ ] Record or generate the three fixture interviews (`testdata/`), because Claude Code cannot make audio
- [ ] Open issues #1–#16 with labels `m0`, `m1`

**CLAUDE.md** (keep under 80 lines)

```markdown
# Daemon (`dmn`)
Rust Linux interview copilot. M1–M2 is a ratatui TUI; M3 adds a Tauri 2 window. Spec: docs/SPEC.md. Design: docs/design/.

## Commands (use `just`; every recipe is also runnable by hand)
- Check fast: `just check` (`cargo check --workspace --all-targets`) — run this after every edit
- Test: `just test` (`cargo test --workspace`)
- Lint: `just lint` (`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`)
- Build: `just build` (`cargo build --release -p dmn-cli`, plus `--features vulkan` when libvulkan-dev and glslc are installed; `just build cpu|gpu` forces one; M3 adds `cargo tauri build`)
- Replay a fixture: `cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav`
- Eval: `just eval` (python eval/run.py; prints recall/precision/latency)

## Rules
- The PipeWire process callback allocates nothing, locks nothing, logs nothing.
- No `unwrap`/`expect` outside tests and `main`; errors are `thiserror` enums per crate.
- Never write audio to disk unless `privacy.record_audio` is true.
- The UI renders JSON fields only; enforce length limits in `crates/dmn-llm/src/limits.rs`, not in prompts.
- No new dependencies without a line in docs/DEPENDENCIES.md saying why; no `unsafe` outside `dmn-audio` and `dmn-stt`, and every `unsafe` block has a `// SAFETY:` comment.
- Every crate's `lib.rs` doc comment states its input, output and the channel it owns.
- Tests use testdata WAVs, never a microphone; CI has no PipeWire. Anything that needs PipeWire is behind `#[ignore]` and run by `just probe` on a dev machine.
- Keep the TUI's three-area layout (question / approach / transcript+status); the one exception is the transcript-first screen while interview assistance is off (#79). The M3 window follows docs/design/.

## Done means
- `just check`, `just test` and `just lint` pass; the issue's acceptance criteria are met and stated in the PR body.
```

**Issues, in order** (acceptance criteria in each issue body come from the Milestones table)

| # | Title | Milestone | Depends on |
| --- | --- | --- | --- |
| 1 | Workspace scaffold: every crate as an empty lib with its `lib.rs` doc comment, `dmn --version`, `justfile`, CI green | M0 | — |
| 2 | Config: TOML loader, XDG paths, `dmn doctor --init`, `tracing` JSON log | M0 | 1 |
| 3 | `dmn-audio` part 1: `AudioSource` trait, `WavSource`, `ringbuf` plumbing, resampler; unit tests on fixtures | M0 | 2 |
| 4 | `dmn-audio` part 2: PipeWire main-loop thread, registry listener, live node table, `dmn devices` | M0 | 3 |
| 5 | `dmn-audio` part 3: `PipeWireSource` capture stream with `target.object`, sink-monitor fallback, `dmn doctor --probe` | M0 | 4 |
| 6 | `dmn doctor` full checks (libpipewire, model file, Ollama reachability, GPU backend) + CI: system deps, cached whisper.cpp build | M0 | 2 |
| 7 | `dmn-vad`: energy VAD + utterance segmenter, tests on fixtures | M1 | 3 |
| 8 | `dmn-stt` part 1: `whisper-rs` wrapper, model lookup/download, `dmn bench` real-time factor | M1 | 6, 7 |
| 9 | `dmn-stt` part 2: per-source worker thread, partials, hallucination filter; `dmn replay` prints labelled segments | M1 | 8 |
| 10 | `dmn-detect`: rule scorer; `eval/run.py` recall/precision on fixtures | M1 | 9 |
| 11 | `dmn-llm` part 1: `Provider` trait, Ollama client (NDJSON streaming, JSON-schema `format`), `MockProvider` for tests | M1 | 2 |
| 12 | `dmn-llm` part 2: fast-phase prompt + schema + `limits.rs`; replay prints question + approach JSON | M1 | 10, 11 |
| 13 | `dmn-session` + `dmn-store`: state reducers, `watch`/broadcast, SQLite schema + migrations; replay writes a session log | M1 | 12 |
| 14 | TUI part 1: static ratatui layout rendered from a saved `Session` JSON fixture; snapshot tests | M1 | 13 |
| 15 | TUI part 2: live wiring, keys `p r m q`, status line, `dmn run --replay` | M1 | 14 |
| 16 | Live-call tuning: VAD thresholds, detector weights, `dmn bench --live` latency report | M1 | 15 |

Two lanes run in parallel: audio (#3–#5, then #7–#10) and LLM (#11) share only #2, so a second agent can take #11 while the first is in the PipeWire code. Issues #5 and #9 are the two that touch unfamiliar C-backed APIs; expect two runs each.

**Issue template** (paste into each)

```markdown
## Goal
One sentence from the spec section.
## Scope
Crates/files touched. Out of scope: everything else, including later issues.
## Acceptance
- [ ] criterion copied from docs/SPEC.md Milestones
- [ ] `just check`, `just test` and `just lint` green
## Verify
Exact command(s) and expected output.
```

**Kickoff prompt** (first run, issue #1; later runs change the issue number)

```
Read CLAUDE.md and docs/SPEC.md in full before writing code. Implement issue #1 exactly as scoped; do not start later issues or refactor beyond it. Run `just check` after every edit, and `just test` + `just lint` before you finish. Open a PR whose body lists each acceptance criterion with the command you ran to verify it. If the spec and the issue disagree, the issue wins; say so in the PR.
```

**Headless-run notes**

- Allowed tools per run: file edit, `just check`, `just test`, `just lint`, `cargo run -p dmn-cli -- replay ...`; no network except crates.io.
- Budget: about 30 turns per issue at this granularity; #5 and #9 get two runs. Rust compile-test loops are the cost driver, so `just check` (seconds) between edits and `just test` only when check is clean.
- If a run stalls, split along the issue's own bullet list rather than merging a half-done PR.
- Human review points: after #5 (does per-app capture work on your PipeWire?), after #10 (are detector weights sane on the fixtures?), after #14 (does the TUI read in 2–5 s?), after #16 (first live call). M2 and M3 issues are opened after #16 lands, from the Milestones table.

## Open questions and risks

The two decisions most likely to change are the `pipewire` crate's low-level API for per-app capture and the whisper.cpp build inside `whisper-rs`; both have a cheap fallback and are checked in M0 before anything depends on them.

| Risk | Likelihood | Fallback |
| --- | --- | --- |
| `pipewire` crate stream/param code (SPA pods) is verbose and easy to get wrong for an agent | Medium | `doctor --probe` catches it in M0; emergency `pw-record` subprocess source behind the same `AudioSource` trait |
| `target.object` on a `Stream/Output/Audio` node does not capture that app on the installed PipeWire | Low–medium | Default sink monitor + echo guard; per-app capture via explicit `pw-link` of ports in M3 |
| `whisper-rs` cmake build fails on the distro (or GPU feature flags) | Medium | Pin whisper.cpp commit; CPU-only build first; `whisper-server` sidecar behind `stt.engine` |
| whisper `small.en` too slow on the target laptop for the 2.0 s budget | Medium on CPU-only | `base.en`, or `--features vulkan`; measured in M0 by `dmn bench` |
| Tauri's WebKitGTK 4.1 version on the distro renders the design differently | Low | Design uses only widely supported CSS; CI screenshot check in M3 |
| Local 8B model produces weak or off-schema Approach lines | Medium | Schema-constrained decoding via `format`; Rust-side limits; remote provider one config line away |
| Always-on-top silently ignored on GNOME Wayland | High on GNOME | `force_x11=true` launches under XWayland; `gtk-layer-shell` for wlroots/KDE in M3; documented per compositor |
| Whisper hallucinations on silence create fake questions | Medium | Hallucination phrase filter + VAD gate + detector score; tracked as false positives in eval |
| Echo (no AEC) makes the user's voice appear as Interviewer | Medium on laptops | Echo guard (text near-duplicate within 300 ms); recommend headphones in README |
| Agent turn budget: Rust issues take longer than the Go plan would have | High | Budgeted in the handoff; `just check` loop; split issues by pipeline step |

**Open questions**

- [ ] Target hardware for the latency numbers: which laptop, CPU-only or GPU (Vulkan/CUDA)? Sets the default model. Partly answered: M1 was measured on the dev box's CPU (Ryzen 7700X, `small.en`, 8 threads: fixture p50 1.78–1.85 s). #61 measured GPU whisper on its RTX 4090 beside `qwen3.6:35b-a3b` (sysdesign at 1×, context loaded, energy VAD at 500 ms): the LLM at Ollama's 65k context leaves 0.3–0.8 GB of VRAM, too little for whisper, and CPU whisper gives p50 1.72–1.83 s / p95 3.3–4.2 s; with `[llm].num_ctx = 8192` the LLM fits entirely and one whisper context fits beside it, so a Vulkan build puts the interviewer's lane on the GPU (`[stt].device = "auto"`): p50 1.34–1.36 s / p95 1.68–1.73 s, about 1 CPU core less. Defaults stay CPU (`device = "cpu"`, `num_ctx = 0`) so a CPU-only laptop and CI need nothing; the GPU setup is a README recipe. Still open: the target laptop without a discrete GPU.
- [x] Default local model: `qwen3.6:35b-a3b` (fast phase 0.95 s on the dev 4090; `llama3.1:8b` misclassified question kind; `qwen3.6:27b` took 3.0 s). Decided 2026-09-21.
- [x] Does the app need to work for in-person interviews (single mic, no system audio) in v1, which pulls diarization from M3 into M2? No: v1 is for online calls with two audio streams; diarization stays in M3/M4. Decided 2026-09-25.
- [x] Coding-interview mode: show worked solutions in Deeper Answer at all, or only hints and complexity notes? Hints and complexity notes by default; worked solutions on request (`[ui].coding_solutions = "full"`, `dmn run --solutions full`, or `s`), still collapsed until opened. Decided 2026-09-24 (#50).
- [ ] Session recording (WAVs) off by default is the current call; confirm.

**Sources**: [`pipewire` crate documentation](https://pipewire.pages.freedesktop.org/pipewire-rs/pipewire/index.html) (main loop, context, registry pattern), [pipewire crate on docs.rs](https://docs.rs/crate/pipewire/latest). Tauri 2, `whisper-rs`, `ort` and `rusqlite` versions, PipeWire property names and whisper speed figures are from working knowledge and are pinned and verified by `dmn doctor` and `dmn bench` in M0.
