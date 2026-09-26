#!/usr/bin/env python3
"""Split a review prompt into passes that fit the reviewer's input (used by scripts/review.sh).

usage: review_passes.py <template> <diff> <copilot.md> <copilot-state> <max-bytes> <outdir>

template: the prompt with @@PASS@@, @@COPILOT_INSTRUCTION@@, @@DIFF@@ and @@COPILOT@@ placeholders,
each on its own line. diff: the PR diff (lockfiles already summarised by review.sh). copilot.md:
scripts/copilot_findings.py output, or empty. copilot-state: present | none | skipped.
max-bytes: the largest prompt to send, 0 for no cap.

agy drops input past about 192 KB and Gemini reviews what arrived without saying so (measured
2026-09-26 with numbered lines; PR #48's 217 KB prompt lost its last 9 files and still passed), so
a prompt over the cap is split by file, in diff order, into passes that each carry the whole
template (issue, PR, SPEC) plus their files' diff and the Copilot findings anchored in those files.
Copilot's overview rides with pass 1; findings on files not in the diff ride with the last pass.

Writes <outdir>/pass-<k>.md and prints one line per pass: "<k> <bytes> <files>".
Exits 2 when one file's diff cannot fit a pass on its own.
"""

import re
import sys

PASS_NOTE = (
    "This review is split into {K} passes because the whole diff does not fit one prompt. This is "
    "pass {k}: the diff below covers only {files}; the other files are reviewed in other passes. "
    'For an acceptance criterion whose evidence is not in these files, write "not in this pass" '
    "rather than guessing."
)
COPILOT_PRESENT = (
    "GitHub Copilot has reviewed this PR; its findings for the files in this prompt are at the end, "
    "numbered as in Copilot's review (C1..Cn across the whole PR). For EACH one, give a verdict with "
    "evidence from the diff: VALID (still present at the PR head), FIXED (the head no longer has "
    "it), or FALSE (Copilot is wrong; say why). A VALID finding about correctness, safety or a "
    "CLAUDE.md / SPEC rule is a blocking finding and must be counted below; a VALID low-impact nit "
    "may be listed as non-blocking with a reason. Then add one line: COPILOT: <valid> valid, "
    "<fixed> fixed, <false> false."
)
COPILOT_ELSEWHERE = (
    "GitHub Copilot has reviewed this PR, but none of its findings concern the files in this pass; "
    "there is nothing to triage here."
)
COPILOT_NONE = (
    "GitHub Copilot had not reviewed this PR when this review started; there are no Copilot "
    "findings to triage."
)
COPILOT_SKIPPED = (
    "Copilot's findings, if any, are left out of this review on purpose; there is nothing to triage."
)

CHUNK_START = re.compile(r"^(?:diff --git a/(\S+) b/|\[(\S+): \d+ changed lines omitted)")
FINDING = re.compile(r"^### C\d+(?: \[[A-Za-z]+\])? (.+):(\S+) \(")


def split_diff(diff: str) -> list[tuple[str, str]]:
    """The diff as (path, text) per file, in diff order."""
    chunks: list[tuple[str | None, list[str]]] = []
    for line in diff.splitlines(keepends=True):
        m = CHUNK_START.match(line)
        if m or not chunks:
            chunks.append((m.group(1) or m.group(2) if m else None, []))
        chunks[-1][1].append(line)
    if len(chunks) > 1 and chunks[0][0] is None:  # a preamble joins the first file
        chunks[1] = (chunks[1][0], chunks[0][1] + chunks[1][1])
        del chunks[0]
    return [(p or "(diff)", "".join(lines)) for p, lines in chunks]


def split_copilot(md: str) -> tuple[str, dict[str, list[str]]]:
    """copilot_findings.py output as (overview markdown, {path: [finding markdown]})."""
    overview: list[str] = []
    by_path: dict[str, list[str]] = {}
    block: list[str] = []
    target: list[str] | None = None
    for line in md.splitlines(keepends=True):
        if line.startswith("<!-- copilot-inline-count"):
            continue
        m = FINDING.match(line)
        if line.startswith("## Copilot overview") or m:
            if target is not None:
                target.extend(block)
            block = []
            target = overview if not m else by_path.setdefault(m.group(1), [])
        if target is not None:
            block.append(line)
    if target is not None:
        target.extend(block)
    return "".join(overview), by_path


def main() -> None:
    template_path, diff_path, copilot_path, state, limit_s, outdir = sys.argv[1:7]
    limit = int(limit_s)
    read = lambda p: open(p, encoding="utf-8", errors="replace").read()  # noqa: E731
    template, diff, copilot_md = read(template_path), read(diff_path), read(copilot_path)
    chunks = split_diff(diff) if diff.strip() else []
    overview, by_path = split_copilot(copilot_md) if copilot_md.strip() else ("", {})
    in_diff = {p for p, _ in chunks}
    stray = [t for p, ts in by_path.items() if p not in in_diff for t in ts]

    def render(k: int, total: int, idxs: list[int], last: bool) -> str:
        paths = [chunks[i][0] for i in idxs]
        copilot = (overview if k == 1 else "") + "".join(t for p in paths for t in by_path.get(p, []))
        if last:
            copilot += "".join(stray)
        if copilot:
            instruction = COPILOT_PRESENT
        elif state == "present":
            instruction = COPILOT_ELSEWHERE
        elif state == "none":
            instruction = COPILOT_NONE
        else:
            instruction = COPILOT_SKIPPED
        note = "" if total == 1 else PASS_NOTE.format(K=total, k=k, files=", ".join(paths)) + "\n"
        out = template.replace("@@PASS@@\n", note, 1)
        out = out.replace("@@COPILOT_INSTRUCTION@@\n", instruction + "\n", 1)
        section = "\n---\n# GitHub Copilot review of this PR\n\n" + copilot if copilot else ""
        out = out.replace("@@COPILOT@@", section, 1)
        return out.replace("@@DIFF@@", "".join(chunks[i][1] for i in idxs), 1)

    def size(k: int, total: int, idxs: list[int]) -> int:
        return len(render(k, total, idxs, last=True).encode("utf-8"))

    everything = list(range(len(chunks)))
    if limit == 0 or size(1, 1, everything) <= limit:
        passes = [everything]
    else:
        passes, cur = [], []
        for i in everything:
            if size(len(passes) + 1, 99, cur + [i]) <= limit:
                cur.append(i)
                continue
            if cur:
                passes.append(cur)
            cur = [i]
            if size(len(passes) + 1, 99, cur) > limit:
                path, text = chunks[i]
                print(
                    f"error: the diff of {path} ({len(text.encode('utf-8'))} bytes) does not fit one "
                    f"review pass of {limit} bytes with the issue, PR and SPEC; raise --max-prompt-bytes "
                    "(0 = no cap; agy drops input past about 192 KB) or review that file by hand",
                    file=sys.stderr,
                )
                sys.exit(2)
        passes.append(cur)

    total = len(passes)
    for k, idxs in enumerate(passes, 1):
        text = render(k, total, idxs, last=(k == total))
        with open(f"{outdir}/pass-{k}.md", "w", encoding="utf-8") as f:
            f.write(text)
        print(k, len(text.encode("utf-8")), " ".join(chunks[i][0] for i in idxs))


if __name__ == "__main__":
    main()
