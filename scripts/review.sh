#!/usr/bin/env bash
# just review <pr> [--post] [--via agy|claude] [--model <id>] [--no-copilot] [--copilot-wait <s>] [--max-prompt-bytes <n>] [--prompt-only]
# One review per PR (docs/PROCESS.md § 5): a fresh session of the reviewer gets the diff, the issue,
# docs/SPEC.md and the prompt below, and this exits 0 only on "BLOCKING: 0 findings".
# --via agy (the default) runs the prompt through the agy CLI: Claude Opus 4.6 (Thinking), agy's
# strongest reviewer since Gemini 3.1 Pro left it (2026-10-21), on agy's quota, not Claude Code's.
# --via claude uses the reviewer from the size table.
# GitHub Copilot reviews PRs on its own schedule and nothing here waits for it: if its review exists
# when this runs, every finding goes into the prompt to be triaged VALID / FIXED / FALSE; if not,
# the review proceeds without it and the prompt says so. --copilot-wait <s> waits that long first;
# --no-copilot leaves Copilot's findings out even when present.
# agy drops input past about 192 KB without saying so (measured 2026-09-26; PR #48's 217 KB prompt
# lost 9 files and still passed), so a prompt over --max-prompt-bytes (default 160000, 0 = no cap)
# is split by file into several passes by scripts/review_passes.py; the verdict is the total over
# all passes, and a pass that ends without a BLOCKING line fails the review.
source "$(dirname "$0")/lib.sh"

pr=""; post=0; via="${DMN_REVIEWER:-agy}"; model_override=""; copilot=1; copilot_wait=0; prompt_only=0
max_bytes="${DMN_REVIEW_MAX_PROMPT_BYTES:-160000}"
while [ $# -gt 0 ]; do case "$1" in
  --post) post=1; shift ;;
  --via) via="$2"; shift 2 ;;
  --model) model_override="$2"; shift 2 ;;
  --no-copilot) copilot=0; shift ;;
  --copilot-wait) copilot_wait="$2"; shift 2 ;;
  --max-prompt-bytes) max_bytes="$2"; shift 2 ;;
  --prompt-only) prompt_only=1; shift ;;
  -*) die "unknown flag $1" ;;
  *) pr="$1"; shift ;;
esac; done
[ -n "$pr" ] || die "usage: review.sh <pr-number> [--post] [--via agy|claude] [--model <id>] [--no-copilot] [--copilot-wait <s>] [--max-prompt-bytes <n>] [--prompt-only]"

prj=$(gh pr view "$pr" --json number,title,body,labels,headRefName,headRefOid)
head_sha=$(jq -r .headRefOid <<<"$prj")
repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
size=$(label "$prj" "size:")
n=$(jq -r .body <<<"$prj" | grep -oiE 'closes #[0-9]+' | head -1 | grep -oE '[0-9]+' || true)
[ -n "$n" ] || n=$(jq -r .headRefName <<<"$prj" | grep -oE '^issue/[0-9]+' | grep -oE '[0-9]+' || true)
[ -n "$n" ] || die "cannot find the issue number in the PR body (Closes #n) or branch name"
[ -n "$size" ] || size=$(gh issue view "$n" --json labels | jq -r '.labels[].name | select(startswith("size:")) | sub("size:";"")')
[ -n "$size" ] || die "no size label on PR #$pr or issue #$n"
read -r _ _ _ reviewer <<<"$(size_row "$size")"
case "$via" in
  claude) need claude; model=$(model_id "$(tr '[:upper:]' '[:lower:]' <<<"$reviewer" | grep -oE 'sonnet|opus|fable' | head -1)") ;;
  agy)    need agy; model="claude-opus-4-6-thinking" ;;
  *) die "unknown reviewer backend '$via' (agy|claude)" ;;
esac
[ -n "$model_override" ] && model="$model_override"

issue=$(gh issue view "$n" --json title,body)
# Lockfiles and generated fixture data are summarised, not inlined: they bury the code under
# hundreds of KB and reviewers start missing files at the end of the diff.
diff=$(gh pr diff "$pr" | python3 -c '
import re, sys
skip = re.compile(r"(^|/)(Cargo\.lock|uv\.lock|package-lock\.json)$|^testdata/.*\.(json|md)$")
out, cur, keep, n = [], None, True, 0
def flush():
    if cur and not keep: out.append(f"[{cur}: {n} changed lines omitted from review: lockfile or generated fixture data]\n")
for line in sys.stdin:
    m = re.match(r"diff --git a/\S+ b/(\S+)", line)
    if m:
        flush(); cur, n = m.group(1), 0; keep = not skip.search(cur)
    if keep: out.append(line)
    elif line[:1] in "+-" and not line.startswith(("+++", "---")): n += 1
flush(); sys.stdout.write("".join(out))')

# GitHub Copilot's review, if it has posted one: overview + inline comments, each tagged with the
# commit it was made on. Never waited for unless --copilot-wait says so.
copilot_md=""; copilot_state="skipped"
if [ "$copilot" = 1 ]; then
  has_review() { [ "$(gh api "repos/$repo/pulls/$pr/reviews" --paginate -q '[.[] | select(.user.login | test("copilot"; "i"))] | length' | awk '{s+=$1} END {print s+0}')" -gt 0 ]; }
  found=0; waited=0
  has_review && found=1
  while [ "$found" = 0 ] && [ "$waited" -lt "$copilot_wait" ]; do
    [ "$waited" = 0 ] && echo "waiting up to ${copilot_wait}s for a Copilot review on PR #$pr ..."
    step=$((copilot_wait - waited)); [ "$step" -gt 20 ] && step=20
    sleep "$step"; waited=$((waited + step))
    has_review && found=1
  done
  if [ "$found" = 1 ]; then
    copilot_md=$( { gh api "repos/$repo/pulls/$pr/reviews" --paginate -q '.[] | select(.user.login | test("copilot"; "i")) | {kind: "review", commit: .commit_id, body: .body} | @json'
                    gh api "repos/$repo/pulls/$pr/comments" --paginate -q '.[] | select(.user.login | test("copilot"; "i")) | {kind: "inline", commit: .commit_id, path: .path, line: (.line // .original_line), body: .body} | @json'; } \
      | python3 "$ROOT/scripts/copilot_findings.py" "$head_sha")
    copilot_state="present"
    copilot_n=$(grep -oE 'copilot-inline-count [0-9]+' <<<"$copilot_md" | grep -oE '[0-9]+$')
    copilot_desc="$copilot_n findings"
  else
    copilot_state="none"; copilot_desc="none yet, not waited for"
  fi
else
  copilot_desc="skipped"
fi

ro="You may read files in the repo but must not edit anything."
[ "$via" = agy ] && ro="Everything you need is in this prompt. Do not run shell commands or tools (they are denied in this mode); reading the diff and documents below is sufficient. Do not edit anything."
# review_passes.py fills the @@…@@ lines: the pass note (several passes only), the Copilot triage
# instruction, the diff for the pass and the Copilot findings anchored in its files.
template="You are reviewing PR #$pr for issue #$n of the Daemon repo. $ro
@@PASS@@
List every acceptance criterion from the issue and state whether the diff meets it, with evidence from the diff.
List blocking findings: correctness, safety (unsafe, RT callback rules, audio on disk), spec violations.
@@COPILOT_INSTRUCTION@@
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
@@DIFF@@
@@COPILOT@@"

passdir=$(mktemp -d "$RUNS/prompt-$pr-XXXX")
printf '%s' "$template" > "$passdir/template.md"
printf '%s' "$diff" > "$passdir/diff.md"
printf '%s' "$copilot_md" > "$passdir/copilot.md"
python3 "$ROOT/scripts/review_passes.py" "$passdir/template.md" "$passdir/diff.md" "$passdir/copilot.md" \
  "$copilot_state" "$max_bytes" "$passdir" > "$passdir/passes.txt" || { rm -rf "$passdir"; die "could not build the review prompt (see above)"; }
mapfile -t passes < "$passdir/passes.txt"
K=${#passes[@]}

if [ "$prompt_only" = 1 ]; then
  for line in "${passes[@]}"; do
    read -r k bytes files <<<"$line"
    [ "$K" -gt 1 ] && echo "=== pass $k/$K ($bytes bytes): $files" >&2
    cat "$passdir/pass-$k.md"; echo
  done
  rm -rf "$passdir"; exit 0
fi

out="$RUNS/review-$pr-$(date +%Y%m%d-%H%M%S).md"
echo "reviewing PR #$pr (issue #$n, size $size) via $via/$model, copilot: $copilot_desc, passes: $K -> $out"
cd "$ROOT"
: > "$out"; : > "$out.err"
total=0; incomplete=0
for line in "${passes[@]}"; do
  read -r k bytes files <<<"$line"
  pf="$passdir/pass-$k.md"; po="$passdir/out-$k.md"
  [ "$K" -gt 1 ] && echo "pass $k/$K ($bytes bytes): $files"
  # The prompt (diff + SPEC) exceeds the argv limit, so it always travels over stdin.
  case "$via" in
    claude) claude -p --model "$model" --permission-mode dontAsk --allowedTools "Read,Glob,Grep" \
              --output-format text < "$pf" > "$po" 2>&1 || true ;;
    agy)    python3 -c 'import json,sys; print(json.dumps({"event":"user","message":{"role":"user","content":sys.stdin.read()}}))' < "$pf" \
              | agy --model "$model" --mode plan --print-timeout 15m --input-format stream-json --output-format stream-json -p='' 2>>"$out.err" \
              | python3 -c 'import json,sys
for line in sys.stdin:
    try: ev=json.loads(line)
    except ValueError: continue
    if ev.get("event")=="result":
        r=ev["result"]; print(r.get("response") or "(no response; status %s: %s)" % (r.get("status"), r.get("error")))' > "$po" || true
            [ -s "$po" ] || cat "$out.err" >> "$po" ;;
  esac
  [ "$K" -gt 1 ] && printf '# Review pass %s/%s: %s\n\n' "$k" "$K" "$files" >> "$out"
  cat "$po" >> "$out"; echo >> "$out"
  b=$(grep -E '^BLOCKING: [0-9]+ findings' "$po" | tail -1 | grep -oE '[0-9]+' | head -1 || true)
  if [ -n "$b" ]; then total=$((total + b)); else incomplete=$((incomplete + 1)); echo "error: pass $k/$K ended without a BLOCKING line" >&2; fi
done
rm -rf "$passdir"
if [ "$incomplete" -gt 0 ]; then
  printf 'REVIEW INCOMPLETE: %s of %s passes ended without a BLOCKING line; no verdict.\n' "$incomplete" "$K" | tee -a "$out"
elif [ "$K" -gt 1 ]; then
  printf '# All %s passes\n\nBLOCKING: %s findings\n' "$K" "$total" >> "$out"
fi
cat "$out"
if [ "$post" = 1 ]; then gh pr comment "$pr" --body-file "$out"; fi
[ "$incomplete" = 0 ] || exit 1
grep -E '^BLOCKING: [0-9]+ findings' "$out" | tail -1 | grep -qE '^BLOCKING: 0 findings' && exit 0
exit 1
