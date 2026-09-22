# Fixtures

Three mock interviews, each as two time-aligned 16 kHz mono 16-bit WAVs (interviewer, candidate) plus
the timing files the eval harness scores against. They are synthesised with Piper TTS from the scripts
in `tools/fixtures/scripts/`, so anyone can regenerate or redistribute them.

| Fixture | Length | Turns | Real questions | What it exercises |
| --- | --- | --- | --- | --- |
| `sysdesign/` | 12:05 | 32 | 14 | Multi-part design questions, clarification follow-ups ("And what if Redis goes down…?"), imperative cues ("Walk me through…", "Talk me through…") |
| `behavioral/` | 9:13 | 28 | 12 | STAR prompts ("Tell me about a time…", "Describe…", "Give me an example…") with short follow-ups |
| `mixed/` | 9:03 | 32 | 10 | Small talk, "can you hear me okay?", rhetorical "…right?" lines that are not questions, hand-offs ("Let's switch gears"), and 6–7 s silences |

```
testdata/
  <fixture>/
    interviewer.wav   candidate.wav    ignored by git; see "Getting the WAVs"
    questions.json                     every real question: [{"t", "end", "text", "kind"}]
    turns.json                         every turn: [{"i", "speaker", "t", "end", "is_question", "kind", "text"}]
    script.md                          readable transcript with timestamps
  fixtures-v1.sha256                   checksum of the release tarball
```

- **Timing.** `t` and `end` are seconds from the start of the WAV. They come from the synthesised audio, not from estimates. A question's span is the whole interviewer turn, preamble included, so `end` is the moment the interviewer stops talking, which is where the 2 s latency budget starts.
- **Kinds.** Kinds follow SPEC § Transcription and question detection: `system_design`, `coding`, `behavioral`, `knowledge`, `clarification`, `small_talk`. Turns with `is_question: false` are the negatives for precision: greetings, rhetorical statements, small talk and closing remarks.
- **Audio.** Each track holds only its own speaker. Everywhere else there is a -60 dBFS noise floor, so a VAD sees realistic silence rather than digital zero. Speech is levelled to -20 dBFS RMS per turn.

## Getting the WAVs

The WAVs are about 20 MB each and are not in git. Choose one way to get them:

```
just fixtures-fetch   # download the fixtures-v1 release asset, check its sha256, unpack here (CI uses this)
just fixtures         # regenerate locally with Piper (about 10 s per fixture on the dev box), then verify
```

Regeneration is byte-identical on the same machine and package versions. Piper samples noise inside its ONNX graph, so `tools/fixtures/generate.py` seeds ONNX Runtime per fixture and voice. If a script edit changes the audio, publish a new `fixtures-vN` release and update `fixtures-v1.sha256` and the recipe to match.

## Voices and licences

| Role | Piper voice | Training data | Licence |
| --- | --- | --- | --- |
| Interviewer | `en_US-norman-medium` | Public-domain recordings, trained from scratch | Public domain |
| Candidate | `en_US-ljspeech-medium` | [LJ Speech](https://keithito.com/LJ-Speech-Dataset/), trained from scratch | Public domain |

Piper's code and the `rhasspy/piper-voices` repository are MIT licensed. The voice model cards give the licences above.

Several popular Piper voices are not safe to redistribute from, so do not switch to them:

- `en_US-lessac-*` comes from the Blizzard 2013 Lessac data, which is licensed for research only, with no third-party use.
- `en_US-amy-*` and `en_US-joe-*` are fine-tuned from Lessac.
- `ryan`, `hfc_*` and `l2arctic` carry NonCommercial licences.

## Replay

`dmn replay <interviewer.wav> <candidate.wav>` feeds both tracks through the real pipeline. That arrives with issue #9. The recall and precision harness in `eval/` (issue #10) compares detected questions against `questions.json`.
