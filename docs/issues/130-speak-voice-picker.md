# #130 — speak: a voice picker — choose which enrolled voice speaks, whatever the language

Labels: `size:M, model:opus, m4, lane:ui, backlog`

Proposed 2026-10-18 by the owner ("different voices"). Today a voice belongs to a language: `[speak].languages = [{ code, voice }]` (#115), and `v` switches the language, which takes its voice with it. The owner's three voices (`voice`, `voice-ru`, `voice-zh`) are all cloned from the same clip of them. The owner chose a voice library separate from the languages, so any enrolled voice can speak any target language, with a picker key to switch mid-call like `v`.

What the engines allow: Qwen3-TTS (`crates/dmn-tts/src/qwen3.rs`, #122) clones in `x_vector_only_mode` and is cross-lingual ("One clip speaks every language"). Its library is `GET /v1/audio/voices` → `uploaded_voices`. Chatterbox speaks a voice only in the language it was enrolled with (`Voice.language`).

## Goal
`o` in `dmn run` opens a list of the voices enrolled on the TTS server, and the next sentence is spoken in the one chosen. With Qwen3 that voice speaks every `[speak].languages` language. The choice is saved as `[speak].voice` and survives a restart.

## Size / Model / Turns
M | Opus | 45

## Scope
- **Config.** `[speak].voice = "<name>"`, optional. Set, it is the voice for every language (Qwen3). Unset, today's choice (`languages[].voice`, else `voice_ref`) is unchanged exactly. The picker writes it in place, keeping every other line and comment, like `target` (#115; the writers next to `Config::write_voice_ref` in `crates/dmn-core/src/config.rs`). Template line: `# voice = "voice"   # dmn run: the enrolled voice that speaks every language; unset = each language's own voice; o picks (#130)`.
- **Library.** The picker lists the server's enrolled voices (`TtsProvider` voice listing). The list is fetched at start and refreshed when the picker opens, so a voice enrolled from another terminal mid-call shows up. `dmn speak enroll --wav <clip> --name <name>` without `--language` adds a voice to the library without touching `languages`. `dmn speak voices` prints the library: name, plus the enrolled language under Chatterbox.
- **Picker.** `o` opens it over the unchanged layout, built from the same component and keys as the `v` picker (↑/↓, Enter, Esc; Esc still quits when no picker is open). The current voice is marked. A switch writes a dim transcript marker `— voice: <name> —`, and the status line's voice tag names the voice when it differs from the language's own.
- **Timing.** A turn already being spoken finishes in its voice; the switch applies from the next sentence, as the language picker does.
- **Chatterbox.** The picker offers only voices enrolled for the current `target`. A later `v` to a language the chosen voice cannot speak falls back to that language's own voice, with a 5 s notice naming both.
- **`dmn doctor`.** A `[speak].voice` the server does not have fails with the enroll command to run (`dmn speak enroll --wav <clip> --name <voice>`). Under Chatterbox, a voice that cannot speak `target` is reported too.
- **Docs.** README speak section (enroll a second voice, `o`), SPEC speak paragraph, key table.

Out of scope: the engine's built-in preset speakers (Qwen3's CustomVoice is a different model with more VRAM; a follow-up if wanted), voice design from a text description, voices for the other side of the call, and changing the language picker.

## Depends on
#115 (language picker), #122 (Qwen3 engine), both merged.

## Acceptance
- [ ] `[speak].voice` parses, round-trips and is written in place; unset leaves every language's voice exactly as today (`cargo test -p dmn-core speak`)
- [ ] `o` opens the picker, Enter switches, Esc cancels, and the next sentence uses the chosen voice while a turn in flight keeps its own (state tests and a new picker snapshot: `cargo test -p dmn-cli tui`)
- [ ] With Qwen3, one chosen voice speaks every listed language; with Chatterbox, only voices of the current target are offered and the `v` fallback notice appears (MockProvider / the test server: `cargo test -p dmn-tts`, `cargo test -p dmn-cli speak`)
- [ ] `dmn speak enroll --wav <clip> --name <name>` without `--language` adds a library voice and leaves `languages` alone; `dmn speak voices` lists it (`cargo test -p dmn-cli speak`)
- [ ] `dmn doctor` reports a `[speak].voice` the server lacks (`cargo test -p dmn-cli doctor`)
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-core speak
cargo test -p dmn-tts
cargo test -p dmn-cli speak
cargo test -p dmn-cli tui          # DMN_UPDATE_SNAPSHOTS=1 for the new picker snapshot; review its diff
cargo test -p dmn-cli doctor
```
