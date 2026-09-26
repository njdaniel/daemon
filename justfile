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

# Download the fixture WAVs from their releases when any are missing (CI uses this):
# fixtures-v1 holds sysdesign, behavioral and mixed; fixtures-v2 adds continuous (#63)
fixtures-fetch:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{justfile_directory()}}"
    tmps=(); trap 'rm -rf "${tmps[@]}"' EXIT  # also on a failed download or checksum
    fetch() { # <release> <fixture…>: download and unpack the release when one of its fixtures is missing
      local rel=$1; shift
      local missing=0
      for f in "$@"; do
        for r in interviewer candidate; do [ -f "testdata/$f/$r.wav" ] || missing=1; done
      done
      if [ "$missing" = 0 ]; then echo "$rel: fixtures present"; return; fi
      local tmp; tmp=$(mktemp -d); tmps+=("$tmp")
      # The repo is private, so prefer an authenticated gh download; fall back to a plain URL.
      if command -v gh >/dev/null && gh release download "$rel" -R njdaniel/daemon -p "$rel.tar.gz" -D "$tmp" 2>/dev/null; then :
      else curl -fsSL -o "$tmp/$rel.tar.gz" "https://github.com/njdaniel/daemon/releases/download/$rel/$rel.tar.gz"
      fi
      (cd "$tmp" && sed "s#  .*#  $rel.tar.gz#" "{{justfile_directory()}}/testdata/$rel.sha256" | sha256sum -c --quiet)
      tar -xzf "$tmp/$rel.tar.gz"
      echo "$rel: unpacked into testdata/"
    }
    fetch fixtures-v1 sysdesign behavioral mixed
    fetch fixtures-v2 continuous

# Offline eval: recall / precision / latency on the fixtures
eval:
    python3 eval/run.py

# --- Development process (docs/PROCESS.md) ---

# Run one issue headlessly: just agent 2, or `just agent next`; add --budget USD until caps are recorded
agent +args:
    scripts/agent.sh {{args}}

# Review a PR at once: Gemini via agy, Copilot findings triaged when present but never waited for; --via claude, --post
review +args:
    scripts/review.sh {{args}}

# Create labels and GitHub issues from docs/issues/
issues-push:
    scripts/issues-push.sh
