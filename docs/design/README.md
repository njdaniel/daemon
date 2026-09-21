# Daemon canvas (M3 visual source of truth)

Exported 2026-09-21 from the Daemon design canvas
(https://claude.ai/artifact/Kya3g5rdSzQ7k9tGPqZV2B).

| File | Artboard | Size |
| --- | --- | --- |
| `Main.dc.html` | Live view, system-design question (interactive: Pin, one section open at a time) | 440 × 760 |
| `Behavioral.dc.html` | Live view, behavioral question with STAR block | 440 × 760 |
| `Context.dc.html` | Context & sources view | 440 × 760 |
| `Settings.dc.html` | Settings: audio, transcription, model | 440 × 760 |
| `Collapsed.dc.html` | Collapsed pill (⌥H) | 440 × 92 |
| `canvas.json` | Artboard index and layout | |

The files are self-contained HTML; the `<script src="./support.js">` line is
the canvas runtime and is not needed to read the markup. The M3 front end
(`frontend/`) ports these 1:1; `docs/SPEC.md` § UI spec states the behavior.
