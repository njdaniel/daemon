# #125 — audio: the echo guard is off by default — your sentences stop waiting 1.5 s for an echo that headphones never make

Labels: `size:S, model:sonnet, m4, lane:stt`

Proposed 2026-10-18 from the #122 live Discord test. With the fast voice engine, the largest step between the end of the user's sentence and its translation in `dmn-voice` was the echo guard (#60): every You final waits 1.5 s (up to 4 s while the loopback lane has an utterance open, `HOLD` / `MAX_HOLD` in `crates/dmn-transcript/src/echo.rs`) for a matching Interviewer final before anything downstream sees it, and translate mode (#114) only starts after that. On headphones the mic never hears the call, so the hold is pure delay. Turning it off in the owner's config took the end of speech to the first translated word from about 2.7 s to about 1.7 s (before #122's leading-silence trim; ~1.2 s after it). The owner: "i think by default echo guard should be off".

## Goal
`[audio].echo_guard` defaults to `false`. A config that does not set it runs with no hold on the You lane; one that sets `echo_guard = true` keeps today's behaviour exactly. The docs say plainly when to turn it on: the mic can hear the call (speakers, a laptop without echo cancellation, questions read from a phone in the room).

## Size / Model / Turns
S | Sonnet | 30

## Scope
- `crates/dmn-core/src/config.rs`: `Audio::default().echo_guard = false`; the template line becomes `echo_guard = false   # true when the mic can hear the call (speakers): drops mic text that copies the call, at the cost of a 1.5 s hold on each of your sentences`. Tests that assumed the default follow.
- SPEC: the config example (`spec_example_round_trips_and_matches_defaults` keeps it equal to the defaults), § Audio capture rule 2 and the #114 / #122 paragraphs that cite the hold.
- README: the echo-guard paragraph (it still says "planned for M2 and is not active yet") states the default, the cost of turning it on (each of your sentences waits up to 1.5 s, 4 s while the call is talking, which delays translate mode by as much), and when to turn it on; the translate-mode latency paragraph drops the hold from the default path.
- `dmn doctor`: one informational line naming the setting, and with `[speak]` on and the guard on, the extra wait per sentence.
- A user's existing file that says `echo_guard = true` (the template wrote it explicitly until now) keeps it; README says how to turn it off. No migration.

Out of scope: a smarter hold (release a You final at once when the loopback lane was silent while it was spoken, so the guard costs nothing on headphones even when on); push-to-talk's own bypass (#124).

## Depends on
#60 (the guard), #114 (translate mode, which pays the hold)

## Acceptance
- [ ] `Config::default().audio.echo_guard` is `false`; a file without the key gets `false`; `echo_guard = true` still enables today's guard (its tests in `echo.rs` pass unchanged)
- [ ] The SPEC config example matches the new default (`cargo test -p dmn-core spec_example`)
- [ ] README and SPEC describe the default, the 1.5 s / 4 s cost and when to turn it on; the "not active yet" text is gone
- [ ] `dmn doctor` reports the setting, and the per-sentence wait when `[speak]` is on with the guard on
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-transcript echo
cargo test -p dmn-cli doctor
```
