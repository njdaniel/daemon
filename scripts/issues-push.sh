#!/usr/bin/env bash
# just issues-push: create the required labels and one GitHub issue per docs/issues/NN-*.md (skips existing titles).
source "$(dirname "$0")/lib.sh"
gh repo view >/dev/null 2>&1 || die "no GitHub remote; create the repo first (gh repo create njdaniel/daemon --private --source . --push)"

mk() { gh label create "$1" --color "$2" --description "$3" --force >/dev/null; }
mk size:S 0e8a16 "one file/module, known crates";   mk size:M fbca04 "one crate, new tests or one async client"
mk size:L d93f0b "FFI, unsafe, threads or unfamiliar C API"
mk model:sonnet c5def5 "implemented by Sonnet"; mk model:opus 5319e7 "implemented by Opus"; mk model:fable 000000 "implemented by Fable"
for m in m0 m1 m2 m3 m4; do mk "$m" bfd4f2 "milestone $m"; done
for l in audio stt llm ui infra; do mk "lane:$l" e4e669 "lane $l"; done

existing=$(gh issue list --state all --limit 500 --json title -q '.[].title')

for f in "$ROOT"/docs/issues/[0-9]*-*.md; do
  title=$(head -1 "$f" | sed -E 's/^# #[0-9]+ — //')
  if printf '%s\n' "$existing" | grep -qxF "$title"; then
    echo "exists: $title"; continue
  fi
  labels=$(grep -m1 '^Labels:' "$f" | sed -E 's/^Labels: `//; s/`$//; s/, /,/g')
  body=$(sed -E '1,/^Labels:/d' "$f" | sed '1{/^$/d}')
  gh issue create --title "$title" --label "$labels" --body "$body" >/dev/null && echo "created: $title"
done
