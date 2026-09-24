//! Area 2: the numbered Approach, or the STAR cards for a behavioral question.

use dmn_session::{Question, StarCards};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use super::text::{truncate, width, wrap};
use super::{Mode, View};

/// An approach line wraps onto at most this many rows before it is cut with `…`. The model's
/// lines are ≤ 90 characters (`dmn-llm` limits), so two rows hold one even at 80 columns.
const ROWS_PER_ITEM: usize = 2;

pub(super) fn render(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let star = view
        .live()
        .and_then(|q| q.star.as_ref().filter(|_| q.approach.is_empty()));
    let title = if star.is_some() {
        " STAR "
    } else {
        " APPROACH "
    };
    let mut block = Block::bordered()
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(
            title,
            Style::new().add_modifier(Modifier::BOLD),
        )));
    let hint = if view.loading {
        Some("thinking…")
    } else if view.streaming {
        Some("…")
    } else {
        None
    };
    if let Some(h) = hint {
        block = block.title(
            Line::from(Span::styled(
                format!(" {h} "),
                Style::new().fg(Color::DarkGray),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(area);
    block.render(area, buf);

    let cells = usize::from(inner.width);
    let mut lines = match (view.live(), star) {
        (Some(_), Some(cards)) => star_lines(cards, cells),
        (Some(q), None) => approach_lines(q, cells),
        (None, _) => Vec::new(),
    };
    if lines.is_empty() && !view.loading {
        let note = match view.mode {
            Mode::Listening => "The Approach appears here when the interviewer asks a question.",
            Mode::Live | Mode::Pinned => "…",
        };
        lines.push(Line::from(Span::styled(
            truncate(note, cells),
            Style::new().fg(Color::DarkGray),
        )));
    }
    let mut paragraph = Paragraph::new(lines);
    if view.loading {
        // SPEC § AI Assistance: while loading, the previous Approach dims rather than clearing.
        paragraph = paragraph.style(Style::new().add_modifier(Modifier::DIM));
    }
    paragraph.render(inner, buf);
}

/// `1  first line` with continuation rows indented under the text.
fn numbered(
    marker: &str,
    marker_style: Style,
    text: &str,
    text_style: Style,
    cells: usize,
) -> Vec<Line<'static>> {
    let indent = width(marker);
    wrap(text, cells.saturating_sub(indent), ROWS_PER_ITEM)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                Span::styled(marker.to_owned(), marker_style)
            } else {
                Span::raw(" ".repeat(indent))
            };
            Line::from(vec![lead, Span::styled(row, text_style)])
        })
        .collect()
}

fn approach_lines(q: &Question, cells: usize) -> Vec<Line<'static>> {
    let number = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut out: Vec<Line<'static>> = q
        .approach
        .iter()
        .enumerate()
        .flat_map(|(i, text)| numbered(&format!("{}  ", i + 1), number, text, Style::new(), cells))
        .collect();
    // The latest clarification's own lines sit under the parent Approach, which it does not
    // replace (#13 decision).
    if let Some(c) = q.clarifications.last().filter(|c| !c.approach.is_empty()) {
        out.push(Line::default());
        let mark = Style::new().fg(Color::Cyan);
        out.extend(
            c.approach
                .iter()
                .flat_map(|text| numbered("↳  ", mark, text, Style::new(), cells)),
        );
    }
    out
}

fn star_lines(cards: &StarCards, cells: usize) -> Vec<Line<'static>> {
    let letter = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    if let Some(story) = cards.story.as_deref().filter(|s| !s.is_empty()) {
        out.push(Line::from(Span::styled(
            truncate(&format!("Story: {story}"), cells),
            Style::new().fg(Color::DarkGray),
        )));
    }
    for (mark, text, style) in [
        ("S  ", &cards.situation, Style::new()),
        ("T  ", &cards.task, Style::new()),
        // SPEC § AI Assistance: the Action card is emphasised.
        (
            "A  ",
            &cards.action,
            Style::new().add_modifier(Modifier::BOLD),
        ),
        ("R  ", &cards.result, Style::new()),
    ] {
        if !text.is_empty() {
            out.extend(numbered(mark, letter, text, style, cells));
        }
    }
    out
}
