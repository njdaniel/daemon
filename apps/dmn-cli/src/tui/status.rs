//! Area 3: the last two transcript lines, then the status dots and shortcuts.

use dmn_session::{Health, Line as TranscriptLine, Speaker};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use super::text::{truncate, truncate_front, width};
use super::{Mode, View};

/// Transcript rows (SPEC § Transcript & status: the last two segments, one line each).
const TRANSCRIPT_LINES: usize = 2;
/// Speaker labels are padded to one width so the text lines up.
const LABEL_CELLS: usize = 11;
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
    let newest = &all[all.len().saturating_sub(TRANSCRIPT_LINES)..];

    let lines: Vec<Line<'static>> = newest
        .iter()
        .map(|(l, partial)| transcript_line(l, *partial, cells))
        .collect();
    Paragraph::new(lines).render(inner, buf);
}

fn transcript_line(l: &TranscriptLine, partial: bool, cells: usize) -> Line<'static> {
    let (label, colour) = match l.speaker {
        Speaker::Interviewer => ("Interviewer", Color::Cyan),
        Speaker::You => ("You", Color::Magenta),
        Speaker::Unknown => ("?", Color::DarkGray),
    };
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
    for (name, health) in [
        ("mic", &st.mic),
        ("system", &st.system),
        ("model", &st.model),
    ] {
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
    if let Some(d) = detail {
        let used = Line::from(spans.clone()).width();
        let room = usize::from(left.width).saturating_sub(used + 2);
        if room > 3 {
            spans.push(Span::styled(
                format!(" {}", truncate(&d, room)),
                Style::new().fg(Color::Red),
            ));
        }
    }
    Paragraph::new(Line::from(spans)).render(left, buf);
    Paragraph::new(Line::from(Span::styled(
        keys,
        Style::new().fg(Color::DarkGray),
    )))
    .render(right, buf);
}

/// The two most useful keys for the state on screen (SPEC § Transcript & status: "Footer shows
/// the two most useful shortcuts for the current state"). While a coding question is on screen
/// the second one is `s` with the coding-solutions mode it would switch from (#50), so which
/// mode Deeper Answer follows is always visible.
fn shortcuts(view: &View<'_>) -> String {
    let coding = view.live().is_some_and(|q| q.kind == CODING);
    let solutions = view.session.coding_solutions.label();
    let pair = match (view.mode, view.loading, coding) {
        (Mode::Pinned, _, true) => format!("p unpin · s: {solutions}"),
        (Mode::Live, _, true) => format!("p pin · s: {solutions}"),
        (Mode::Pinned, _, false) => "p unpin · r redo".to_owned(),
        (Mode::Live, false, false) => "p pin · r redo".to_owned(),
        (Mode::Live, true, false) | (Mode::Listening, _, _) => "m mute · q quit".to_owned(),
    };
    format!("{pair} ")
}

/// The fast phase's `kind` for a coding question (SPEC § Transcription and question detection).
const CODING: &str = "coding";
