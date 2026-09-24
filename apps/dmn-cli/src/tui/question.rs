//! Area 1: the question, its focus line and the latest clarification.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use super::text::{truncate, width, wrap};
use super::{Mode, View};

/// The label colours the SPEC gives the states: teal for live, amber for pinned.
const LIVE: Color = Color::Cyan;
const PINNED: Color = Color::Yellow;

/// Content lines inside the block (SPEC: the question block holds 2–4 lines).
const LINES: usize = 4;
/// The question itself never takes more than three lines; longer ones end in `…`.
const QUESTION_LINES: usize = 3;

pub(super) fn render(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let (label, colour) = match view.mode {
        Mode::Listening => ("LISTENING", Color::Gray),
        Mode::Live => ("LIVE QUESTION", LIVE),
        Mode::Pinned => ("PINNED QUESTION", PINNED),
    };
    let mut block = Block::bordered()
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(
            format!(" {label} "),
            Style::new().fg(colour).add_modifier(Modifier::BOLD),
        )));
    if let Some(right) = right_title(view) {
        block = block.title(right.right_aligned());
    }
    let inner = block.inner(area);
    block.render(area, buf);
    Paragraph::new(lines(view, usize::from(inner.width))).render(inner, buf);
}

/// The kind of question, and the "N new" badge while pinned questions queue.
fn right_title(view: &View<'_>) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(q) = view.live() {
        if !q.kind.is_empty() {
            spans.push(Span::styled(
                format!(" {} ", q.kind.replace('_', " ")),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
    let queued = view.session.queued.len();
    if view.mode == Mode::Pinned && queued > 0 {
        spans.push(Span::styled(
            format!(" {queued} new "),
            Style::new()
                .fg(Color::Black)
                .bg(PINNED)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

fn lines(view: &View<'_>, width_cells: usize) -> Vec<Line<'static>> {
    let muted = Style::new().fg(Color::DarkGray);
    let Some(q) = view.live() else {
        // LISTENING: the last interviewer sentence in muted text, so the user sees the app hears.
        let heard = view.session.last_interviewer.as_deref();
        return match heard {
            Some(t) => wrap(t, width_cells, QUESTION_LINES)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, muted)))
                .collect(),
            None => vec![Line::from(Span::styled(
                "Waiting for the interviewer…",
                muted,
            ))],
        };
    };

    let focus = (!q.focus.is_empty()).then_some(q.focus.as_str());
    let clarification = q.clarifications.last().map(|c| c.text.as_str());
    let others = usize::from(focus.is_some()) + usize::from(clarification.is_some());
    let question_lines = QUESTION_LINES.min(LINES - others);

    let mut out: Vec<Line<'static>> = wrap(&q.text, width_cells, question_lines)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().add_modifier(Modifier::BOLD))))
        .collect();
    if let Some(c) = clarification {
        let mark = "↳ ";
        out.push(Line::from(vec![
            Span::styled(mark, Style::new().fg(LIVE)),
            Span::raw(truncate(c, width_cells.saturating_sub(width(mark)))),
        ]));
    }
    if let Some(f) = focus {
        let label = "Focus: ";
        out.push(Line::from(vec![
            Span::styled(label, muted),
            Span::raw(truncate(f, width_cells.saturating_sub(width(label)))),
        ]));
    }
    out
}
