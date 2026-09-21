# Eval harness (Python, offline only)

`run.py` runs `dmn replay` on every fixture under `testdata/`, reads the session log, and reports question recall, precision and question-end → Approach latency (p50, p95). Lands with issue #10; Python is not used anywhere on the audio path.
