# Shared helpers for scripts/agent.sh and scripts/review.sh. Sourced, not run.
set -euo pipefail
ROOT=$(git rev-parse --show-toplevel)
RUNS="$ROOT/.agent-runs"; mkdir -p "$RUNS"
PROCESS="$ROOT/docs/PROCESS.md"

die() { echo "error: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "$1 is required"; }
need gh; need jq  # each script checks for its own model CLI (claude, agy)

# model_id <sonnet|opus|fable>
model_id() {
  case "$1" in
    sonnet) echo claude-sonnet-5 ;;
    opus)   echo claude-opus-5 ;;
    fable)  echo claude-fable-5-1 ;;
    *) die "unknown model label '$1'" ;;
  esac
}

# size_row <S|M|L> -> prints "model turns budget reviewer" from the PROCESS.md size table
size_row() {
  local row
  row=$(grep -E "^\| $1 \|" "$PROCESS" | head -1) || true
  [ -n "$row" ] || die "size $1 not in $PROCESS"
  # columns: | Size | Fits when | Model | Max turns | Budget (USD) | Reviewed by |
  echo "$row" | awk -F'|' '{gsub(/^ +| +$/,"",$4); gsub(/^ +| +$/,"",$5); gsub(/^ +| +$/,"",$6); gsub(/^ +| +$/,"",$7); print $4, $5, $6, $7}'
}

# label <json> <prefix> -> value after prefix, or empty
label() { jq -r --arg p "$2" '.labels[].name | select(startswith($p)) | sub($p; "")' <<<"$1" | head -1; }

kickoff() {
  cat <<EOP
Read CLAUDE.md, docs/PROCESS.md and docs/SPEC.md in full before writing code.
Implement issue #$1 exactly as scoped; do not start later issues or refactor beyond it.
Run \`just check\` after every edit, and \`just test\` + \`just lint\` before you finish.
Work on the current branch only; commit as you go. When done, push the branch and open a PR with
\`gh pr create --label "$2"\` whose body starts with "Closes #$1" and lists each acceptance criterion
with the command you ran to verify it. Never merge, never force-push.
If the spec and the issue disagree, the issue wins; say so in the PR.
You have a budget of about $3 turns; if you cannot finish inside it, commit what is green and say what remains in the PR.
EOP
}
