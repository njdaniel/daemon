# Daemon (`dmn`) task runner. Every recipe is also runnable by hand.

default:
    @just --list

# Fast compile check; run after every edit
check:
    cargo check --workspace --all-targets

# Unit + integration tests (no PipeWire; uses testdata WAVs)
test:
    cargo test --workspace

# Clippy with warnings as errors, plus formatting check
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all --check

fmt:
    cargo fmt --all

# Release build of the CLI
build:
    cargo build --release -p dmn-cli

# Tests that need a live PipeWire server (marked #[ignore]); dev machine only
probe:
    cargo test --workspace -- --ignored

# Replay a fixture interview through the real pipeline
replay fixture="sysdesign":
    cargo run -p dmn-cli -- replay testdata/{{fixture}}/interviewer.wav testdata/{{fixture}}/candidate.wav

# Regenerate the mock-interview fixtures in testdata/ with Piper TTS (voices cached in ~/.cache/dmn/piper)
fixtures *names:
    cd tools && uv run python fixtures/generate.py {{names}}
    cd tools && uv run python fixtures/verify.py {{names}}

# Download the fixture WAVs from the fixtures-v1 release when any are missing (CI uses this)
fixtures-fetch:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    missing=0
    for f in sysdesign behavioral mixed; do
      for r in interviewer candidate; do [ -f "testdata/$f/$r.wav" ] || missing=1; done
    done
    if [ "$missing" = 0 ]; then echo "fixtures present"; exit 0; fi
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
    # The repo is private, so prefer an authenticated gh download; fall back to a plain URL.
    if command -v gh >/dev/null && gh release download fixtures-v1 -p fixtures-v1.tar.gz -D "$tmp" 2>/dev/null; then :
    else curl -fsSL -o "$tmp/fixtures-v1.tar.gz" https://github.com/njdaniel/daemon/releases/download/fixtures-v1/fixtures-v1.tar.gz
    fi
    (cd "$tmp" && sed 's#  .*#  fixtures-v1.tar.gz#' "{{justfile_directory()}}/testdata/fixtures-v1.sha256" | sha256sum -c --quiet)
    tar -xzf "$tmp/fixtures-v1.tar.gz"
    echo "fixtures unpacked into testdata/"

# Offline eval: recall / precision / latency on the fixtures
eval:
    python3 eval/run.py

# --- Development process (docs/PROCESS.md) ---

# Run one issue headlessly: just agent 2, or `just agent next`; add --budget USD until caps are recorded
agent +args:
    scripts/agent.sh {{args}}

# Review a PR with the model from the size table; add --post to comment on the PR
review +args:
    scripts/review.sh {{args}}

# Create labels and GitHub issues from docs/issues/
issues-push:
    scripts/issues-push.sh
