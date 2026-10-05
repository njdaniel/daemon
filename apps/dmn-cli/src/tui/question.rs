//! Area 1: the question, its focus line and the latest clarification: the live question's, or
//! those of the earlier question `←` stepped back to (#94).

use dmn_session::QuestionSource;
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
/// An earlier question (#94) is labelled like `LISTENING`: neither is the live question.
const EARLIER: Color = Color::Gray;

/// Content lines inside the block (SPEC: the question block holds 2–4 lines).
const LINES: usize = 4;
/// The question itself never takes more than three lines; longer ones end in `…`.
const QUESTION_LINES: usize = 3;

pub(super) fn render(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let (label, colour) = match view.mode {
        Mode::Listening => ("LISTENING".to_owned(), Color::Gray),
        Mode::Live => ("LIVE QUESTION".to_owned(), LIVE),
        Mode::Pinned => ("PINNED QUESTION".to_owned(), PINNED),
        // #94: where the user is among the questions shown so far.
        Mode::Earlier => {
            let (at, of) = view.session.place().unwrap_or_default();
            (format!("QUESTION {at} of {of}"), EARLIER)
        }
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

/// The kind of question, and the "N new" badge while pinned questions queue. Behind an earlier
/// question (#94) the status line counts what arrived instead.
fn right_title(view: &View<'_>) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let Some(q) = view.shown() {
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

/// The LISTENING placeholder before anything has been heard, per `[detect].question_source` (#90).
fn waiting(source: QuestionSource) -> &'static str {
    match source {
        QuestionSource::Interviewer => "Waiting for the interviewer…",
        QuestionSource::You => "Waiting for your question…",
        QuestionSource::Both => "Waiting for a question…",
    }
}

fn lines(view: &View<'_>, width_cells: usize) -> Vec<Line<'static>> {
    let muted = Style::new().fg(Color::DarkGray);
    let Some(q) = view.shown() else {
        // LISTENING: the last sentence from a lane that can ask, in muted text, so the user sees
        // the app hears. The placeholder names that lane (#90).
        let heard = view.session.last_heard.as_deref();
        return match heard {
            Some(t) => wrap(t, width_cells, QUESTION_LINES)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, muted)))
                .collect(),
            None => vec![Line::from(Span::styled(
                waiting(view.session.question_source),
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
