# #124 — speak: push-to-talk — hold a button to speak, the translation starts the moment you let go

Labels: `size:L, model:opus, m4, lane:audio, backlog`

Proposed 2026-10-18 from the #122 live Discord test. With Qwen3-TTS the first translated word plays ~0.17 s after the request, so the wait is now the sentence end itself: 0.4 s of silence before the VAD closes the utterance (`vad_close_ms`), then whisper (~0.3 s), the translation (~0.2 s) and the voice; until the echo guard was turned off in the test, every You final also waited 1.5 s there. Talking with pauses also splits one thought into several turns. The owner: "lets make an feature issue for ptt and the translation starts right when the button is release". **Decided** with the owner: push-to-talk is an option, off by default; the button is read straight from its device (evdev), so it works while Discord has focus; while it is not held, the call hears silence.

Why evdev: COSMIC's portal has no GlobalShortcuts interface (its `cosmic.portal` lists Access, FileChooser, Screenshot, Settings, ScreenCast), COSMIC custom shortcuts fire on press only, and dmn's terminal does not have focus during a call. The owner's account is not in the `input` group, so setup grants access to the one device with a udev rule.

## Goal
Push-to-talk is an option: off by default, and a config without it behaves exactly as today (pass-through, translate mode and `t` unchanged). With push-to-talk on, holding a configured button makes everything said while it is held one turn; releasing it closes the turn at once (no silence wait), and its translation starts playing into `dmn-voice` about 0.7 s after the release. While the button is not held the call hears nothing from the microphone.

## Size / Model / Turns
L | Opus | 60

## Scope

### 1. The button (evdev)
- A thread reads one input device (`/dev/input/by-id/...-event-*`) and sends `Pressed` / `Released` with the kernel's event time on a bounded crossbeam channel.
- It never grabs the device, so the button keeps its normal function elsewhere; the docs therefore recommend a key nothing else uses (F13–F24 mapped on a programmable keyboard such as the owner's UHK, or a spare mouse button: a G402 side button is "back" in a browser).
- Behind a small trait so tests feed synthetic presses; reading the real device is an `#[ignore]` test for `just probe`.
- New dependency (the `evdev` crate, or raw `input_event` reads through `nix`) with its line in docs/DEPENDENCIES.md; no `unsafe` outside dmn-audio and dmn-stt.

### 2. Config and setup
- Opt-in: nothing reads an input device unless `[speak].mode = "ptt"` is set or the user switches to it during a run. `mode = "ptt"` without `ptt_device` / `ptt_key` is a config error naming both and `dmn speak ptt learn`.
- `[speak].ptt_device` (a by-id path) and `[speak].ptt_key` (an evdev key name: `KEY_F13`, `BTN_SIDE`).
- `dmn speak ptt learn` waits for the next button press on the devices it can read and writes both keys in place, as `enroll` edits the config.
- README gives the one-time udev rule (`TAG+="uaccess"` for that device, then `udevadm trigger`) instead of adding the user to the `input` group, which would let every program read every keystroke.
- `dmn doctor`: the device exists, is readable, and has the key; otherwise the fix.

### 3. The turn follows the button
- A third voice mode, `ptt`, beside `passthrough` (still the default) and `translate`. `[speak].mode = "ptt"` starts in it; with a button configured, `l` cycles your own voice → translate → push-to-talk, so it can also be picked during a run, and without one `l` toggles the two modes as today.
- In it the You lane's segmenter (`crates/dmn-vad/src/segmenter.rs`, run by `worker.rs`) is driven by the button, not the VAD: press opens an utterance (with the existing pre-roll, so the first syllable is kept), pauses inside the hold do not split it, and release closes it at once (a new `CloseReason`, the path `Segmenter::flush` takes today), so whisper's final pass starts immediately.
- A hold past whisper's segment limit is cut where the segmenter cuts today and the pieces join into one turn (the translate-mode queue already joins finals: `pipeline.rs`, `heard` / `start_next`).

### 4. No waiting after release
- A held span's final skips the echo guard's 1.5 s hold (`crates/dmn-transcript/src/echo.rs`): the press says the user is the one speaking.
- It goes straight to the translate queue; one press is one translation, never merged with the next press.

### 5. The call hears silence when not held
- In `ptt` mode the pass-through stays off (`speaker.set_passthrough(false)`); only the speaker's translations reach `dmn-voice`.

### 6. Screen
- The status tag reads `voice: PTT → zh`, with `● talking` while the button is held.
- A held span's line in the transcript is marked as one turn.
- A missing or unreadable device is said on the status line at start, and the run goes on in `translate` mode.

### 7. Timing in the log
- `released_to_first_audio_ms` beside `translated_ms` / `first_audio_ms` on the "speak: spoke into dmn-voice" line, measured from the release event.

### Docs
- README § Speak (setup, the udev rule, choosing a key), SPEC § Speak, docs/DEPENDENCIES.md, docs/issues/README.md line.

Out of scope: the XDG GlobalShortcuts portal (COSMIC lacks it; revisit when it lands), a tap-to-start / tap-to-send desktop shortcut, Discord's own push-to-talk, translating while the button is still held (phrase by phrase), the echo guard's default (its own issue).

## Depends on
#114 (translate mode, pass-through), #122 (fast TTS; without it the release still waits 1–2 s for Chatterbox)

## Acceptance
- [ ] Off by default: a config without the `ptt` keys opens no input device and behaves as before (the existing speak tests pass unchanged; `l` toggles pass-through ↔ translate)
- [ ] `[speak].ptt_device`, `ptt_key` and `mode = "ptt"` parse; a bad key name is a config error naming it; `mode = "ptt"` without a button is a config error naming `dmn speak ptt learn`
- [ ] Synthetic press/release over a fixture WAV with a 1 s pause in the middle: one final, one translation, one synthesis, started at the release (no `vad_close_ms` wait), tested with the mock LLM and `MockTtsProvider`
- [ ] Speech while the button is not held produces no turn and nothing in `dmn-voice`; pass-through stays off in `ptt` mode
- [ ] A held span's final skips the echo guard hold; two presses give two translations
- [ ] `dmn speak ptt learn` writes both keys in place; `dmn doctor` reports the device and the fix for an unreadable one
- [ ] Live on Discord (owner, headphones, zh/ru/de): release → first audio in `dmn-voice` ≤ 0.8 s, `released_to_first_audio_ms` in the PR body
- [ ] README (setup, udev rule, choosing a key), SPEC § Speak and DEPENDENCIES updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-vad segmenter
cargo test -p dmn-cli ptt
cargo test -p dmn-core config
# live: dmn speak ptt learn; [speak].mode = "ptt"; dmn run --no-assist --app ""; hold, speak, release; read released_to_first_audio_ms in the log
```
