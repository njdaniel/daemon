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
