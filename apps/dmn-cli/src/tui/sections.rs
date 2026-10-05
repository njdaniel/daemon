//! Area 2, under the Approach: the six expandable sections the slow phase fills (#56, SPEC § AI
//! Assistance): Deeper Answer, Follow-up Questions, Tradeoffs, Example, Things I Should Mention,
//! Resume Context, keys `1`–`6` in that fixed order.
//!
//! The rows sit in a compact grid (two columns once there is room), so six of them take three
//! lines and the Approach above keeps its space. A row still loading shows a subtle `…`, a row
//! that timed out a small `slow` tag, and a row with nothing for this question is hidden
//! (`Sections::visible` decides). Exactly one section is open at a time and only by a key press
//! (`Question::open`); its body goes under the grid, taking rows from the Approach area down to
//! a floor, and is cut rather than growing past it. A cut body scrolls (#94): its last row is
//! the position hint, `↓ 6 more` at the top, `↑ 3 · ↓ 3 more` on the way, `↑ 6` at the end.
//!
//! Inside the body every item wraps onto as many rows as it needs, so scrolling reaches its last
//! word: nothing is cut by a count of rows. What is still cut is a row that cannot wrap, each to
//! its width with `…`: a line of code (wrapping would change what it says), a code block's
//! `Name · language` line, and the one-line note of a section opened before anything arrived.

use dmn_session::{CodeBlock, Question, Section, SectionState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::View;
use super::text::{truncate, width, wrap};

/// Below this many cells the rows stack in one column instead of two.
const TWO_COLUMNS_FROM: usize = 56;
/// Left and right margin, so the rows line up with the Approach text inside its border.
const MARGIN: u16 = 2;
/// The body of an open section sits under its row's title: `1 ▾ ` is four cells.
const BODY_INDENT: usize = 4;

/// What the sections take on screen for the question on screen, worked out once per frame.
pub(super) struct Plan {
    /// The rows on screen, in the fixed order.
    rows: Vec<Section>,
    /// Each row's hint: `…` while loading, `slow` once timed out.
    tags: Vec<(Section, Option<Span<'static>>)>,
    open: Option<Section>,
    columns: usize,
    body: Vec<Line<'static>>,
    /// The Approach above is the previous question's while a new one loads: dim these too.
    dim: bool,
    /// Rows the body is scrolled down (#94), as the session has it: [`render`] keeps it within
    /// the body, which a resize can shorten under it.
    scroll: u16,
}

/// How an open section's body that is taller than its area scrolls (#94).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Extent {
    /// Body rows on screen: the area less the hint's row.
    pub(super) rows: u16,
    /// The offset that puts the body's last row on screen.
    pub(super) max: u16,
}

impl Plan {
    /// Lines the grid of rows takes.
    pub(super) fn grid_rows(&self) -> u16 {
        u16::try_from(self.rows.len().div_ceil(self.columns)).unwrap_or(u16::MAX)
    }

    /// Lines the open section's body would like.
    pub(super) fn body_rows(&self) -> u16 {
        u16::try_from(self.body.len()).unwrap_or(u16::MAX)
    }

    /// How the body scrolls in `room` rows: `None` when it fits, and then it is drawn whole.
    pub(super) fn extent(&self, room: u16) -> Option<Extent> {
        let rows = room.checked_sub(1)?;
        (self.body_rows() > room).then(|| Extent {
            rows,
            max: self.body_rows() - rows,
        })
    }
}

/// The key that toggles `section`: its place in the fixed order, from 1.
pub(super) fn key_of(section: Section) -> usize {
    Section::ALL
        .iter()
        .position(|s| *s == section)
        .map_or(0, |i| i + 1)
}

/// The rows the question on screen shows, or `None` when it shows none (no question, or no slow
/// phase has started for it).
pub(super) fn plan(view: &View<'_>, area_width: u16) -> Option<Plan> {
    let q = view.shown()?;
    let rows: Vec<Section> = Section::ALL
        .into_iter()
        .filter(|s| q.sections.visible(*s))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let cells = usize::from(area_width.saturating_sub(2 * MARGIN));
    let columns = if cells >= TWO_COLUMNS_FROM && rows.len() > 1 {
        2
    } else {
        1
    };
    let open = q.open.filter(|s| rows.contains(s));
    let body = open
        .map(|s| body(q, s, cells.saturating_sub(BODY_INDENT)))
        .unwrap_or_default();
    let tags = rows
        .iter()
        .map(|s| (*s, tag(q.sections.state(*s))))
        .collect();
    Some(Plan {
        rows,
        tags,
        open,
        columns,
        body,
        dim: view.loading,
        scroll: view.session.scroll,
    })
}

/// Draw the grid of rows into `grid` and the open section's body into `body_area`.
pub(super) fn render(plan: &Plan, grid: Rect, body_area: Rect, buf: &mut Buffer) {
    let base = if plan.dim {
        Style::new().add_modifier(Modifier::DIM)
    } else {
        Style::new()
    };
    let inner = Rect {
        x: grid.x.saturating_add(MARGIN),
        width: grid.width.saturating_sub(2 * MARGIN),
        ..grid
    };
    let per_column = plan.rows.len().div_ceil(plan.columns);
    let column_width = inner.width / u16::try_from(plan.columns).unwrap_or(1).max(1);
    for (i, section) in plan.rows.iter().enumerate() {
        // Column-major, so the rows read top to bottom in the fixed order.
        let (column, row) = (i / per_column, i % per_column);
        let Ok(row) = u16::try_from(row) else { break };
        if row >= inner.height {
            continue;
        }
        let x = inner.x + column_width * u16::try_from(column).unwrap_or(0);
        let w = if column + 1 == plan.columns {
            inner.right().saturating_sub(x)
        } else {
            column_width
        };
        let cell = Rect::new(x, inner.y + row, w, 1);
        Paragraph::new(row_line(*section, plan, usize::from(w).saturating_sub(1)))
            .style(base)
            .render(cell, buf);
    }

    if plan.body.is_empty() || body_area.height == 0 {
        return;
    }
    let body_inner = Rect {
        x: body_area
            .x
            .saturating_add(MARGIN + u16::try_from(BODY_INDENT).unwrap_or(0)),
        width: body_area
            .width
            .saturating_sub(2 * MARGIN + u16::try_from(BODY_INDENT).unwrap_or(0)),
        ..body_area
    };
    let mut lines = plan.body.clone();
    if let Some(extent) = plan.extent(body_inner.height) {
        // Cut to the area, and say so, instead of pushing the transcript off the screen: the
        // rows from the scroll offset, then where that is in the body (#94).
        let above = plan.scroll.min(extent.max);
        lines = lines
            .into_iter()
            .skip(usize::from(above))
            .take(usize::from(extent.rows))
            .collect();
        lines.push(Line::from(Span::styled(
            position(above, extent.max - above),
            Style::new().fg(Color::DarkGray),
        )));
    }
    Paragraph::new(lines).style(base).render(body_inner, buf);
}

/// The hint on a cut body's last row: the rows above the first one shown and below the last.
fn position(above: u16, below: u16) -> String {
    match (above, below) {
        (0, _) => format!("↓ {below} more"),
        (_, 0) => format!("↑ {above}"),
        _ => format!("↑ {above} · ↓ {below} more"),
    }
}

/// `1 ▸ Deeper Answer …` (collapsed, loading) or `2 ▾ Follow-up Questions` (open).
fn row_line(section: Section, plan: &Plan, cells: usize) -> Line<'static> {
    let open = plan.open == Some(section);
    let key = Span::styled(
        format!("{} ", key_of(section)),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    );
    let marker = Span::styled(
        if open { "▾ " } else { "▸ " },
        Style::new().fg(Color::DarkGray),
    );
    // `plan` only holds sections of the question on screen, whose state the tag reads.
    let tag = plan_tag(plan, section);
    let tag_cells = tag.as_ref().map_or(0, |t| width(&t.content) + 1);
    let room = cells.saturating_sub(4 + tag_cells);
    let title_style = if open {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let mut spans = vec![
        key,
        marker,
        Span::styled(truncate(section.title(), room), title_style),
    ];
    if let Some(t) = tag {
        spans.push(Span::raw(" "));
        spans.push(t);
    }
    Line::from(spans)
}

/// The hint a row carries for its state (SPEC § AI Assistance: a subtle "…" for content not yet
/// available; § Context and LLM: a small "slow" tag on timeout). Done and failed rows carry
/// none: errors appear only as status dots.
fn tag(state: &SectionState) -> Option<Span<'static>> {
    match state {
        SectionState::Loading => Some(Span::styled("…", Style::new().fg(Color::DarkGray))),
        SectionState::Slow => Some(Span::styled("slow", Style::new().fg(Color::Yellow))),
        SectionState::Done | SectionState::Empty | SectionState::Failed(_) => None,
    }
}

/// A row's tag, captured when the plan is made, so rendering does not need the question.
fn plan_tag(plan: &Plan, section: Section) -> Option<Span<'static>> {
    plan.tags
        .iter()
        .find(|(s, _)| *s == section)
        .and_then(|(_, t)| t.clone())
}

/// The body of an open section, at `cells` wide.
fn body(q: &Question, section: Section, cells: usize) -> Vec<Line<'static>> {
    let s = &q.sections;
    let mut out = Vec::new();
    match section {
        Section::Deeper => {
            for item in &s.deeper {
                out.extend(labelled(
                    &item.label,
                    &item.text,
                    Style::new().add_modifier(Modifier::BOLD),
                    cells,
                ));
            }
            if let Some(code) = &s.solution {
                out.extend(code_block("Solution", code, cells));
            }
        }
        Section::Followups => {
            for item in &s.followups {
                out.extend(bullet(item, cells));
            }
        }
        Section::Tradeoffs => {
            for t in &s.tradeoffs {
                let text = format!("+ {}  − {}", t.pro, t.con);
                out.extend(labelled(
                    &t.option,
                    &text,
                    Style::new().add_modifier(Modifier::BOLD),
                    cells,
                ));
            }
        }
        Section::Example => {
            if let Some(code) = &s.example {
                out.extend(code_block("Example", code, cells));
            }
        }
        Section::Mention => {
            for item in &s.mention {
                out.extend(bullet(item, cells));
            }
        }
        Section::ResumeContext => {
            for c in &s.resume_context {
                out.extend(labelled(
                    &c.source,
                    &c.text,
                    Style::new().fg(Color::DarkGray),
                    cells,
                ));
            }
        }
    }
    if out.is_empty() {
        // Open before anything arrived: say where it is, in the row's own words.
        let note = match s.state(section) {
            SectionState::Slow => "Nothing arrived within the 20 s limit.",
            _ => "…",
        };
        out.push(Line::from(Span::styled(
            truncate(note, cells),
            Style::new().fg(Color::DarkGray),
        )));
    }
    out
}

/// `label: text`, wrapped in full (#94: the body scrolls, so no item is cut by rows; its length
/// is `dmn-llm`'s limit, 240 characters), with the label styled on the first row.
fn labelled(label: &str, text: &str, label_style: Style, cells: usize) -> Vec<Line<'static>> {
    let label = label.trim();
    let whole = if label.is_empty() {
        text.trim().to_owned()
    } else {
        format!("{label}: {}", text.trim())
    };
    wrap(&whole, cells, usize::MAX)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let head = format!("{label}:");
            match (i, row.strip_prefix(&head)) {
                (0, Some(rest)) if !label.is_empty() => Line::from(vec![
                    Span::styled(head, label_style),
                    Span::raw(rest.to_owned()),
                ]),
                _ => Line::from(row),
            }
        })
        .collect()
}

/// `• text`, wrapped in full with continuation rows under the text.
fn bullet(text: &str, cells: usize) -> Vec<Line<'static>> {
    wrap(text.trim(), cells.saturating_sub(2), usize::MAX)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                Span::styled("• ", Style::new().fg(Color::Cyan))
            } else {
                Span::raw("  ")
            };
            Line::from(vec![lead, Span::raw(row)])
        })
        .collect()
}

/// A code block: a dim `Name · language` line, then the code line by line, each cut to the
/// width (code is not re-wrapped: wrapping would change what it says).
fn code_block(name: &str, code: &CodeBlock, cells: usize) -> Vec<Line<'static>> {
    let header = if code.language.trim().is_empty() {
        name.to_owned()
    } else {
        format!("{name} · {}", code.language.trim())
    };
    let mut out = vec![Line::from(Span::styled(
        truncate(&header, cells),
        Style::new().fg(Color::DarkGray),
    ))];
    out.extend(
        code.code
            .trim_end()
            .lines()
            .map(|l| Line::from(truncate(&l.replace('\t', "    "), cells))),
    );
    out
}
