#!/usr/bin/env bash
# just agent <n|next> [--dry-run]
# Runs one issue headlessly per docs/PROCESS.md § 4: branch issue/<n>-<slug>, claude -p, PR. Never merges.
source "$(dirname "$0")/lib.sh"

n=""; dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry=1; shift ;;
    -*) die "unknown flag $1" ;;
    *) n="$1"; shift ;;
  esac
done
[ -n "$n" ] || die "usage: agent.sh <issue-number|next> [--dry-run]"

deps_merged() { # <issue json> -> 0 if every "## Depends on" issue is closed
  local body deps d st
  body=$(jq -r .body <<<"$1")
  deps=$(awk '/^## Depends on/{f=1;next} /^## /{f=0} f' <<<"$body" | grep -oE '#[0-9]+' | tr -d '#' || true)
  for d in $deps; do
    st=$(gh issue view "$d" --json state -q .state)
    [ "$st" = "CLOSED" ] || { echo "  #$d is $st"; return 1; }
  done
}
lane_free() { # <lane> -> 0 if no open PR carries lane:<lane>
  [ "$(gh pr list --state open --label "lane:$1" --json number -q 'length')" = "0" ]
}

if [ "$n" = "next" ]; then
  for cand in $(gh issue list --state open --json number -q '.[].number' | sort -n); do
    j=$(gh issue view "$cand" --json number,labels,body)
    lane=$(label "$j" "lane:")
    if deps_merged "$j" >/dev/null && lane_free "$lane"; then n="$cand"; break; fi
  done
  [ "$n" != "next" ] || die "no open issue has all dependencies merged and a free lane"
  echo "dispatch: picked #$n"
fi

issue=$(gh issue view "$n" --json number,title,body,labels,state)
[ "$(jq -r .state <<<"$issue")" = "OPEN" ] || die "issue #$n is not open"
title=$(jq -r .title <<<"$issue")
size=$(label "$issue" "size:");  [ -n "$size" ] || die "issue #$n has no size:* label"
mlabel=$(label "$issue" "model:"); [ -n "$mlabel" ] || die "issue #$n has no model:* label"
lane=$(label "$issue" "lane:");   [ -n "$lane" ] || die "issue #$n has no lane:* label"
[ "$size" != "XL" ] || die "XL is not allowed; split the issue (PROCESS.md § 1)"

read -r tmodel turns reviewer <<<"$(size_row "$size")"
[ "$(tr '[:upper:]' '[:lower:]' <<<"$tmodel")" = "$mlabel" ] || die "label model:$mlabel disagrees with PROCESS.md size $size ($tmodel); fix the label"
deps_merged "$issue" || die "issue #$n has unmerged dependencies"
lane_free "$lane" || die "lane $lane has a PR in flight"

model=$(model_id "$mlabel")
slug=$(sed -E 's/^[a-z0-9-]+: //; s/`//g; s/[^a-zA-Z0-9]+/-/g; s/^-|-$//g' <<<"$title" | tr '[:upper:]' '[:lower:]' | cut -c1-40)
branch="issue/$n-$slug"
labels="size:$size,model:$mlabel,lane:$lane,$(label "$issue" "" | grep -E '^m[0-4]$' || true)"

# Allowed tools by size (PROCESS.md § 4.2) plus the commit-only contract.
base="Read,Glob,Grep,Edit,Write,Bash(just check),Bash(just test),Bash(just lint),Bash(just fmt),Bash(cargo check*),Bash(cargo test*),Bash(cargo clippy*),Bash(cargo fmt*),Bash(cargo add*),Bash(git status*),Bash(git diff*),Bash(git log*),Bash(git add*),Bash(git commit*),Bash(git push -u origin issue/*),Bash(gh pr create*),Bash(gh pr view*)"
[ "$size" = "L" ] && base="$base,Bash(cargo run -p dmn-cli -- replay*),Bash(cargo run -p dmn-cli -- doctor*),Bash(cargo run -p dmn-cli -- devices*),Bash(just probe),Bash(just replay*)"
disallowed="Bash(git push --force*),Bash(git push -f*),Bash(gh pr merge*),Bash(rm -rf*),WebFetch,WebSearch"

turns_flag=()
if claude --help 2>/dev/null | grep -q -- "--max-turns" && [[ "$turns" =~ ^[0-9]+ ]]; then turns_flag=(--max-turns "${turns%% *}"); fi

prompt="$(kickoff "$n" "$labels" "$turns")

---
# Issue #$n: $title

$(jq -r .body <<<"$issue")"

echo "issue:   #$n $title"
echo "size:    $size  model: $model  turns: $turns  reviewer: $reviewer"
echo "branch:  $branch"
if [ "$dry" = 1 ]; then echo "(dry run; not starting)"; exit 0; fi

cd "$ROOT"
git diff --quiet && git diff --cached --quiet || die "working tree not clean"
git switch main >/dev/null && git pull --ff-only >/dev/null
git switch -c "$branch"
log="$RUNS/$n-$(date +%Y%m%d-%H%M%S).log"
echo "log:     $log"
set +e
claude -p "$prompt" --model "$model" "${turns_flag[@]}" \
  --permission-mode dontAsk --allowedTools "$base" --disallowedTools "$disallowed" \
  --output-format text 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
set -e
echo "run exited $rc; log at $log"
git switch main >/dev/null 2>&1 || true
exit "$rc"
