//! The language picker (#115): a small centred overlay, drawn over the three areas while
//! [`Session::picker`] is open, listing the session's languages ([`Session::languages`]) with
//! their voices, the cursor on one row and the current language marked. The areas under it keep
//! their layout: the overlay is drawn last, on top of the finished frame, and goes away with the
//! picker. `↑ ↓` move the cursor, Enter chooses, Esc closes (the keys are `dmn run`'s; the
//! overlay only draws what the session says).
//!
//! With one language listed the picker still opens, on that one row, with a hint on how to
//! enroll more, so the key is seen to do something.

use dmn_session::Session;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

use super::text::{truncate, width};

/// The overlay's title.
const TITLE: &str = " LANGUAGE ";
/// The keys, as the last row inside the border.
const KEYS: &str = "↑↓ move · ⏎ choose · esc close";
/// The hint under a single entry (#115: "a hint to enroll more").
const HINT: &str = "one language enrolled; add another with";
const HINT_COMMAND: &str = "dmn speak enroll --wav <clip> --language <code>";
/// The mark on the language `dmn-voice` speaks now.
const CURRENT: &str = "← now";
/// Cells between the border and the text on each side.
const PADDING: u16 = 1;

/// Draw the picker over `area` (the whole pane) when it is open; nothing otherwise.
pub(super) fn render(session: &Session, area: Rect, buf: &mut Buffer) {
    let Some(picker) = session.picker else {
        return;
    };
    if session.languages.is_empty() {
        return;
    }
    let current = session.speak_target();
    let code_cells = session
        .languages
        .iter()
        .map(|l| width(&l.code))
        .max()
        .unwrap_or(2);
    let voice_cells = session
        .languages
        .iter()
        .map(|l| width(&l.voice))
        .max()
        .unwrap_or(0);
    // One row per language: `▸ de   voice-de   ← now`.
    let mut rows: Vec<Line<'static>> = session
        .languages
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let on_cursor = i == picker.cursor;
            let is_current = Some(l.code.as_str()) == current;
            let cursor = if on_cursor { "▸ " } else { "  " };
            let mut text = format!(
                "{cursor}{:<code_cells$}   {:<voice_cells$}",
                l.code, l.voice
            );
            if is_current {
                text.push_str("   ");
                text.push_str(CURRENT);
            }
            let mut style = Style::new();
            if on_cursor {
                style = style.fg(Color::Black).bg(Color::Cyan);
            } else if is_current {
                style = style.add_modifier(Modifier::BOLD);
            }
            Line::from(Span::styled(text, style))
        })
        .collect();
    let dim = Style::new().fg(Color::DarkGray);
    if session.languages.len() == 1 {
        rows.push(Line::from(Span::styled(HINT, dim)));
        rows.push(Line::from(Span::styled(HINT_COMMAND, dim)));
    }
    rows.push(Line::from(Span::styled(KEYS, dim)));

    // Size the box to its longest row (plus border and padding), never wider or taller than
    // the pane; a row that does not fit is cut with `…` rather than wrapped.
    let inner_cells = rows.iter().map(Line::width).max().unwrap_or(0);
    let want_w = u16::try_from(inner_cells)
        .unwrap_or(u16::MAX)
        .saturating_add(2 + 2 * PADDING);
    let want_h = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let w = want_w.min(area.width).max(width(TITLE) as u16 + 2);
    let h = want_h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let boxed = Rect::new(x, y, w.min(area.width), h);
    Clear.render(boxed, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(TITLE)
        .title_alignment(Alignment::Left)
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(boxed);
    block.render(boxed, buf);
    let text_area = Rect::new(
        inner.x + PADDING.min(inner.width / 2),
        inner.y,
        inner.width.saturating_sub(2 * PADDING),
        inner.height,
    );
    let max = usize::from(text_area.width);
    let rows: Vec<Line<'static>> = rows
        .into_iter()
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let style = line.spans.first().map(|s| s.style).unwrap_or_default();
            if width(&text) > max {
                Line::from(Span::styled(truncate(&text, max), style))
            } else {
                // Pad the cursor row so its highlight spans the box.
                let pad = " ".repeat(max.saturating_sub(width(&text)));
                Line::from(Span::styled(format!("{text}{pad}"), style))
            }
        })
        .collect();
    Paragraph::new(rows).render(text_area, buf);
}
