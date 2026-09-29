# Fixtures

Five mock interviews, each as two time-aligned 16 kHz mono 16-bit WAVs (interviewer, candidate) plus
the timing files the eval harness scores against. They are synthesised with Piper TTS from the scripts
in `tools/fixtures/scripts/`, so anyone can regenerate or redistribute them.

| Fixture | Length | Turns | Real questions | What it exercises |
| --- | --- | --- | --- | --- |
| `sysdesign/` | 12:05 | 32 | 14 | Multi-part design questions, clarification follow-ups ("And what if Redis goes down…?"), imperative cues ("Walk me through…", "Talk me through…") |
| `behavioral/` | 9:13 | 28 | 12 | STAR prompts ("Tell me about a time…", "Describe…", "Give me an example…") with short follow-ups |
| `mixed/` | 9:03 | 32 | 10 | Small talk, "can you hear me okay?", rhetorical "…right?" lines that are not questions, hand-offs ("Let's switch gears"), and 6–7 s silences |
| `continuous/` | 7:38 | 42 | 16 | #63: two-person speech with 0.4–0.6 s gaps; questions split mid-sentence by 0.9 s pauses (`||` in the script), so the VAD closes an utterance inside the question; fillers and restarts ("So, uh, how would you, um…", "What did you, uh, what did you…"); a clarifying question from the candidate; back-to-back interviewer statements; and a reverse-questions close ("Do you have any questions for me?") |
| `es/` | 10:45 | 35 | 13 | #77: the same shape as `mixed/`, spoken in Spanish (system design with a clarification, STAR prompts, a knowledge and a coding question, a rhetorical "¿verdad?", a hand-off, a reverse-questions close). `questions.json` and `turns.json` hold the English reference in `text` and the Spanish words in `spoken`, so the pipeline is scored on whisper's translation: `python3 eval/run.py --fixture es --language auto --model small` |

```
testdata/
  <fixture>/
    interviewer.wav   candidate.wav    ignored by git; see "Getting the WAVs"
    questions.json                     every real question: [{"t", "end", "text", "kind"}], plus "spoken" for a fixture in another language (#77)
    turns.json                         every turn: [{"i", "speaker", "t", "end", "is_question", "kind", "text"}], plus "spoken" likewise
    script.md                          readable transcript with timestamps
  fixtures-v1.sha256                   checksum of the release tarball for sysdesign, behavioral, mixed
  fixtures-v2.sha256                   checksum of the release tarball for continuous (#63)
  fixtures-v3.sha256                   checksum of the release tarball for es (#77)
```

- **Timing.** `t` and `end` are seconds from the start of the WAV. They come from the synthesised audio, not from estimates. A question's span is the whole interviewer turn, preamble included, so `end` is the moment the interviewer stops talking, which is where the 2 s latency budget starts.
- **Kinds.** Kinds follow SPEC § Transcription and question detection: `system_design`, `coding`, `behavioral`, `knowledge`, `clarification`, `small_talk`. Turns with `is_question: false` are the negatives for precision: greetings, rhetorical statements, small talk and closing remarks.
- **Audio.** Each track holds only its own speaker. Everywhere else there is a -60 dBFS noise floor, so a VAD sees realistic silence rather than digital zero. Speech is levelled to -20 dBFS RMS per turn.

## Getting the WAVs

The WAVs are about 20 MB each and are not in git. Choose one way to get them:

```
just fixtures-fetch   # download the fixtures-v1, -v2 and -v3 release assets, check their sha256, unpack here (CI uses this)
just fixtures         # regenerate locally with Piper (about 10 s per fixture on the dev box), then verify
```

Regeneration is byte-identical on the same machine and package versions. Piper samples noise inside its ONNX graph, so `tools/fixtures/generate.py` seeds ONNX Runtime per fixture and voice. Each release holds only the fixtures it adds: `fixtures-v1` the first three, `fixtures-v2` `continuous/` (#63), `fixtures-v3` `es/` (#77), so adding a fixture never changes an existing tarball or its checksum. If a script edit changes existing audio, publish a new `fixtures-vN` release for it and update its `.sha256` file and the recipe to match.

## Voices and licences

| Role | Piper voice | Training data | Licence |
| --- | --- | --- | --- |
| Interviewer | `en_US-norman-medium` | Public-domain recordings, trained from scratch | Public domain |
| Candidate | `en_US-ljspeech-medium` | [LJ Speech](https://keithito.com/LJ-Speech-Dataset/), trained from scratch | Public domain |
| Interviewer (`es/`) | `es_ES-davefx-medium` | [OHF-Voice datasets](https://github.com/OHF-Voice/voice-datasets) | CC0 |
| Candidate (`es/`) | `es_MX-ald-medium` | [Ald Mexican Spanish speech dataset](https://huggingface.co/datasets/rmcpantoja/Ald_Mexican_Spanish_speech_dataset) | Unlicense |

Piper's code and the `rhasspy/piper-voices` repository are MIT licensed. The voice model cards give the licences above.

Several popular Piper voices are not safe to redistribute from, so do not switch to them:

- `en_US-lessac-*` comes from the Blizzard 2013 Lessac data, which is licensed for research only, with no third-party use.
- `en_US-amy-*` and `en_US-joe-*` are fine-tuned from Lessac.
- `ryan`, `hfc_*` and `l2arctic` carry NonCommercial licences.

## Replay

`dmn replay <interviewer.wav> <candidate.wav>` feeds both tracks through the real pipeline. That arrives with issue #9. The recall and precision harness in `eval/` (issue #10) compares detected questions against `questions.json`.

## Echo fixture (`echo/`, #60)

The mic of a laptop without echo cancellation hears the call too. `python3 tools/fixtures/echo.py`
derives `echo/` from `sysdesign/` (after `just fixtures-fetch`): the interviewer track is copied,
and the candidate track becomes the candidate plus the interviewer at -9 dB, 100 ms late
(`--source`, `--gain-db` and `--delay-ms` change that). The text files are copied, so it scores
like its source. The whole folder is derived and ignored by git, so it needs no release of its
own. The echo guard should drop every copied question and keep every real answer:
`dmn replay testdata/echo/interviewer.wav testdata/echo/candidate.wav --no-llm` prints
`echo guard   N mic line(s) dropped`.

## Context sources (`context/`, #53)

Small fictional documents for `dmn sources` and, later, retrieval (#54). Nothing in them is about
a real person or company, and they are checked into git.

| File | Kind | What it exercises |
| --- | --- | --- |
| `context/resume.md` | resume | Markdown; about 430 words, so more than one 300-token chunk |
| `context/resume.pdf` | resume | The same resume as a one-page text PDF, generated from `resume.md` by `python3 tools/fixtures/context_pdf.py` (stdlib only, deterministic) |
| `context/job-description.txt` | job_description | Plain text |
| `context/notes/` | notes | A folder of two notes, one `.md` and one `.txt` |
| `context/stories/` | stories | Eight short STAR stories (#58), one per file, covering the behavioral fixture's questions; tags in YAML front matter (`[a, b]` or `a, b`) or on a first `tags:` line, and one untagged story that only BM25 can match |

Refusals (empty, unsupported, non-UTF-8, broken or image-only PDFs) are built in tempdirs by the
tests rather than committed.
