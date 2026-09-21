#!/usr/bin/env bash
# just review <pr> [--post] [--via claude|agy] [--model <id>]
# Fresh session of the reviewing model per docs/PROCESS.md § 5. Exits 0 only on "BLOCKING: 0 findings".
# --via agy runs the same prompt through the agy CLI (Gemini by default: a different model family).
source "$(dirname "$0")/lib.sh"

pr=""; post=0; via="${DMN_REVIEWER:-claude}"; model_override=""
while [ $# -gt 0 ]; do case "$1" in
  --post) post=1; shift ;;
  --via) via="$2"; shift 2 ;;
  --model) model_override="$2"; shift 2 ;;
  -*) die "unknown flag $1" ;;
  *) pr="$1"; shift ;;
esac; done
[ -n "$pr" ] || die "usage: review.sh <pr-number> [--post] [--via claude|agy] [--model <id>]"

prj=$(gh pr view "$pr" --json number,title,body,labels,headRefName)
size=$(label "$prj" "size:")
n=$(jq -r .body <<<"$prj" | grep -oiE 'closes #[0-9]+' | head -1 | grep -oE '[0-9]+' || true)
[ -n "$n" ] || n=$(jq -r .headRefName <<<"$prj" | grep -oE '^issue/[0-9]+' | grep -oE '[0-9]+' || true)
[ -n "$n" ] || die "cannot find the issue number in the PR body (Closes #n) or branch name"
[ -n "$size" ] || size=$(gh issue view "$n" --json labels | jq -r '.labels[].name | select(startswith("size:")) | sub("size:";"")')
[ -n "$size" ] || die "no size label on PR #$pr or issue #$n"
read -r _ _ _ reviewer <<<"$(size_row "$size")"
case "$via" in
  claude) model=$(model_id "$(tr '[:upper:]' '[:lower:]' <<<"$reviewer" | grep -oE 'sonnet|opus|fable' | head -1)") ;;
  agy)    need agy; model="gemini-3.1-pro-high" ;;
  *) die "unknown reviewer backend '$via' (claude|agy)" ;;
esac
[ -n "$model_override" ] && model="$model_override"

issue=$(gh issue view "$n" --json title,body)
diff=$(gh pr diff "$pr")
ro="You may read files in the repo but must not edit anything."
[ "$via" = agy ] && ro="Everything you need is in this prompt. Do not run shell commands or tools (they are denied in this mode); reading the diff and documents below is sufficient. Do not edit anything."
prompt="You are reviewing PR #$pr for issue #$n of the Daemon repo. $ro

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
echo "reviewing PR #$pr (issue #$n, size $size) via $via/$model -> $out"
cd "$ROOT"
# The prompt (diff + SPEC) exceeds the argv limit, so it always travels over stdin.
pf=$(mktemp "$RUNS/prompt-$pr-XXXX.md"); printf '%s' "$prompt" > "$pf"
case "$via" in
  claude) claude -p --model "$model" --permission-mode dontAsk --allowedTools "Read,Glob,Grep" \
            --output-format text < "$pf" > "$out" 2>&1 || true ;;
  agy)    python3 -c 'import json,sys; print(json.dumps({"event":"user","message":{"role":"user","content":sys.stdin.read()}}))' < "$pf" \
            | agy --model "$model" --mode plan --print-timeout 15m --input-format stream-json --output-format stream-json -p='' 2>"$out.err" \
            | python3 -c 'import json,sys
for line in sys.stdin:
    try: ev=json.loads(line)
    except ValueError: continue
    if ev.get("event")=="result":
        r=ev["result"]; print(r.get("response") or "(no response; status %s: %s)" % (r.get("status"), r.get("error")))' > "$out" || true
          [ -s "$out" ] || cat "$out.err" >> "$out" ;;
esac
rm -f "$pf"
cat "$out"
if [ "$post" = 1 ]; then gh pr comment "$pr" --body-file "$out"; fi
tail -1 "$out" | grep -qE '^BLOCKING: 0 findings' && exit 0
exit 1
