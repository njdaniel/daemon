//! Area 3: the last two transcript lines, then the status dots and shortcuts. With assistance
//! off (#79) the transcript is the whole screen above the status line, each line wrapped.

use dmn_session::{Health, Line as TranscriptLine, Section, Speaker};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::sections::key_of;
use super::text::{truncate, truncate_front, width, wrap};
use super::{Mode, View};

/// Transcript rows (SPEC § Transcript & status: the last two segments, one line each).
const TRANSCRIPT_LINES: usize = 2;
/// Speaker labels are padded to one width so the text lines up.
const LABEL_CELLS: usize = 11;
/// #79: the share of the local LLM in system memory, in percent, from which the status line says
/// `llm partly on cpu`. The default `qwen3.6:35b-a3b` normally sits at 4 % on the dev 4090
/// (`ollama ps`, 2026-09-29), and the 2 s budget was measured like that; loaded after GPU
/// whisper it is far more.
const LLM_CPU_NOTICE: u8 = 10;
/// Trails the open utterance's partial text.
const CURSOR: &str = "▍";

pub(super) fn render_transcript(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let block = Block::new().borders(Borders::TOP).title(Span::styled(
        " TRANSCRIPT ",
        Style::new().fg(Color::DarkGray),
    ));
    let inner = block.inner(area);
    block.render(area, buf);
    let cells = usize::from(inner.width);

    // Finals and open partials together, in speaking order; the newest two are shown.
    let s = view.session;
    let mut all: Vec<(&TranscriptLine, bool)> = s
        .transcript
        .iter()
        .map(|l| (l, false))
        .chain(s.partials.iter().map(|l| (l, true)))
        .collect();
    all.sort_by(|a, b| a.0.start.total_cmp(&b.0.start));
    if s.assist_off {
        // Bottom-aligned, like a terminal: the newest line sits just above the status line
        // however much of the window there is.
        let rows = wrapped(&all, cells, usize::from(inner.height));
        let used = u16::try_from(rows.len()).unwrap_or(inner.height);
        let [_, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(used)]).areas(inner);
        Paragraph::new(rows).render(bottom, buf);
        return;
    }
    let newest = &all[all.len().saturating_sub(TRANSCRIPT_LINES)..];

    let lines: Vec<Line<'static>> = newest
        .iter()
        .map(|(l, partial)| transcript_line(l, *partial, cells))
        .collect();
    Paragraph::new(lines).render(inner, buf);
}

/// #79, the transcript-first screen: the newest lines that fit in `rows`, each wrapped under
/// its speaker label, in speaking order. The oldest line shown loses its first rows when only its
/// end fits, so the newest words are always on screen.
fn wrapped(all: &[(&TranscriptLine, bool)], cells: usize, rows: usize) -> Vec<Line<'static>> {
    let indent = " ".repeat(LABEL_CELLS + 1);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (l, partial) in all.iter().rev() {
        if out.len() >= rows {
            break;
        }
        let (label, colour) = speaker(l.speaker);
        let room = cells.saturating_sub(LABEL_CELLS + 1 + if *partial { width(CURSOR) } else { 0 });
        let mut text = wrap(&l.text, room, rows);
        if text.is_empty() {
            text.push(String::new());
        }
        let last = text.len() - 1;
        let lines: Vec<Line<'static>> = text
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let head = if i == 0 {
                    Span::styled(
                        format!("{label:<LABEL_CELLS$} "),
                        Style::new().fg(colour).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw(indent.clone())
                };
                let mut spans = vec![head, Span::raw(t)];
                if *partial && i == last {
                    spans.push(Span::styled(CURSOR, Style::new().fg(Color::DarkGray)));
                }
                Line::from(spans)
            })
            .collect();
        // Newest first while collecting; each line's rows go in bottom-up too.
        out.extend(lines.into_iter().rev());
    }
    out.truncate(rows);
    out.reverse();
    out
}

fn speaker(s: Speaker) -> (&'static str, Color) {
    match s {
        Speaker::Interviewer => ("Interviewer", Color::Cyan),
        Speaker::You => ("You", Color::Magenta),
        Speaker::Unknown => ("?", Color::DarkGray),
    }
}

fn transcript_line(l: &TranscriptLine, partial: bool, cells: usize) -> Line<'static> {
    let (label, colour) = speaker(l.speaker);
    let room = cells.saturating_sub(LABEL_CELLS + 1 + if partial { width(CURSOR) } else { 0 });
    let mut spans = vec![
        Span::styled(
            format!("{label:<LABEL_CELLS$} "),
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::raw(truncate_front(&l.text, room)),
    ];
    if partial {
        spans.push(Span::styled(CURSOR, Style::new().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

pub(super) fn render_status(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let keys = shortcuts(view);
    let keys_cells = u16::try_from(width(&keys)).unwrap_or(u16::MAX);
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(keys_cells)]).areas(area);

    let st = &view.session.status;
    let mut spans = Vec::new();
    let mut detail = None;
    // #59: the model dot is named after the provider answering, once one has been announced.
    let model = st.provider.as_ref().map_or("model", |p| p.name.as_str());
    for (name, health) in [("mic", &st.mic), ("system", &st.system), (model, &st.model)] {
        let (dot, style, suffix) = match health {
            Health::Ok => ("●", Style::new().fg(Color::Green), ""),
            Health::Unknown => ("○", Style::new().fg(Color::DarkGray), ""),
            Health::Muted => ("○", Style::new().fg(Color::Yellow), " muted"),
            Health::AllAudio => ("●", Style::new().fg(Color::Yellow), ": all audio"),
            Health::Error(why) => {
                // The dots are the only place errors appear (SPEC § Transcript & status); the
                // first error's reason stands in for the window's tooltip.
                if detail.is_none() {
                    detail = Some(format!("{name}: {why}"));
                }
                ("●", Style::new().fg(Color::Red), "")
            }
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(dot, style));
        spans.push(Span::raw(format!(" {name}{suffix} ")));
    }
    // #16: ring-buffer overruns (audio dropped because a consumer stalled) show beside the dots,
    // and only once there are any.
    if st.overruns > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled("●", Style::new().fg(Color::Red)));
        spans.push(Span::raw(format!(" {} dropped ", st.overruns)));
    }
    // #61: which device transcribes. `gpu+cpu` is the usual split beside a local LLM: the
    // interviewer's lane on the GPU, yours on the CPU. Amber only when a GPU was asked for and
    // no lane got one (the reason is in the log and the replay header).
    if !st.stt.is_empty() {
        let on_gpu = st.stt.contains("GPU ");
        let on_cpu = st.stt.split("; ").any(|lane| lane.starts_with("CPU"));
        let device = match (on_gpu, on_cpu) {
            (true, true) => "gpu+cpu",
            (true, false) => "gpu",
            _ => "cpu",
        };
        let (text, style) = if st.stt_fallback && !on_gpu {
            (
                format!(" stt {device} (no gpu) "),
                Style::new().fg(Color::Yellow),
            )
        } else {
            (format!(" stt {device} "), Style::new().fg(Color::DarkGray))
        };
        spans.push(Span::styled(text, style));
    }
    // #77: what whisper heard, when it was not English (the transcript text is translated).
    if !st.language.is_empty() && st.language != "en" {
        spans.push(Span::styled(
            format!(" {}→en ", st.language),
            Style::new().fg(Color::DarkGray),
        ));
    }
    // #79: a local model that loaded partly into system memory (whisper took the GPU first) is
    // named in amber: answers come, but slower. (Assistance off is said on the right, with the
    // keys, where nothing cuts it.)
    if !view.session.assist_off && st.llm_cpu_percent >= LLM_CPU_NOTICE {
        spans.push(Span::styled(
            " llm partly on cpu ",
            Style::new().fg(Color::Yellow),
        ));
    }
    // An error's reason first (red); otherwise, when transcript text leaves the machine, the
    // SPEC § Providers notice in words (amber) for as long as it does.
    let note = detail.map(|d| (d, Color::Red)).or_else(|| {
        st.provider
            .as_ref()
            .filter(|p| p.remote)
            .map(|_| ("transcript leaves this machine".to_owned(), Color::Yellow))
    });
    if let Some((d, color)) = note {
        let used = Line::from(spans.clone()).width();
        let room = usize::from(left.width).saturating_sub(used + 2);
        if room > 3 {
            spans.push(Span::styled(
                format!(" {}", truncate(&d, room)),
                Style::new().fg(color),
            ));
        }
    }
    Paragraph::new(Line::from(spans)).render(left, buf);
    // #79: the right-hand area is never cut (the left one is, on a narrow pane), so the one
    // state that changes what the screen is for leads it, in amber.
    let mut right_spans = Vec::new();
    if view.session.assist_off {
        right_spans.push(Span::styled(ASSIST_OFF, Style::new().fg(Color::Yellow)));
    }
    right_spans.push(Span::styled(
        keys.strip_prefix(ASSIST_OFF).unwrap_or(&keys).to_owned(),
        Style::new().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(right_spans)).render(right, buf);
}

/// #79: how the status line names assistance off; it leads the keys.
const ASSIST_OFF: &str = "assist off";

/// The two most useful keys for the state on screen (SPEC § Transcript & status: "Footer shows
/// the two most useful shortcuts for the current state"). While a coding question is on screen
/// the second one is `s` with the coding-solutions mode it would switch from (#50), so which
/// mode Deeper Answer follows is always visible.
///
/// Once the live question has section rows (#56), the first key is about them: `1–6 more`
/// while all are closed, or the open one's own key to close it (`2 close`).
fn shortcuts(view: &View<'_>) -> String {
    let coding = view.live().is_some_and(|q| q.kind == CODING);
    let solutions = view.session.coding_solutions.label();
    let sections = view.live().and_then(|q| {
        let visible = |s: &Section| q.sections.visible(*s);
        match q.open.filter(visible) {
            Some(open) => Some(format!("{} close", key_of(open))),
            None => Section::ALL
                .iter()
                .any(visible)
                .then(|| "1–6 more".to_owned()),
        }
    });
    let pin = if view.mode == Mode::Pinned {
        "p unpin"
    } else {
        "p pin"
    };
    // #58: a STAR block built from one of several matched stories offers Swap in place of the
    // pin or redo key.
    let swap = view
        .live()
        .is_some_and(|q| q.star.is_some() && q.stories.len() > 1);
    if view.session.assist_off {
        // Ctrl+Shift+A in tmux's notation: short enough that an 80-column pane with every tag
        // (`system: all audio`, `es→en`) still shows the whole state.
        return format!("{ASSIST_OFF} · C-S-a on ");
    }
    let pair = match (view.mode, view.loading, coding, sections) {
        (Mode::Listening, ..) | (Mode::Live, true, false, _) => "m mute · q quit".to_owned(),
        (_, _, true, Some(sec)) => format!("{sec} · s: {solutions}"),
        (_, _, true, None) => format!("{pin} · s: {solutions}"),
        (_, _, false, Some(sec)) if swap => format!("{sec} · w swap"),
        (_, _, false, Some(sec)) => format!("{sec} · {pin}"),
        (_, _, false, None) if swap => format!("{pin} · w swap"),
        (_, _, false, None) => format!("{pin} · r redo"),
    };
    format!("{pair} ")
}

/// The fast phase's `kind` for a coding question (SPEC § Transcription and question detection).
const CODING: &str = "coding";
