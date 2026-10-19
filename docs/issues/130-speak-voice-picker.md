# #130 — speak: more voices — `o` picks the voice, `dmn speak design` makes new ones from a description, and a re-voice mode speaks your own words in them

Labels: `size:L, model:opus, m4, lane:audio, backlog`

Proposed 2026-10-18 by the owner: "different voices … like having a girls voice, and funny impressions", and "I dont want to lose my own voice". Today a voice belongs to a language: `[speak].languages = [{ code, voice }]` (#115), and `v` switches the language, which takes its voice with it. The owner's three voices (`voice`, `voice-ru`, `voice-zh`) are all cloned from the same clip of them. Speech only goes out re-voiced in translate mode (#114), so a different voice always comes with a translation.

What the engine allows: the Qwen3-TTS sidecar (`sidecars/qwen3-tts`, #122) serves `Qwen3-TTS-12Hz-1.7B-Base`. It clones in `x_vector_only_mode` and is cross-lingual ("One clip speaks every language", `crates/dmn-tts/src/qwen3.rs`). Its library is `GET /v1/audio/voices` → `uploaded_voices`. A sister checkpoint, `Qwen3-TTS-12Hz-1.7B-VoiceDesign`, makes a voice from a text description. Qwen's documented pattern is design, then clone: render one sample line with VoiceDesign, then enroll that sample into the Base model, which keeps the voice consistent in every later sentence. The owner's own voice, the Base model and the running sidecar stay exactly as they are. VoiceDesign runs only while a voice is being made (~6.5 GB of VRAM for those minutes, nothing during a call). Chatterbox speaks a voice only in the language it was enrolled with (`Voice.language`).

## Goal
Three pieces, one feature:
1. **`o` picks the voice.** In `dmn run`, `o` opens a list of the voices on the TTS server; the next sentence is spoken in the one chosen. The owner's own voice stays the default and is always first in the list. The choice is saved as `[speak].voice`.
2. **`dmn speak design` makes a voice.** `dmn speak design --name pirate --describe "grumpy old pirate, gravelly, slow"` renders a sample with VoiceDesign, lets the owner hear it, and enrolls it into the Base model, where it shows up in the `o` list.
3. **Re-voice mode speaks your words in it.** A third voice mode, `revoice`, re-speaks each sentence you finish, word for word, in the chosen voice and in the language you spoke. There's no LLM and no translation, so English stays English, in the pirate's voice.

## Size / Model / Turns
L | Opus | 60, two runs planned (the split below if one run cannot finish it)

## Scope
**1. Voice picker (`o`)**
- Config: `[speak].voice = "<name>"`, optional. Set, it is the voice for every language (Qwen3) and for re-voice. Unset, today's choice (`languages[].voice`, else `voice_ref`) is unchanged exactly. The picker writes it in place, keeping every other line and comment, like `target` (#115; the writers next to `Config::write_voice_ref` in `crates/dmn-core/src/config.rs`).
- The list is the server's enrolled voices (the `TtsProvider` voice listing), fetched at start and refreshed when the picker opens. The owner's own voice (the language's voice from `languages` / `voice_ref`) is first and labelled `your voice`; choosing it clears `[speak].voice`.
- Same component and keys as the `v` picker (↑/↓, Enter, Esc; Esc still quits with no picker open), over the unchanged layout. A switch writes a dim transcript marker `— voice: <name> —`. While a voice other than the owner's is chosen, the status line's voice tag names it, so it is never on by accident.
- A turn already being spoken finishes in its voice; the switch applies from the next sentence.
- Chatterbox: only voices enrolled for the current `target` are offered, and a later `v` to a language the chosen voice cannot speak falls back to that language's own voice with a 5 s notice.
- `dmn speak enroll --wav <clip> --name <name>` without `--language` adds a library voice without touching `languages`. `dmn speak voices` prints the library (name, `designed`/`enrolled`, and the language under Chatterbox).

**2. `dmn speak design`**
- `dmn speak design --name <name> --describe "<audible traits>" [--line "<sample text>"] [--language <code>]`. It starts the VoiceDesign server, renders the sample line, plays it (or writes `--out`), asks `keep? [y/N]`, then enrolls the WAV into the Base sidecar under `<name>` (the existing enroll path: copy under the data dir, upload to `/v1/audio/voices`). It stops the VoiceDesign server when done. `--yes` skips the question for scripts.
- Sidecar: a second service in `sidecars/qwen3-tts/compose.yaml` under a compose profile (`design`), same image `vllm/vllm-omni:v0.30.0`, serving `Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign` on 127.0.0.1:4125, not started by default. Its stage overrides are as small as the Base service's. The command starts and stops it (`docker compose --profile design up -d` / `stop`) and waits for `/health`.
- VRAM check first: with less free than the service needs (measured when building; ~6.5 GB expected), the command stops and names what holds the GPU (`nvidia-smi` processes), e.g. a stale Chatterbox container. It never stops anything itself.
- `dmn-tts`: a `design` request against the VoiceDesign server (vLLM-Omni's speech endpoint with the VoiceDesign task and the description as its instruction; verify the field names against the v0.30.0 server when building and record them in `qwen3.rs`'s header like the Base API). The clip is stored with consent tag `dmn-designed`.
- `[speak].design_url` (default `http://localhost:4125`) in the config template.

**3. Re-voice mode**
- `VoiceMode::Revoice` beside `Passthrough` and `Translate` (`crates/dmn-session/src/action.rs`), `[speak].mode = "revoice"`. The mic is muted into `dmn-voice` as in translate mode. Each finished You turn goes to the TTS as is: no translator request, no #117 off-script check (there's no model reply to check). Its language is the segment's detected language when there is one (#77), else `[stt].language`, else Qwen3's `Auto`.
- `l` cycles your own voice → translate → re-voice. If #124 (push-to-talk) has merged first, re-voice joins its cycle after push-to-talk and push-to-talk applies to it the same way (hold to speak, released turn re-voiced); if not, #124 adds itself to this cycle. Status tag: `voice: <name>` with a distinct label for re-voice (e.g. `revoice: pirate`).
- Everything else stays as in translate mode: sentence splitting, the leading-silence trim, the echo window and the transcript's `You → <voice>` label in place of `You → de`.

**Docs**: README speak section (make a voice, pick it, re-voice), SPEC speak paragraphs and key table, `dmn doctor` lines (`[speak].voice` exists on the server; the design server's image and model are present when `design_url` is set).

**Out of scope**: cloning recognisable real people for impressions (design from descriptions, or enroll the owner's own or a consenting speaker's clip; the consent tags say which), Qwen3's CustomVoice preset speakers, live voice conversion with no sentence delay, voices for the other side of the call, and changing the language picker.

**If one run cannot finish it**, split here: (a) picker + `[speak].voice` + `enroll --name` + `speak voices`; (b) `dmn speak design` + the sidecar service; (c) re-voice mode. Each leaves `main` working on its own; (b) and (c) need (a).

## Depends on
#115 (language picker), #122 (Qwen3 engine), #114 (voice modes), all merged.

## Acceptance
- [ ] `[speak].voice` parses, round-trips and is written in place; unset leaves every language's voice exactly as today (`cargo test -p dmn-core speak`)
- [ ] `o` lists `your voice` first, then the server's voices; Enter switches from the next sentence, Esc cancels, and choosing `your voice` clears the key (state tests and a new picker snapshot: `cargo test -p dmn-cli tui`)
- [ ] With Qwen3 one chosen voice speaks every listed language; with Chatterbox only voices of the current target are offered and the `v` fallback notice appears (MockProvider / the test server: `cargo test -p dmn-tts`, `cargo test -p dmn-cli speak`)
- [ ] `dmn speak enroll --name` without `--language` adds a library voice and leaves `languages` alone; `dmn speak voices` lists it
- [ ] `dmn speak design` against the test server: renders, asks, enrolls into the Base server under the name with consent `dmn-designed`, and stops the design server; with too little free VRAM it refuses and names the processes (`cargo test -p dmn-cli design`, `cargo test -p dmn-tts design`)
- [ ] Re-voice: a finished You turn reaches the TTS word for word in the chosen voice and the turn's language, with no translator request (MockProvider counts zero LLM calls); `l` cycles the three modes; `[speak].mode = "revoice"` starts in it (`cargo test -p dmn-cli revoice`)
- [ ] One live check on the dev box, by the owner: design one voice, pick it with `o`, re-voice a few English sentences into a Discord call; then `o` → `your voice` and translate mode still sound as before
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-core speak
cargo test -p dmn-tts
cargo test -p dmn-cli speak
cargo test -p dmn-cli design
cargo test -p dmn-cli revoice
cargo test -p dmn-cli tui          # DMN_UPDATE_SNAPSHOTS=1 for the new picker snapshot; review its diff
```
