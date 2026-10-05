//! The terminal UI: the SPEC's three areas drawn from a [`Session`] and nothing else.
//!
//! Input: a `&Session` (the value behind `dmn-session`'s `watch` channel).
//! Output: cells in a ratatui [`Buffer`]; no terminal I/O happens here.
//! Owns: no channel. `dmn run` (`crate::run`) redraws it on every `watch` change and handles
//! the keys; the snapshot tests render saved sessions.
//!
//! Layout, top to bottom (SPEC § UI, Terminal MVP), sized for a 100 × 30 pane beside the call
//! and still readable at 80 × 24:
//!
//! 1. Question: a bordered block of four lines holding the question (or, before any question,
//!    the last thing the interviewer said), the focus line and the latest clarification (`↳`).
//!    Its title is the state label (`LISTENING`, `LIVE QUESTION`, `PINNED QUESTION`), with the
//!    "N new" badge while pinned questions queue. `←` steps back to an earlier question (#94),
//!    which holds the screen as a pin does under the title `QUESTION 2 of 5`; `→` steps forward,
//!    and from the newest earlier one back to the live question.
//! 2. Approach: the numbered lines (or the STAR cards for a behavioral question, the matched
//!    story in their header). While the next question's reply is on its way the previous
//!    Approach stays, dimmed, rather than clearing. Under it, once the slow phase has started,
//!    the six expandable sections (#56): a compact grid of rows, keys `1`–`6`, and the body of
//!    the one that is open, which takes rows from the Approach down to a floor. A body taller
//!    than its rows scrolls (#94, `↑ ↓` a row, PgUp / PgDn the rows visible) and its last row
//!    says how much is above and below (`↑ 3 · ↓ 6 more`).
//! 3. Transcript and status: the last two transcript lines, then one line with the mic, system
//!    and model dots and the two most useful shortcuts for the current state, followed by
//!    `⏎ ask` (#96) and led, for 5 s after the model declined what Enter sent, by
//!    `not a question`. The keys name `←` once there is an earlier question and `↑↓` while the
//!    open section is cut, and behind an earlier question they are led by the count of
//!    questions that arrived since (`2 new`).
//!
//! From 120 columns (#88) the transcript is a right-hand column instead, 40 % of the width and
//! the full height above the status line, wrapped with the newest line at the bottom; the
//! question, the Approach and the sections keep the left in the same order, and the status line
//! stays full-width. Narrower panes (the 100 × 30 and 80 × 24 above) are stacked as listed.
//!
//! The mouse (#95) reads the same layout: [`section_at`] finds the section row under a click from
//! the row rectangles `sections::Plan::row_cells` gives the renderer, and the wheel is
//! [`scroll_to`] with [`Scroll::WheelUp`] / `WheelDown` while [`on_left_column`] holds.
//!
//! Density rules: every field is cut to its area with `…` instead of growing it (an open
//! section's body with the scroll hint), and nothing expands on its own: a section opens only on
//! its key, one at a time (`Question::open`).
//!
//! With interview assistance off (#79, Ctrl+Shift+A or `--no-assist`) there is no question and no
//! Approach, so the screen is the transcript alone: every line of the session's window, wrapped
//! rather than cut, the newest just above the same status line, which says `assist off`.

mod approach;
mod question;
mod sections;
mod status;
#[cfg(test)]
mod tests;
mod text;

use dmn_session::{Progress, Question, Section, Session};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use status::TranscriptArea;

/// Rows the question block takes: four content lines inside a border.
const QUESTION_ROWS: u16 = 6;
/// Rows the transcript takes: a rule with its title, then two lines.
const TRANSCRIPT_ROWS: u16 = 3;
/// From this many columns the transcript is a right-hand column instead of two lines under the
/// Approach (#88): below it the question and Approach would get too narrow beside it, so 100 × 30
/// and 80 × 24 panes keep the stacked layout.
const SIDE_MIN_COLS: u16 = 120;
/// The transcript column's share of the width on a wide pane.
const SIDE_PERCENT: u16 = 40;
/// The status line.
const STATUS_ROWS: u16 = 1;
/// The Approach block never gets less than a border and one line.
const APPROACH_MIN_ROWS: u16 = 3;
/// An open section's body always gets at least this many rows, even when the Approach then has
/// to be cut for it: the user pressed its key to read it.
const BODY_MIN_ROWS: u16 = 3;

/// What the screen is showing, derived from the session alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No question yet.
    Listening,
    Live,
    Pinned,
    /// An earlier question holds the screen (#94), whether or not the live one is pinned.
    Earlier,
}

/// One press of a scroll key (#94).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Up,
    Down,
    PageUp,
    PageDown,
    /// One notch of the mouse wheel (#95): [`WHEEL_ROWS`] rows.
    WheelUp,
    WheelDown,
}

/// Rows one notch of the wheel scrolls (#95).
const WHEEL_ROWS: u16 = 3;

/// Everything the widgets read, worked out once per frame.
struct View<'a> {
    session: &'a Session,
    mode: Mode,
    /// A newer candidate's reply is on its way: the Approach on screen is the previous one.
    loading: bool,
    /// The live question's own reply is still streaming in.
    streaming: bool,
    /// The open section's body is taller than its rows (#94): the footer offers the scroll
    /// keys. Known once the frame is laid out.
    scrollable: bool,
}

impl<'a> View<'a> {
    fn new(session: &'a Session) -> Self {
        let mode = match (&session.live, session.pinned) {
            _ if session.earlier().is_some() => Mode::Earlier,
            (None, _) => Mode::Listening,
            (Some(_), true) => Mode::Pinned,
            (Some(_), false) => Mode::Live,
        };
        let live_candidate = session.live.as_ref().map(|q| q.candidate);
        // While pinned, a newer question queues instead of replacing this one, so the pinned
        // Approach is not "previous" and stays at full strength. So does an earlier question's
        // (#94): what arrives behind it does not replace it either.
        // `r` dims the Approach on screen the same way until the new reply starts.
        let held = matches!(mode, Mode::Pinned | Mode::Earlier);
        let loading = (!held
            && session
                .pending
                .is_some_and(|p| live_candidate.is_none_or(|c| c != p)))
            || (mode != Mode::Earlier && session.live.as_ref().is_some_and(|q| q.regenerating));
        let streaming = mode != Mode::Earlier
            && session.live.as_ref().is_some_and(|q| {
                q.progress == Progress::Streaming && session.pending == Some(q.candidate)
            });
        Self {
            session,
            mode,
            loading,
            streaming,
            scrollable: false,
        }
    }

    /// The question on screen: the live one, or the earlier one `←` stepped back to (#94).
    fn shown(&self) -> Option<&'a Question> {
        self.session.shown()
    }
}

/// Draw the whole UI into a frame (what `dmn run` calls on every change).
pub fn draw(frame: &mut Frame, session: &Session) {
    let area = frame.area();
    render(session, area, frame.buffer_mut());
}

/// Draw the whole UI into `buf` within `area`.
pub fn render(session: &Session, area: Rect, buf: &mut Buffer) {
    let mut view = View::new(session);
    if session.assist_off {
        // #79: transcript first. The question and Approach areas would only ever be empty (or
        // stale), so their rows go to the transcript.
        let [t, s] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_ROWS)]).areas(area);
        status::render_transcript(&view, TranscriptArea::Screen, t, buf);
        status::render_status(&view, s, buf);
        return;
    }
    let (left, transcript, shape, s) = frame(area);
    status::render_transcript(&view, shape, transcript, buf);
    let m = middle(&view, left);
    view.scrollable = m.extent().is_some();
    question::render(&view, m.question, buf);
    approach::render(&view, m.approach, buf);
    if let Some((plan, grid, body)) = &m.sections {
        sections::render(plan, *grid, *body, buf);
    }
    status::render_status(&view, s, buf);
}

/// Where `by` takes the open section's scroll offset on a pane of `area` (#94): a row for the
/// arrows, the rows visible for PgUp / PgDn, never past either end. `None` when the key moves
/// nothing (no section open, a body that fits, or already at that end), so nothing is
/// dispatched for it.
pub fn scroll_to(session: &Session, area: Rect, by: Scroll) -> Option<u16> {
    if session.assist_off {
        return None;
    }
    let view = View::new(session);
    let (left, ..) = frame(area);
    let extent = middle(&view, left).extent()?;
    let at = session.scroll.min(extent.max);
    let page = extent.rows.max(1);
    let to = match by {
        Scroll::Up => at.saturating_sub(1),
        Scroll::Down => at.saturating_add(1),
        Scroll::PageUp => at.saturating_sub(page),
        Scroll::PageDown => at.saturating_add(page),
        Scroll::WheelUp => at.saturating_sub(WHEEL_ROWS),
        Scroll::WheelDown => at.saturating_add(WHEEL_ROWS),
    }
    .min(extent.max);
    (to != session.scroll).then_some(to)
}

/// Whether the pointer at (`x`, `y`) is on the left column of a pane of `area` (#95): the question,
/// the Approach and the sections, which are everything above the transcript strip when the pane
/// is stacked.
pub fn on_left_column(session: &Session, area: Rect, x: u16, y: u16) -> bool {
    !session.assist_off && frame(area).0.contains(Position::new(x, y))
}

/// The section whose row the pointer at (`x`, `y`) is on, in the grid the frame for `session` on
/// a pane of `area` draws (#95): the same layout and the same row rectangles the renderer uses.
/// `None` anywhere else.
pub fn section_at(session: &Session, area: Rect, x: u16, y: u16) -> Option<Section> {
    if session.assist_off {
        return None;
    }
    let view = View::new(session);
    let (left, ..) = frame(area);
    let m = middle(&view, left);
    let (plan, grid, _) = m.sections.as_ref()?;
    plan.row_cells(*grid)
        .into_iter()
        .find(|(_, cell)| cell.contains(Position::new(x, y)))
        .map(|(section, _)| section)
}

/// The three-area screen: the question and Approach (left on a wide pane), the transcript with
/// its shape, and the status line.
fn frame(area: Rect) -> (Rect, Rect, TranscriptArea, Rect) {
    if area.width >= SIDE_MIN_COLS {
        // #88: question and Approach on the left, where they are read first; the transcript in a
        // column on the right, the full height above the full-width status line.
        let [body, s] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_ROWS)]).areas(area);
        let [left, right] = Layout::horizontal([
            Constraint::Percentage(100 - SIDE_PERCENT),
            Constraint::Percentage(SIDE_PERCENT),
        ])
        .areas(body);
        (left, right, TranscriptArea::Column, s)
    } else {
        let [left, t, s] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(TRANSCRIPT_ROWS),
            Constraint::Length(STATUS_ROWS),
        ])
        .areas(area);
        (left, t, TranscriptArea::Strip, s)
    }
}

/// Areas 1 and 2 laid out: the question block, the Approach and, once the question on screen
/// has section rows, their grid and the open one's body.
struct Middle {
    question: Rect,
    approach: Rect,
    sections: Option<(sections::Plan, Rect, Rect)>,
}

impl Middle {
    /// How far the open section's body scrolls in the rows it got, if it is cut (#94).
    fn extent(&self) -> Option<sections::Extent> {
        let (plan, _, body) = self.sections.as_ref()?;
        plan.extent(body.height)
    }
}

fn middle(view: &View<'_>, left: Rect) -> Middle {
    let [question, a] =
        Layout::vertical([Constraint::Length(QUESTION_ROWS), Constraint::Min(3)]).areas(left);
    match sections::plan(view, a.width) {
        None => Middle {
            question,
            approach: a,
            sections: None,
        },
        Some(plan) => {
            let [approach, grid, body] = split_middle(view, &plan, a);
            Middle {
                question,
                approach,
                sections: Some((plan, grid, body)),
            }
        }
    }
}

/// Area 2 with sections on screen: the Approach on top, the grid of rows pinned under it, and
/// the open section's body (if any) between the grid and the transcript. Closed, the Approach
/// keeps all the room the grid leaves. Open, the body takes the room the Approach does not
/// need; the Approach, the thing read first, is cut only to give the body [`BODY_MIN_ROWS`].
fn split_middle(view: &View<'_>, plan: &sections::Plan, a: Rect) -> [Rect; 3] {
    let grid = plan
        .grid_rows()
        .min(a.height.saturating_sub(APPROACH_MIN_ROWS));
    let rest = a.height - grid;
    let body_wanted = plan.body_rows();
    let approach = if body_wanted == 0 {
        rest
    } else {
        let keep = approach::rows_needed(view, a.width)
            .min(rest.saturating_sub(BODY_MIN_ROWS.min(body_wanted)))
            .max(APPROACH_MIN_ROWS);
        rest.saturating_sub(body_wanted).max(keep).min(rest)
    };
    let body = rest - approach;
    Layout::vertical([
        Constraint::Length(approach),
        Constraint::Length(grid),
        Constraint::Length(body),
    ])
    .areas(a)
}
