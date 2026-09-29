#!/usr/bin/env python3
"""Recall, precision and detection latency for the question detector, end to end.

Runs `dmn replay --json` over each fixture in testdata/, matches the candidates the detector
emitted against that fixture's questions.json, and prints the numbers docs/SPEC.md sets targets
for (recall >= 0.85, precision >= 0.8 at M1; both >= 0.9 at M2; detection <= 100 ms). The exit
status follows the M2 targets.

This measures the whole pipeline: a question whisper mistranscribed badly enough that no cue
survives counts as a miss here, while `cargo test -p dmn-detect --test fixtures` scores the same
rules on the scripted text and so isolates the detector from the audio.

Usage:
    python3 eval/run.py                      # all fixtures, default detector
    python3 eval/run.py --compare            # also score the SPEC's literal cue table
    python3 eval/run.py --fixture mixed      # one fixture
    python3 eval/run.py --reuse              # reuse cached replay output
    python3 eval/run.py --features vulkan    # build the replay with GPU whisper (#85)

Stdlib only, by design: the harness must run on a clean checkout with no Python environment.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

# The checkout to measure. Normally the harness sits inside it; DMN_REPO lets CI, or a copy of
# this file kept outside the tree, point at one explicitly.
REPO = Path(os.environ.get("DMN_REPO") or Path(__file__).resolve().parent.parent)
FIXTURES = ("sysdesign", "behavioral", "mixed", "continuous")
# #77: fixtures spoken in another language. Their questions.json holds the English reference,
# so they are scored the same way, but only under `--language auto` (or their code) with a
# multilingual model such as `--model small`; they are not in the default run and are held to
# the M1 bar, since the text the detector sees is whisper's translation.
TRANSLATED = {"es": "es"}
# docs/SPEC.md § Milestones: M1 recall >= 0.85 / precision >= 0.8; M2 recall and precision >= 0.9.
M1_RECALL, M1_PRECISION = 0.85, 0.8
M2_RECALL, M2_PRECISION = 0.9, 0.9

# A candidate matches a scripted question when their audio spans overlap, or when the candidate
# starts within this much of the question's span. Whisper's utterance bounds differ from the
# script's by a VAD frame or two, and a 12 s forced split can start a little late.
MATCH_SLACK_S = 2.0
# Below this word-level similarity the texts are different questions that happen to sit close in
# time, so the pairing is rejected.
MIN_SIMILARITY = 0.35
# docs/SPEC.md: final segment to candidate.
LATENCY_BUDGET_MS = 100


@dataclass
class Candidate:
    start: float
    end: float
    text: str
    line: str
    score: int
    cues: list[str]
    detect_us: int
    # #63: this candidate continues the one before it (the same question, split at a pause), so
    # it replaces that one on screen rather than adding a second question.
    continues: bool = False


def collapse(candidates: list[Candidate]) -> list[Candidate]:
    """Fold each continuation into the candidate it continues: one question, the widest span,
    the combined text. The screen shows one question for the chain, so the eval counts one."""
    out: list[Candidate] = []
    for c in candidates:
        if c.continues and out:
            prev = out[-1]
            out[-1] = Candidate(
                start=min(prev.start, c.start),
                end=max(prev.end, c.end),
                text=c.text,
                line=c.line,
                score=max(prev.score, c.score),
                cues=c.cues,
                detect_us=max(prev.detect_us, c.detect_us),
                continues=False,
            )
        else:
            out.append(c)
    return out


@dataclass
class Question:
    t: float
    end: float
    text: str
    kind: str


@dataclass
class Result:
    fixture: str
    matched: list[tuple[Question, Candidate]] = field(default_factory=list)
    missed: list[Question] = field(default_factory=list)
    spurious: list[Candidate] = field(default_factory=list)
    summary: dict = field(default_factory=dict)
    wall_s: float = 0.0

    @property
    def recall(self) -> float:
        total = len(self.matched) + len(self.missed)
        return len(self.matched) / total if total else 0.0

    @property
    def precision(self) -> float:
        total = len(self.matched) + len(self.spurious)
        return len(self.matched) / total if total else 0.0

    @property
    def f1(self) -> float:
        r, p = self.recall, self.precision
        return 2 * r * p / (r + p) if r + p else 0.0

    @property
    def slowest_detect_ms(self) -> float:
        return max((c.detect_us for _, c in self.matched), default=0) / 1000.0


def normalise(text: str) -> list[str]:
    keep = "".join(ch.lower() if ch.isalnum() or ch.isspace() else " " for ch in text)
    return keep.split()


def similarity(a: str, b: str) -> float:
    return difflib.SequenceMatcher(None, normalise(a), normalise(b)).ratio()


def overlaps(q: Question, c: Candidate) -> bool:
    if c.start < q.end + MATCH_SLACK_S and q.t < c.end + MATCH_SLACK_S:
        return True
    return False


def fingerprint(args) -> str:
    """What replay's output depends on: the flags passed to it, the source tree (HEAD plus any
    uncommitted diff) and the user's config file. A cached run is reused only when all match."""
    h = hashlib.sha256()
    h.update(
        repr(
            (args.speed, args.model, args.threads, args.spec_literal, args.vad_close_ms, args.vad, args.language, args.features)
        ).encode()
    )
    for cmd in (["git", "rev-parse", "HEAD"], ["git", "diff", "HEAD"]):
        out = subprocess.run(cmd, cwd=REPO, capture_output=True, check=False)
        h.update(out.stdout)
    config = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "dmn" / "config.toml"
    if config.is_file():
        h.update(config.read_bytes())
    return h.hexdigest()[:12]


def replay(fixture: str, cache: Path, args) -> list[dict]:
    """Run `dmn replay --json` (or reuse its cached output) and return the parsed events."""
    cached = cache / f"{fixture}{'-literal' if args.spec_literal else ''}-{fingerprint(args)}.ndjson"
    if args.reuse and cached.is_file():
        return [json.loads(line) for line in cached.read_text().splitlines() if line.strip()]

    cmd = [
        "cargo",
        "run",
        "--release",
        "--quiet",
        "-p",
        "dmn-cli",
        # #85: the same features `just build` picked, so this run neither replaces a GPU binary
        # with a CPU one nor transcribes large models on the CPU.
        *(["--features", args.features] if args.features else []),
        "--",
        "replay",
        str(REPO / "testdata" / fixture / "interviewer.wav"),
        str(REPO / "testdata" / fixture / "candidate.wav"),
        "--json",
        # The eval scores the detector; the LLM fast phase would only add wall time, and the
        # session log would add three sessions to the user's history on every run.
        "--no-llm",
        "--no-store",
        "--speed",
        str(args.speed),
    ]
    if args.model:
        cmd += ["--model", args.model]
    if args.threads:
        cmd += ["--threads", str(args.threads)]
    if args.vad_close_ms:
        cmd += ["--vad-close-ms", str(args.vad_close_ms)]
    if args.vad:
        cmd += ["--vad", args.vad]
    if args.language:
        cmd += ["--language", args.language]
    if args.spec_literal:
        cmd += ["--spec-literal-detector"]

    proc = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        sys.exit(f"replay failed for {fixture}:\n{proc.stderr.strip()[-2000:]}")
    cache.mkdir(parents=True, exist_ok=True)
    cached.write_text(proc.stdout)
    return [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]


def evaluate(fixture: str, events: list[dict], wall_s: float) -> Result:
    questions = [
        Question(q["t"], q["end"], q["text"], q.get("kind", "?"))
        for q in json.loads((REPO / "testdata" / fixture / "questions.json").read_text())
    ]
    candidates = collapse(
        [
            Candidate(
                e["start"],
                e["end"],
                e["text"],
                e.get("line", ""),
                e["score"],
                e["cues"],
                e["detect_us"],
                bool(e.get("continues", False)),
            )
            for e in events
            if e.get("type") == "candidate"
        ]
    )
    summary = next((e for e in events if e.get("type") == "summary"), {})

    # Greedy best-first pairing: the closest, most similar candidate wins each question, and no
    # candidate is used twice.
    pairs = []
    for qi, q in enumerate(questions):
        for ci, c in enumerate(candidates):
            if not overlaps(q, c):
                continue
            sim = max(similarity(q.text, c.text), similarity(q.text, c.line or c.text))
            if sim < MIN_SIMILARITY:
                continue
            pairs.append((sim, -abs(q.t - c.start), qi, ci))
    pairs.sort(reverse=True)

    taken_q: set[int] = set()
    taken_c: set[int] = set()
    result = Result(fixture=fixture, summary=summary, wall_s=wall_s)
    for _sim, _prox, qi, ci in pairs:
        if qi in taken_q or ci in taken_c:
            continue
        taken_q.add(qi)
        taken_c.add(ci)
        result.matched.append((questions[qi], candidates[ci]))
    result.missed = [q for i, q in enumerate(questions) if i not in taken_q]
    result.spurious = [c for i, c in enumerate(candidates) if i not in taken_c]
    result.matched.sort(key=lambda m: m[0].t)
    return result


def short(text: str, width: int = 86) -> str:
    one = " ".join(text.split())
    return one if len(one) <= width else one[: width - 1] + "…"


def report(results: list[Result], label: str) -> tuple[float, float]:
    print(f"\n=== {label} ===")
    print(f"{'fixture':<11} {'recall':>7} {'precision':>10} {'F1':>6} {'hit':>4} {'miss':>5} {'FP':>4} {'detect':>9}")
    m = s = mi = 0
    for r in results:
        print(
            f"{r.fixture:<11} {r.recall:>7.2f} {r.precision:>10.2f} {r.f1:>6.2f} "
            f"{len(r.matched):>4} {len(r.missed):>5} {len(r.spurious):>4} {r.slowest_detect_ms:>7.2f}ms"
        )
        m += len(r.matched)
        s += len(r.spurious)
        mi += len(r.missed)
    recall = m / (m + mi) if m + mi else 0.0
    precision = m / (m + s) if m + s else 0.0
    f1 = 2 * recall * precision / (recall + precision) if recall + precision else 0.0
    print(f"{'overall':<11} {recall:>7.2f} {precision:>10.2f} {f1:>6.2f} {m:>4} {mi:>5} {s:>4}")
    return recall, precision


def detail(results: list[Result]) -> None:
    for r in results:
        if not (r.missed or r.spurious):
            continue
        print(f"\n--- {r.fixture} ---")
        for q in r.missed:
            print(f"  MISS  [{q.kind}] {short(q.text)}")
        for c in r.spurious:
            print(f"  FP    score {c.score} [{' '.join(c.cues)}] {short(c.text)}")


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument(
        "--fixture",
        action="append",
        choices=FIXTURES + tuple(TRANSLATED),
        help="default: the four English fixtures; es needs --language auto (or es) and a multilingual --model",
    )
    p.add_argument("--speed", default=8, type=float, help="replay pacing factor: N plays the audio at N x real time (default 8)")
    p.add_argument("--model", help="whisper model; default is [stt].model")
    p.add_argument("--threads", type=int, help="decoder threads")
    p.add_argument("--vad-close-ms", type=int, help="silence (ms) that ends an utterance; default [stt].vad_close_ms")
    p.add_argument("--vad", choices=("energy", "silero"), help="voice activity detector; default [stt].vad")
    p.add_argument("--language", help="en, auto or a language code (#77); default [stt].language")
    p.add_argument("--reuse", action="store_true", help="reuse cached replay output when present")
    p.add_argument("--features", help="cargo features for the replay build, e.g. vulkan (#85); default none (CPU)")
    p.add_argument("--compare", action="store_true", help="also score the SPEC's literal cue table")
    p.add_argument("--spec-literal", action="store_true", help="score only the SPEC's literal table")
    p.add_argument("--cache", default=None, help="directory for replay output (default: eval/.cache)")
    args = p.parse_args()

    fixtures = args.fixture or list(FIXTURES)
    if not args.language and any(f in TRANSLATED for f in fixtures):
        sys.exit("a translated fixture needs --language auto (or its code) and a multilingual --model such as small")
    cache = Path(args.cache) if args.cache else Path(__file__).resolve().parent / ".cache"

    def run_all(literal: bool) -> list[Result]:
        args.spec_literal = literal
        out = []
        for f in fixtures:
            started = time.monotonic()
            events = replay(f, cache, args)
            out.append(evaluate(f, events, time.monotonic() - started))
        return out

    wanted_literal = args.spec_literal
    results = run_all(wanted_literal)
    label = "SPEC cue table as written" if wanted_literal else "detector (default config)"
    recall, precision = report(results, label)
    detail(results)

    if args.compare and not wanted_literal:
        report(run_all(True), "SPEC cue table as written")

    pipeline = [r.summary for r in results if r.summary]
    # Final-to-candidate time for candidates emitted on arrival. A line that only clears the bar
    # once the next final shows it was followed (>= 1.5 s of silence or the candidate answering)
    # cannot meet a 100 ms budget by definition, so those are counted, not timed.
    slowest = max((s.get("detect_slowest_us", 0) for s in pipeline), default=0) / 1000.0
    late = sum(s.get("candidates_late", 0) for s in pipeline)
    if pipeline:
        lines = sum(s.get("lines", 0) for s in pipeline)
        print(
            f"\npipeline: {lines} transcript lines, slowest final-to-candidate {slowest:.2f} ms "
            f"(budget {LATENCY_BUDGET_MS} ms), {late} candidate(s) emitted once followed"
        )
    else:
        print("\npipeline: no replay summary, so latency is unmeasured")

    fast = bool(pipeline) and slowest <= LATENCY_BUDGET_MS

    def totals(rs: list[Result]) -> tuple[float, float]:
        m = sum(len(r.matched) for r in rs)
        mi = sum(len(r.missed) for r in rs)
        s = sum(len(r.spurious) for r in rs)
        return (m / (m + mi) if m + mi else 0.0, m / (m + s) if m + s else 0.0)

    ok = True
    native = [r for r in results if r.fixture not in TRANSLATED]
    if native:
        recall, precision = totals(native)
        m1 = recall >= M1_RECALL and precision >= M1_PRECISION and fast
        m2 = recall >= M2_RECALL and precision >= M2_PRECISION and fast
        print(
            f"\nM1 targets (recall >= {M1_RECALL}, precision >= {M1_PRECISION}, "
            f"detection <= {LATENCY_BUDGET_MS} ms): {'met' if m1 else 'NOT met'} — "
            f"recall {recall:.2f}, precision {precision:.2f}, slowest {slowest:.2f} ms"
        )
        print(
            f"M2 targets (recall >= {M2_RECALL}, precision >= {M2_PRECISION}, "
            f"detection <= {LATENCY_BUDGET_MS} ms): {'met' if m2 else 'NOT met'}"
        )
        # The milestone in progress sets the exit status.
        ok = ok and m2
    translated = [r for r in results if r.fixture in TRANSLATED]
    if translated:
        recall, precision = totals(translated)
        met = recall >= M1_RECALL and precision >= M1_PRECISION and fast
        print(
            f"\ntranslated fixtures ({', '.join(r.fixture for r in translated)}; #77, M1 bar: "
            f"recall >= {M1_RECALL}, precision >= {M1_PRECISION}): {'met' if met else 'NOT met'} — "
            f"recall {recall:.2f}, precision {precision:.2f}"
        )
        ok = ok and met
    return 0 if ok or wanted_literal else 1


if __name__ == "__main__":
    sys.exit(main())
