#!/usr/bin/env bash
# just review <pr> [--post] [--via claude|agy] [--model <id>] [--no-copilot] [--copilot-wait <s>] [--prompt-only]
# Fresh session of the reviewing model per docs/PROCESS.md § 5. Exits 0 only on "BLOCKING: 0 findings".
# --via agy runs the same prompt through the agy CLI (Gemini by default: a different model family).
# GitHub Copilot's review of the PR is fetched and every finding must be triaged by the reviewer;
# the script waits up to --copilot-wait seconds (default 600) for Copilot to post, and without
# --no-copilot it refuses to pass a PR that has no Copilot review (exit 3).
source "$(dirname "$0")/lib.sh"

pr=""; post=0; via="${DMN_REVIEWER:-claude}"; model_override=""; copilot=1; copilot_wait=600; prompt_only=0
while [ $# -gt 0 ]; do case "$1" in
  --post) post=1; shift ;;
  --via) via="$2"; shift 2 ;;
  --model) model_override="$2"; shift 2 ;;
  --no-copilot) copilot=0; shift ;;
  --prompt-only) prompt_only=1; shift ;;
  --copilot-wait) copilot_wait="$2"; shift 2 ;;
  -*) die "unknown flag $1" ;;
  *) pr="$1"; shift ;;
esac; done
[ -n "$pr" ] || die "usage: review.sh <pr-number> [--post] [--via claude|agy] [--model <id>]"

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
  claude) model=$(model_id "$(tr '[:upper:]' '[:lower:]' <<<"$reviewer" | grep -oE 'sonnet|opus|fable' | head -1)") ;;
  agy)    need agy; model="gemini-3.1-pro-high" ;;
  *) die "unknown reviewer backend '$via' (claude|agy)" ;;
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
    m = re.match(r"diff --git a/(\S+) b/", line)
    if m:
        flush(); cur, n = m.group(1), 0; keep = not skip.search(cur)
    if keep: out.append(line)
    elif line[:1] in "+-" and not line.startswith(("+++", "---")): n += 1
flush(); sys.stdout.write("".join(out))')
# GitHub Copilot's review: overview + inline comments, each tagged with the commit it was made on.
copilot_md=""; copilot_n=0
if [ "$copilot" = 1 ]; then
  has_review() { [ "$(gh api "repos/$repo/pulls/$pr/reviews" --paginate -q '[.[] | select(.user.login | test("copilot"; "i"))] | length' | awk '{s+=$1} END {print s+0}')" -gt 0 ]; }
  waited=0
  until has_review; do
    [ "$waited" -ge "$copilot_wait" ] && break
    [ "$waited" = 0 ] && echo "waiting up to ${copilot_wait}s for a Copilot review on PR #$pr ..."
    sleep 20; waited=$((waited + 20))
  done
  has_review || { echo "error: no Copilot review on PR #$pr after ${copilot_wait}s; rerun later, request one in the PR, or pass --no-copilot" >&2; exit 3; }
  copilot_md=$( { gh api "repos/$repo/pulls/$pr/reviews" --paginate -q '.[] | select(.user.login | test("copilot"; "i")) | {kind: "review", commit: .commit_id, body: .body} | @json'
                  gh api "repos/$repo/pulls/$pr/comments" --paginate -q '.[] | select(.user.login | test("copilot"; "i")) | {kind: "inline", commit: .commit_id, path: .path, line: (.line // .original_line), body: .body} | @json'; } \
    | python3 "$ROOT/scripts/copilot_findings.py" "$head_sha")
  copilot_n=$(grep -oE 'copilot-inline-count [0-9]+' <<<"$copilot_md" | grep -oE '[0-9]+$')
fi

ro="You may read files in the repo but must not edit anything."
[ "$via" = agy ] && ro="Everything you need is in this prompt. Do not run shell commands or tools (they are denied in this mode); reading the diff and documents below is sufficient. Do not edit anything."
prompt="You are reviewing PR #$pr for issue #$n of the Daemon repo. $ro

List every acceptance criterion from the issue and state whether the diff meets it, with evidence from the diff.
List blocking findings: correctness, safety (unsafe, RT callback rules, audio on disk), spec violations.
$( [ "$copilot" = 1 ] && printf '%s' "GitHub Copilot has reviewed this PR; its $copilot_n inline findings (C1..C$copilot_n) are at the end of this prompt. For EACH one, give a verdict with evidence from the diff: VALID (still present at the PR head), FIXED (the head no longer has it), or FALSE (Copilot is wrong; say why). A VALID finding about correctness, safety or a CLAUDE.md / SPEC rule is a blocking finding and must be counted below; a VALID low-impact nit may be listed as non-blocking with a reason. Then add one line: COPILOT: <valid> valid, <fixed> fixed, <false> false." || true )
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
$diff
$( [ "$copilot" = 1 ] && printf '\n---\n# GitHub Copilot review of this PR\n\n%s' "$copilot_md" || true )"

if [ "$prompt_only" = 1 ]; then printf '%s\n' "$prompt"; exit 0; fi

out="$RUNS/review-$pr-$(date +%Y%m%d-%H%M%S).md"
echo "reviewing PR #$pr (issue #$n, size $size) via $via/$model, copilot findings: $( [ "$copilot" = 1 ] && echo "$copilot_n" || echo skipped ) -> $out"
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
grep -E '^BLOCKING: [0-9]+ findings' "$out" | tail -1 | grep -qE '^BLOCKING: 0 findings' && exit 0
exit 1
