# Fixtures

Three 10-minute mock interviews, each as two 16 kHz mono WAVs (interviewer, candidate) plus `questions.json` listing every real question with its timestamp. Generated with two TTS voices so they can be redistributed. WAVs are git-ignored until a home for large files is chosen; `questions.json` and scripts are checked in.

```
testdata/
  sysdesign/   interviewer.wav  candidate.wav  questions.json  script.md
  behavioral/  interviewer.wav  candidate.wav  questions.json  script.md
  mixed/       interviewer.wav  candidate.wav  questions.json  script.md   # includes small talk
```

`questions.json` shape:

```json
[{"t": 12.4, "end": 18.9, "text": "How would you design a rate limiter for a globally distributed API?", "kind": "system_design"}]
```

`dmn replay <interviewer.wav> <candidate.wav>` feeds both through the real pipeline; `eval/run.py` compares detected questions against `questions.json`.
