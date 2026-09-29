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
//!    "N new" badge while pinned questions queue.
//! 2. Approach: the numbered lines (or the STAR cards for a behavioral question, the matched
//!    story in their header). While the next question's reply is on its way the previous
//!    Approach stays, dimmed, rather than clearing. Under it, once the slow phase has started,
//!    the six expandable sections (#56): a compact grid of rows, keys `1`–`6`, and the body of
//!    the one that is open, which takes rows from the Approach down to a floor.
//! 3. Transcript and status: the last two transcript lines, then one line with the mic, system
//!    and model dots and the two most useful shortcuts for the current state.
//!
//! Density rules: every field is cut to its area with `…` instead of growing it, and nothing
//! expands on its own: a section opens only on its key, one at a time (`Question::open`).
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

use dmn_session::{Progress, Question, Session};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};

/// Rows the question block takes: four content lines inside a border.
const QUESTION_ROWS: u16 = 6;
/// Rows the transcript takes: a rule with its title, then two lines.
const TRANSCRIPT_ROWS: u16 = 3;
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
}

/// Everything the widgets read, worked out once per frame.
struct View<'a> {
    session: &'a Session,
    mode: Mode,
    /// A newer candidate's reply is on its way: the Approach on screen is the previous one.
    loading: bool,
    /// The live question's own reply is still streaming in.
    streaming: bool,
}

impl<'a> View<'a> {
    fn new(session: &'a Session) -> Self {
        let mode = match (&session.live, session.pinned) {
            (None, _) => Mode::Listening,
            (Some(_), true) => Mode::Pinned,
            (Some(_), false) => Mode::Live,
        };
        let live_candidate = session.live.as_ref().map(|q| q.candidate);
        // While pinned, a newer question queues instead of replacing this one, so the pinned
        // Approach is not "previous" and stays at full strength.
        // `r` dims the Approach on screen the same way until the new reply starts.
        let loading = (mode != Mode::Pinned
            && session
                .pending
                .is_some_and(|p| live_candidate.is_none_or(|c| c != p)))
            || session.live.as_ref().is_some_and(|q| q.regenerating);
        let streaming = session.live.as_ref().is_some_and(|q| {
            q.progress == Progress::Streaming && session.pending == Some(q.candidate)
        });
        Self {
            session,
            mode,
            loading,
            streaming,
        }
    }

    fn live(&self) -> Option<&'a Question> {
        self.session.live.as_ref()
    }
}

/// Draw the whole UI into a frame (what `dmn run` calls on every change).
pub fn draw(frame: &mut Frame, session: &Session) {
    let area = frame.area();
    render(session, area, frame.buffer_mut());
}

/// Draw the whole UI into `buf` within `area`.
pub fn render(session: &Session, area: Rect, buf: &mut Buffer) {
    let view = View::new(session);
    if session.assist_off {
        // #79: transcript first. The question and Approach areas would only ever be empty (or
        // stale), so their rows go to the transcript.
        let [t, s] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_ROWS)]).areas(area);
        status::render_transcript(&view, t, buf);
        status::render_status(&view, s, buf);
        return;
    }
    let [q, a, t, s] = Layout::vertical([
        Constraint::Length(QUESTION_ROWS),
        Constraint::Min(3),
        Constraint::Length(TRANSCRIPT_ROWS),
        Constraint::Length(STATUS_ROWS),
    ])
    .areas(area);
    question::render(&view, q, buf);
    match sections::plan(&view, a.width) {
        None => approach::render(&view, a, buf),
        Some(plan) => {
            let [approach_area, grid, body] = split_middle(&view, &plan, a);
            approach::render(&view, approach_area, buf);
            sections::render(&plan, grid, body, buf);
        }
    }
    status::render_transcript(&view, t, buf);
    status::render_status(&view, s, buf);
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
