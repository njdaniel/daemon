#!/usr/bin/env python3
"""Write testdata/context/resume.pdf from testdata/context/resume.md (#53).

A minimal, deterministic text PDF: Helvetica with WinAnsiEncoding, one text object per line,
uncompressed content streams, no timestamps, so the committed file only changes when the
Markdown does. Stdlib only.

    python3 tools/fixtures/context_pdf.py
"""

from __future__ import annotations

import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
SRC = ROOT / "testdata" / "context" / "resume.md"
OUT = ROOT / "testdata" / "context" / "resume.pdf"

WIDTH = 90  # characters per line at 10 pt Helvetica on US Letter
LINES_PER_PAGE = 60
TOP, LEFT, LEADING = 750, 54, 12


def blocks(md: str) -> list[str]:
    """Headings, bullets and paragraphs, each reflowed onto one line."""
    out: list[str] = []
    for raw in md.splitlines():
        text = raw.strip().replace("`", "")
        if not text:
            out.append("")
        elif text.startswith("#") or text.startswith("- ") or not out or not out[-1]:
            out.append(text.lstrip("#").strip())
        else:
            out[-1] += " " + text  # a hard-wrapped continuation line
    return out


def lines(md: str) -> list[str]:
    out: list[str] = []
    for block in blocks(md):
        out.extend(textwrap.wrap(block, WIDTH, subsequent_indent="  ") or [""])
    return out


def escape(s: str) -> str:
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def content(page: list[str]) -> bytes:
    ops = ["BT", "/F1 10 Tf", f"{LEADING} TL", f"{LEFT} {TOP} Td"]
    for line in page:
        ops.append(f"({escape(line)}) Tj T*")
    ops.append("ET")
    return ("\n".join(ops) + "\n").encode("latin-1")


def build(text_lines: list[str]) -> bytes:
    pages = [text_lines[i : i + LINES_PER_PAGE] for i in range(0, len(text_lines), LINES_PER_PAGE)]
    n = len(pages)
    # Object numbers: 1 catalog, 2 pages, 3 font, then (page, contents) pairs.
    kids = " ".join(f"{4 + 2 * i} 0 R" for i in range(n))
    objs: list[bytes] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        f"<< /Type /Pages /Kids [{kids}] /Count {n} >>".encode(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    ]
    for i, page in enumerate(pages):
        body = content(page)
        objs.append(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 3 0 R >> >> /Contents {5 + 2 * i} 0 R >>".encode()
        )
        objs.append(f"<< /Length {len(body)} >>\nstream\n".encode() + body + b"endstream")
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for num, obj in enumerate(objs, start=1):
        offsets.append(len(out))
        out += f"{num} 0 obj\n".encode() + obj + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objs) + 1}\n0000000000 65535 f \n".encode()
    for off in offsets:
        out += f"{off:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


def main() -> None:
    OUT.write_bytes(build(lines(SRC.read_text(encoding="utf-8"))))
    print(f"wrote {OUT.relative_to(ROOT)} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
