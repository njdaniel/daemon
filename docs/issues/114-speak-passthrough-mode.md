# #114 — speak: your real voice passes through `dmn-voice` by default; a mode toggle makes every sentence you finish come out translated instead

Labels: `size:L, model:fable, m4, lane:audio`

Proposed 2026-10-07, from the user's notes after the first live Discord tests of #107–#109: "I want to still be able to speak english through the virtual mic. so just a toggle to switch to another language … and i want to know if im speaking in another language". Today, with `dmn-voice` selected as the call's microphone, the other side hears nothing except what `t` sends: the real microphone never reaches the call. Choices made with the user the same day are marked **Decided**.

## Goal
With `dmn-voice` as the call's microphone, the call hears the user's own voice live until the user switches translate mode on; while it is on, each sentence the user finishes is translated into the current target language and spoken in the cloned voice instead, and the screen makes it unmistakable which mode is active.

## Size / Model / Turns
L | Fable | 60, two runs planned

## Scope

### Pass-through (`crates/dmn-audio`)
- The `You` capture lane's frames are also written to `dmn-voice` (the #108 ring) whenever pass-through is on: mic → ring → PipeWire source, at the graph rate, mono f32. The capture callback still allocates, locks and logs nothing: it pushes to the same lock-free ring from its own thread, or the feeding thread copies from the existing frame channel (decide for latency; measure mic-to-`dmn-voice` latency with `pw-record` and report it; under 50 ms is the target).
- A mode switch that is sample-accurate enough not to click: pass-through stops at the next buffer boundary, translated speech is written from then on; a short fade (5–10 ms) at each switch.
- While translated speech plays, pass-through is off regardless of mode, so the call never hears both at once (**Decided**: "Auto per sentence": the user's own voice is muted into `dmn-voice` while translate mode is on).

### Translate mode (`apps/dmn-cli`)
- **Decided**: a toggle key switches between **pass-through** (default at start) and **translate**. The key is not `t` (which stays as the one-shot "speak my last turn" from #109); pick a free one (`l`, for language, if still free) and say so in the PR.
- In translate mode, every finalized `You` turn (the #109 `last_turn` grouping, so a sentence split by whisper is one utterance) is translated and spoken automatically, in order, through one queue; a turn that arrives while another is speaking waits. `t` while in translate mode re-speaks the last turn (useful if the automatic one failed).
- Leaving translate mode cancels the queue's pending turns (not the one playing) and restores pass-through.
- Everything about one utterance (translate → synthesize → play, the echo window, the 5 s failure notice) is #109's path; this issue drives it from the transcript instead of the key.

### The user must know which mode is on
- Status line: a wide, coloured tag that reads differently in each mode: `voice: you (en)` in pass-through, `voice: → de` in translate mode (the target from #115 or `[speak].target`), amber while a translation is in flight. It is the first tag on the line, not the last.
- The transcript marks lines that went out translated (`You → de  …` as the label, with the translation on the next line dim) so the user can read back what the call heard.
- Entering or leaving translate mode writes one line to the transcript (`— translate mode on: → de —`), so the switch is visible in history too.
- With assistance off the same tags apply on the transcript-first screen.

### Config
- `[speak].mode = "passthrough" | "translate"`: the mode at start-up (default `passthrough`). Changing it does not need #115.

### Docs
- README § Speak: the two modes, the toggle, the indicator, what the call hears in each. SPEC § Speak + § Audio (pass-through path) + § Threading rules (the callback rule still holds).

Out of scope: choosing the language in-session (#115), ducking the call's audio, speaking the interviewer's words back translated to the user, barge-in, the M3 window.

## Depends on
#109 (merged), #113 (the speed work; this issue can be built before it but is only usable once a sentence renders in a few seconds)

## Acceptance
- [ ] With `dmn-voice` as the mic and pass-through on, `pw-record --target dmn-voice` of a test tone played into the mic lane (fixture WAV through the capture path in a `#[ignore]` PipeWire test) reproduces the tone at the right pitch and length; measured mic→`dmn-voice` latency in the PR
- [ ] Toggle key: pass-through → translate → pass-through; the status tag, the transcript marker and `[speak].mode` at start-up are tested (reducer tests + snapshots at 100 × 30 and 80 × 24, assist on and off)
- [ ] In translate mode, two finalized `You` turns produce two translate requests and two synthesize requests in order, the second waiting for the first to finish playing (mock LLM + mock TTS + test sink); pass-through is off while either plays; leaving the mode cancels the pending one
- [ ] `t` in translate mode re-speaks the last turn; `t` in pass-through behaves as #109
- [ ] The call never hears both voices: no mic frames are written to the ring while translated speech plays (unit test on the feeder)
- [ ] README and SPEC updated
- [ ] `just check`, `just test` and `just lint` green; `just probe` green for the new PipeWire test

## Verify
```bash
cargo test -p dmn-audio
cargo test -p dmn-cli pipeline
cargo test -p dmn-cli tui
just probe
# live: dmn run --no-assist, dmn-voice as Discord's mic, talk (the call hears you), toggle, talk (the call hears German), toggle back
```
