//! The [`Session`] struct and its reducer.
//!
//! [`reduce`] is the only thing that changes a `Session`, and it is pure: it reads the action and
//! the current state, never a clock, a random number or anything outside. Replaying the same
//! actions in the same order therefore rebuilds the same `Session` ([`Session::replay`]).

use serde::{Deserialize, Serialize};

use crate::action::{Action, Dot, Health, IgnoreReason, Speaker, StarCards};

/// How much transcript the session keeps: the SPEC's prompt window is the last 60 s, and the UI
/// shows the last two lines, so nothing older is ever read from here.
pub const TRANSCRIPT_WINDOW_S: f64 = 60.0;

/// One transcript line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub speaker: Speaker,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Where a question's fast phase is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "error")]
pub enum Progress {
    /// Fields are still streaming in.
    #[default]
    Streaming,
    Done,
    /// The request failed after this much was shown; the UI keeps it and marks it.
    Failed(String),
}

/// A clarification shown as a sub-line under the live question (SPEC § Extraction: "shown as a
/// sub-line under the current question instead of replacing it"). Its own approach lines, if the
/// model sent any, stay with it: the parent question's Approach is kept (#13 decision; the SPEC
/// only says the question is not replaced).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clarification {
    pub candidate: u64,
    pub text: String,
    pub approach: Vec<String>,
    pub progress: Progress,
}

/// A question the fast phase confirmed, with everything the fast phase filled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub candidate: u64,
    pub text: String,
    pub kind: String,
    pub focus: String,
    pub approach: Vec<String>,
    pub star: Option<StarCards>,
    pub clarifications: Vec<Clarification>,
    pub progress: Progress,
    /// `r` was pressed and the new reply has not started yet: the UI dims this question's
    /// Approach, as it dims the previous one while a new question loads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub regenerating: bool,
}

impl Question {
    fn new(candidate: u64, text: String, kind: String) -> Self {
        Self {
            candidate,
            text,
            kind,
            focus: String::new(),
            approach: Vec::new(),
            star: None,
            clarifications: Vec::new(),
            progress: Progress::Streaming,
            regenerating: false,
        }
    }
}

/// The three status dots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub mic: Health,
    pub system: Health,
    pub model: Health,
}

/// Running totals, for the status line and the end-of-run summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub candidates: u64,
    pub shown: u64,
    pub clarifications: u64,
    pub ignored: u64,
    pub failed: u64,
}

/// All live state. The TUI (M1) and the Tauri bridge (M3) render this and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub label: String,
    /// Unix epoch seconds; `None` until [`Action::Started`].
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    /// Finals from the last [`TRANSCRIPT_WINDOW_S`], oldest first.
    pub transcript: Vec<Line>,
    /// The open utterance of each speaker, if any (its latest partial).
    pub partials: Vec<Line>,
    /// The last thing the interviewer said, for the `LISTENING` state before any question.
    pub last_interviewer: Option<String>,
    /// The question on screen.
    pub live: Option<Question>,
    /// While pinned, new questions queue instead of replacing the live one.
    pub pinned: bool,
    /// Questions that arrived while pinned, oldest first; the UI shows their count as a badge.
    pub queued: Vec<Question>,
    /// The candidate whose fast phase is running: the previous Approach dims while it is set.
    pub pending: Option<u64>,
    pub status: Status,
    pub counts: Counts,
}

impl Session {
    /// Rebuild a session from an action log.
    pub fn replay<'a>(actions: impl IntoIterator<Item = &'a Action>) -> Self {
        let mut s = Session::default();
        for a in actions {
            reduce(&mut s, a);
        }
        s
    }

    /// The question (live or queued) a candidate's fast-phase fields belong to.
    fn question_mut(&mut self, candidate: u64) -> Option<&mut Question> {
        if self.live.as_ref().is_some_and(|q| q.candidate == candidate) {
            return self.live.as_mut();
        }
        self.queued.iter_mut().find(|q| q.candidate == candidate)
    }

    fn clarification_mut(&mut self, candidate: u64) -> Option<&mut Clarification> {
        self.live
            .as_mut()?
            .clarifications
            .iter_mut()
            .find(|c| c.candidate == candidate)
    }

    fn settle(&mut self, candidate: u64, progress: &Progress) {
        if self.pending == Some(candidate) {
            self.pending = None;
        }
        if let Some(q) = self.question_mut(candidate) {
            // Still `regenerating` means the new reply never started: the question on screen
            // is the earlier, complete one, and a failed retry leaves it as it was.
            q.progress = if q.regenerating {
                Progress::Done
            } else {
                progress.clone()
            };
            q.regenerating = false;
        } else if let Some(c) = self.clarification_mut(candidate) {
            c.progress = progress.clone();
        }
    }
}

/// Put `index` into `lines`: overwrite an existing line, append the next one, ignore a gap (the
/// fast phase sends lines contiguously, so a gap means a malformed action).
fn set_line(lines: &mut Vec<String>, index: usize, text: &str) {
    match index.cmp(&lines.len()) {
        std::cmp::Ordering::Less => lines[index] = text.to_owned(),
        std::cmp::Ordering::Equal => lines.push(text.to_owned()),
        std::cmp::Ordering::Greater => {}
    }
}

/// Apply one action. Pure: the result depends only on `s` and `action`.
pub fn reduce(s: &mut Session, action: &Action) {
    match action {
        Action::Started { label, at } => {
            *s = Session {
                label: label.clone(),
                started_at: Some(*at),
                ..Session::default()
            };
        }
        Action::Ended { at } => s.ended_at = Some(*at),
        Action::Segment {
            speaker,
            start,
            end,
            text,
            is_final,
            ..
        } => {
            s.partials
                .retain(|p| !(p.speaker == *speaker && p.start == *start));
            let line = Line {
                speaker: *speaker,
                start: *start,
                end: *end,
                text: text.clone(),
            };
            if !*is_final {
                if !text.is_empty() {
                    s.partials.push(line);
                }
                return;
            }
            if text.is_empty() {
                return;
            }
            if *speaker == Speaker::Interviewer {
                s.last_interviewer = Some(text.clone());
            }
            s.transcript.push(line);
            let latest = s.transcript.iter().map(|l| l.end).fold(*end, f64::max);
            s.transcript
                .retain(|l| l.end >= latest - TRANSCRIPT_WINDOW_S);
        }
        Action::Candidate { id, .. } => {
            s.counts.candidates += 1;
            s.pending = Some(*id);
        }
        Action::Question {
            candidate,
            text,
            kind,
        } => {
            let q = Question::new(*candidate, text.clone(), kind.clone());
            // A regenerated reply replaces its own question where it is, pinned or queued.
            if let Some(existing) = s.question_mut(*candidate) {
                *existing = q;
                return;
            }
            if s.pinned && s.live.is_some() {
                s.queued.push(q);
            } else {
                s.live = Some(q);
            }
        }
        Action::Clarification { candidate, text } => match s.live.as_mut() {
            Some(live) => live.clarifications.push(Clarification {
                candidate: *candidate,
                text: text.clone(),
                approach: Vec::new(),
                progress: Progress::Streaming,
            }),
            // Nothing on screen to refine: show it as a question of its own.
            None => {
                s.live = Some(Question::new(
                    *candidate,
                    text.clone(),
                    "clarification".into(),
                ));
            }
        },
        Action::Focus { candidate, text } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.focus = text.clone();
            }
        }
        Action::ApproachLine {
            candidate,
            index,
            text,
        } => {
            if let Some(q) = s.question_mut(*candidate) {
                set_line(&mut q.approach, *index, text);
            } else if let Some(c) = s.clarification_mut(*candidate) {
                set_line(&mut c.approach, *index, text);
            }
        }
        Action::Star { candidate, cards } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.star = Some(cards.clone());
            }
        }
        Action::Answered {
            candidate,
            clarification,
            ..
        } => {
            if *clarification {
                s.counts.clarifications += 1;
            } else {
                s.counts.shown += 1;
            }
            s.settle(*candidate, &Progress::Done);
        }
        Action::Ignored { candidate, .. } => {
            s.counts.ignored += 1;
            if s.pending == Some(*candidate) {
                s.pending = None;
            }
            // A regenerate the model declined leaves the question as it was.
            if let Some(q) = s.question_mut(*candidate).filter(|q| q.regenerating) {
                q.regenerating = false;
                q.progress = Progress::Done;
            }
        }
        Action::Failed { candidate, error } => {
            s.counts.failed += 1;
            s.settle(*candidate, &Progress::Failed(error.clone()));
        }
        Action::Regenerate { candidate } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.regenerating = true;
                q.progress = Progress::Streaming;
                s.pending = Some(*candidate);
            }
        }
        Action::Pin => {
            if s.live.is_some() {
                s.pinned = true;
            }
        }
        Action::Unpin => {
            s.pinned = false;
            // SPEC § Live Question: "unpin shows the newest".
            if let Some(newest) = s.queued.pop() {
                s.live = Some(newest);
            }
            s.queued.clear();
        }
        Action::Status { dot, health } => {
            let slot = match dot {
                Dot::Mic => &mut s.status.mic,
                Dot::System => &mut s.status.system,
                Dot::Model => &mut s.status.model,
            };
            *slot = health.clone();
        }
    }
}

/// Labels the store and logs use for an [`IgnoreReason`].
pub fn ignore_label(reason: IgnoreReason) -> &'static str {
    match reason {
        IgnoreReason::NotAQuestion => "not_a_question",
        IgnoreReason::SmallTalk => "small_talk",
    }
}
