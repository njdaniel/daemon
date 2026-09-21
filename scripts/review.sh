#!/usr/bin/env bash
# just review <pr> [--post]
# Fresh session of the reviewing model per docs/PROCESS.md § 5. Exits 0 only on "BLOCKING: 0 findings".
source "$(dirname "$0")/lib.sh"

pr=""; post=0
while [ $# -gt 0 ]; do case "$1" in --post) post=1; shift ;; -*) die "unknown flag $1" ;; *) pr="$1"; shift ;; esac; done
[ -n "$pr" ] || die "usage: review.sh <pr-number> [--post]"

prj=$(gh pr view "$pr" --json number,title,body,labels,headRefName)
size=$(label "$prj" "size:")
n=$(jq -r .body <<<"$prj" | grep -oiE 'closes #[0-9]+' | head -1 | grep -oE '[0-9]+' || true)
[ -n "$n" ] || n=$(jq -r .headRefName <<<"$prj" | grep -oE '^issue/[0-9]+' | grep -oE '[0-9]+' || true)
[ -n "$n" ] || die "cannot find the issue number in the PR body (Closes #n) or branch name"
[ -n "$size" ] || size=$(gh issue view "$n" --json labels | jq -r '.labels[].name | select(startswith("size:")) | sub("size:";"")')
[ -n "$size" ] || die "no size label on PR #$pr or issue #$n"
read -r _ _ _ reviewer <<<"$(size_row "$size")"
model=$(model_id "$(tr '[:upper:]' '[:lower:]' <<<"$reviewer" | grep -oE 'sonnet|opus|fable' | head -1)")

issue=$(gh issue view "$n" --json title,body)
diff=$(gh pr diff "$pr")
prompt="You are reviewing PR #$pr for issue #$n of the Daemon repo. You may read files in the repo but must not edit anything.

List every acceptance criterion from the issue and state whether the diff meets it, with evidence from the diff.
List blocking findings: correctness, safety (unsafe, RT callback rules, audio on disk), spec violations.
Do not restyle, do not suggest scope beyond the issue.
End with exactly one line: BLOCKING: <n> findings.

---
# Issue #$n: $(jq -r .title <<<"$issue")
$(jq -r .body <<<"$issue")

---
# PR #$pr: $(jq -r .title <<<"$prj")
$(jq -r .body <<<"$prj")

---
# docs/SPEC.md
$(cat "$ROOT/docs/SPEC.md")

---
# Diff
$diff"

out="$RUNS/review-$pr-$(date +%Y%m%d-%H%M%S).md"
echo "reviewing PR #$pr (issue #$n, size $size) with $model -> $out"
cd "$ROOT"
claude -p "$prompt" --model "$model" --permission-mode dontAsk --allowedTools "Read,Glob,Grep" \
  --output-format text > "$out" 2>&1 || true
cat "$out"
if [ "$post" = 1 ]; then gh pr comment "$pr" --body-file "$out"; fi
tail -1 "$out" | grep -qE '^BLOCKING: 0 findings' && exit 0
exit 1
