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

/// The border and the one-cell padding on each side.
const FRAME_CELLS: u16 = 4;
/// The top and bottom border.
const FRAME_ROWS: u16 = 2;

/// The STAR cards when the live question has them in place of an Approach: SPEC § AI Assistance,
/// "Behavioral questions replace the Approach block with the STAR block" (the fast phase also
/// returns an Approach for them, which the cards stand in for).
fn star<'a>(view: &View<'a>) -> Option<&'a StarCards> {
    view.live().and_then(|q| {
        q.star
            .as_ref()
            .filter(|_| q.kind == BEHAVIORAL || q.approach.is_empty())
    })
}

/// The fast phase's `kind` for a behavioral question.
const BEHAVIORAL: &str = "behavioral";

/// What the STAR header names (#58): the story the session has chosen (which Swap moves on at
/// once, before the new cards arrive), with its place in the ranking when there is more than
/// one; otherwise the story the cards say they came from.
fn story_title(q: Option<&Question>, cards: &StarCards) -> Option<String> {
    match q.and_then(|q| q.story_label().map(|l| (l, q.story, q.stories.len()))) {
        Some((label, _, 1)) => Some(label.to_owned()),
        Some((label, at, n)) => Some(format!("{label} {}/{n}", at + 1)),
        None => cards.story.clone().filter(|s| !s.is_empty()),
    }
}

/// Rows the block would like at `area_width`: its content plus the border. An open section
/// (#56) takes the rest of the area, down to a floor the caller sets.
pub(super) fn rows_needed(view: &View<'_>, area_width: u16) -> u16 {
    let cells = usize::from(area_width.saturating_sub(FRAME_CELLS));
    let lines = u16::try_from(content(view, cells).len()).unwrap_or(u16::MAX);
    lines.saturating_add(FRAME_ROWS)
}

/// The numbered Approach, the STAR cards, or a note while there is nothing to show yet.
fn content(view: &View<'_>, cells: usize) -> Vec<Line<'static>> {
    let mut lines = match (view.live(), star(view)) {
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
    lines
}

pub(super) fn render(view: &View<'_>, area: Rect, buf: &mut Buffer) {
    let cards = star(view);
    let hint = if view.loading {
        Some("thinking…")
    } else if view.streaming {
        Some("…")
    } else if cards.is_some() && view.live().is_some_and(|q| q.no_other_story) {
        // Swap pressed with no other matched story (#58).
        Some("no other story")
    } else {
        None
    };
    // SPEC § AI Assistance: the STAR block names the matched story in its header (#58).
    let title = match cards.and_then(|c| story_title(view.live(), c)) {
        Some(story) => {
            let room = usize::from(area.width)
                .saturating_sub(usize::from(FRAME_CELLS) + hint.map_or(0, |h| width(h) + 2));
            truncate(&format!(" STAR · {story} "), room)
        }
        None if cards.is_some() => " STAR ".to_owned(),
        None => " APPROACH ".to_owned(),
    };
    let mut block = Block::bordered()
        .padding(Padding::horizontal(1))
        .title(Line::from(Span::styled(
            title,
            Style::new().add_modifier(Modifier::BOLD),
        )));
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

    let mut lines = content(view, usize::from(inner.width));
    let room = usize::from(inner.height);
    if lines.len() > room && room > 0 {
        // Cut to the block with a visible mark, never silently: an open section (#56) on a
        // small terminal can leave fewer rows than the Approach has.
        lines.truncate(room - 1);
        lines.push(Line::from(Span::styled(
            "…",
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
