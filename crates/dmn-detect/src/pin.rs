//! Which question is on screen, and what happens to the ones that arrive while it is pinned.
//!
//! A pure state machine: no channels, no clock, no I/O. `dmn-session` drives it in #13 and the TUI
//! reads it, so the rule "a new question never replaces a pinned one" is testable on its own.

use crate::Candidate;

/// What [`PinState::offer`] did with a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offered {
    /// It became the live question.
    Shown,
    /// A question is pinned, so this one waits behind the "n new" badge.
    Queued,
}

/// The live question, the pin, and the queue behind it (docs/SPEC.md § Question detector: "a new
/// question never replaces a pinned one; it is queued and shown as a small '1 new' badge").
#[derive(Debug, Clone, Default)]
pub struct PinState {
    current: Option<Candidate>,
    pinned: bool,
    queue: Vec<Candidate>,
}

impl PinState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The question on screen.
    pub fn current(&self) -> Option<&Candidate> {
        self.current.as_ref()
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// How many questions are waiting: the badge's number.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Offer a new candidate. It replaces the live question unless one is pinned.
    pub fn offer(&mut self, candidate: Candidate) -> Offered {
        if self.pinned {
            self.queue.push(candidate);
            Offered::Queued
        } else {
            self.current = Some(candidate);
            Offered::Shown
        }
    }

    /// Pin the live question. Pinning with nothing on screen does nothing.
    pub fn pin(&mut self) -> bool {
        if self.current.is_some() {
            self.pinned = true;
        }
        self.pinned
    }

    /// Unpin and show the newest queued question, dropping the ones it overtook: the other person
    /// has moved on, and an older question is not worth showing after the fact.
    pub fn unpin(&mut self) -> Option<&Candidate> {
        self.pinned = false;
        if let Some(newest) = self.queue.pop() {
            self.current = Some(newest);
            self.queue.clear();
        }
        self.current.as_ref()
    }

    /// Drop everything: a new session, or the user clearing the view.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dmn_audio::Source;

    use super::*;
    use crate::Cue;

    fn candidate(text: &str, at: u64) -> Candidate {
        Candidate {
            source: Source::Them,
            start: Duration::from_secs(at),
            end: Duration::from_secs(at + 5),
            text: text.to_owned(),
            line: text.to_owned(),
            score: 5,
            cues: vec![Cue::QuestionMark, Cue::Them],
        }
    }

    #[test]
    fn without_a_pin_the_newest_question_is_the_live_one() {
        let mut p = PinState::new();
        assert_eq!(p.offer(candidate("first?", 10)), Offered::Shown);
        assert_eq!(p.offer(candidate("second?", 20)), Offered::Shown);
        assert_eq!(p.current().map(|c| c.text.as_str()), Some("second?"));
        assert_eq!(p.queued(), 0);
    }

    #[test]
    fn a_pinned_question_is_never_replaced_and_the_badge_counts_the_rest() {
        let mut p = PinState::new();
        p.offer(candidate("pinned?", 10));
        assert!(p.pin());
        assert_eq!(p.offer(candidate("second?", 20)), Offered::Queued);
        assert_eq!(p.offer(candidate("third?", 30)), Offered::Queued);
        assert_eq!(p.current().map(|c| c.text.as_str()), Some("pinned?"));
        assert_eq!(p.queued(), 2);
    }

    #[test]
    fn unpinning_shows_the_newest_and_forgets_the_rest() {
        let mut p = PinState::new();
        p.offer(candidate("pinned?", 10));
        p.pin();
        p.offer(candidate("second?", 20));
        p.offer(candidate("third?", 30));
        assert_eq!(p.unpin().map(|c| c.text.as_str()), Some("third?"));
        assert_eq!(p.queued(), 0);
        assert!(!p.is_pinned());
    }

    #[test]
    fn unpinning_with_an_empty_queue_keeps_the_question_on_screen() {
        let mut p = PinState::new();
        p.offer(candidate("pinned?", 10));
        p.pin();
        assert_eq!(p.unpin().map(|c| c.text.as_str()), Some("pinned?"));
    }

    #[test]
    fn pinning_nothing_does_nothing_and_reset_clears() {
        let mut p = PinState::new();
        assert!(!p.pin());
        p.offer(candidate("q?", 10));
        p.pin();
        p.offer(candidate("q2?", 20));
        p.reset();
        assert!(p.current().is_none());
        assert!(!p.is_pinned());
        assert_eq!(p.queued(), 0);
    }
}
