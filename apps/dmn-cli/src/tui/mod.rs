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
//! 2. Approach: the numbered lines (or the STAR cards for a behavioral question). While the next
//!    question's reply is on its way the previous Approach stays, dimmed, rather than clearing.
//! 3. Transcript and status: the last two transcript lines, then one line with the mic, system
//!    and model dots and the two most useful shortcuts for the current state.
//!
//! Density rules: every field is cut to its area with `…` instead of growing it, nothing
//! expands on its own, and there are no expandable sections yet (the slow phase that fills them
//! is later work), so "one section open at a time" holds trivially.

mod approach;
mod question;
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
    let [q, a, t, s] = Layout::vertical([
        Constraint::Length(QUESTION_ROWS),
        Constraint::Min(3),
        Constraint::Length(TRANSCRIPT_ROWS),
        Constraint::Length(STATUS_ROWS),
    ])
    .areas(area);
    question::render(&view, q, buf);
    approach::render(&view, a, buf);
    status::render_transcript(&view, t, buf);
    status::render_status(&view, s, buf);
}
