#!/usr/bin/env python3
"""Format GitHub Copilot review data for the reviewer prompt (used by scripts/review.sh).

stdin: JSON lines, each {"kind": "review"|"inline", "commit", "body", ["path", "line"]}.
argv[1]: the PR head commit SHA.
stdout: Markdown with the Copilot overview(s) and inline findings numbered C1..Cn, each tagged
with whether it is still anchored at the PR head or outdated, then a trailing
`<!-- copilot-inline-count N -->` marker.
"""

import json
import re
import sys


def clean(text: str) -> tuple[str | None, str]:
    text = re.sub(r"<!--.*?-->", "", text, flags=re.S)
    sev = re.search(r'alt="(Critical|High|Medium|Low) severity"', text)
    text = re.sub(r"<picture>.*?</picture>|<[^>]+>", "", text, flags=re.S)
    text = re.sub(r"\*Get a fresh assessment.*", "", text)
    text = re.sub(r"\n{3,}", "\n\n", text).strip()
    return (sev.group(1) if sev else None), text


def main() -> None:
    head = sys.argv[1]
    n = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        c = json.loads(line)
        commit = c.get("commit") or ""
        if commit == head:
            where = "still anchored at the PR head: the commented line is unchanged"
        else:
            where = f"outdated: anchored at {commit[:7]}, the commented code has changed since; check whether the problem survived"
        sev, body = clean(c.get("body") or "")
        if c["kind"] == "review":
            if body:
                print(f"## Copilot overview ({where})\n\n{body}\n")
        else:
            n += 1
            tag = f" [{sev}]" if sev else ""
            print(f"### C{n}{tag} {c.get('path')}:{c.get('line')} ({where})\n\n{body}\n")
    print(f"<!-- copilot-inline-count {n} -->")


if __name__ == "__main__":
    main()
