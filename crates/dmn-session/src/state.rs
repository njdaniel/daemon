//! The [`Session`] struct and its reducer.
//!
//! [`reduce`] is the only thing that changes a `Session`, and it is pure: it reads the action and
//! the current state, never a clock, a random number or anything outside. Replaying the same
//! actions in the same order therefore rebuilds the same `Session` ([`Session::replay`]).

use serde::{Deserialize, Serialize};

use crate::action::{
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, Dot, Health, IgnoreReason,
    ProviderLabel, Section, SectionState, SlowEnd, SlowField, Speaker, StarCards, TradeoffItem,
};
use crate::memory::{self, Promise, SummaryLine};

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

/// The state of each of the six sections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionStates {
    pub deeper: SectionState,
    pub followups: SectionState,
    pub tradeoffs: SectionState,
    pub example: SectionState,
    pub mention: SectionState,
    pub resume_context: SectionState,
}

impl SectionStates {
    pub fn get(&self, section: Section) -> &SectionState {
        match section {
            Section::Deeper => &self.deeper,
            Section::Followups => &self.followups,
            Section::Tradeoffs => &self.tradeoffs,
            Section::Example => &self.example,
            Section::Mention => &self.mention,
            Section::ResumeContext => &self.resume_context,
        }
    }

    fn get_mut(&mut self, section: Section) -> &mut SectionState {
        match section {
            Section::Deeper => &mut self.deeper,
            Section::Followups => &mut self.followups,
            Section::Tradeoffs => &mut self.tradeoffs,
            Section::Example => &mut self.example,
            Section::Mention => &mut self.mention,
            Section::ResumeContext => &mut self.resume_context,
        }
    }
}

/// What the slow phase (#55) filled for a question: the six expandable sections and each one's
/// state. Deeper Answer also holds the worked solution of a coding question in `full` mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sections {
    pub deeper: Vec<DeeperItem>,
    pub solution: Option<CodeBlock>,
    pub followups: Vec<String>,
    pub tradeoffs: Vec<TradeoffItem>,
    pub example: Option<CodeBlock>,
    pub mention: Vec<String>,
    pub resume_context: Vec<Citation>,
    pub states: SectionStates,
    /// A slow phase has started for this question. Until then the sections are not shown at all:
    /// a question answered with the slow phase off (`slow_timeout_ms = 0`) or an M1-era log has
    /// no rows, rather than six "…" hints that never resolve.
    #[serde(default)]
    pub started: bool,
}

impl Sections {
    /// Nothing filled and every section still loading: how a question starts, and what an
    /// M1-era session log or fixture holds (the field is left out of their JSON).
    pub fn is_untouched(&self) -> bool {
        *self == Sections::default()
    }

    pub fn state(&self, section: Section) -> &SectionState {
        self.states.get(section)
    }

    /// Whether `section`'s row is on screen (#56, SPEC § AI Assistance): only once a slow phase
    /// has started; a row still loading shows (with its "…" hint), a row that timed out shows
    /// (with its "slow" tag), and a row with nothing for this question is hidden. A failed row
    /// shows only what it already had: errors appear only as status dots.
    pub fn visible(&self, section: Section) -> bool {
        if !self.started {
            return false;
        }
        match self.state(section) {
            SectionState::Loading | SectionState::Slow => true,
            SectionState::Done | SectionState::Empty | SectionState::Failed(_) => {
                self.has_content(section)
            }
        }
    }

    /// Whether `section` has anything to show.
    pub fn has_content(&self, section: Section) -> bool {
        match section {
            Section::Deeper => !self.deeper.is_empty() || self.solution.is_some(),
            Section::Followups => !self.followups.is_empty(),
            Section::Tradeoffs => !self.tradeoffs.is_empty(),
            Section::Example => self.example.is_some(),
            Section::Mention => !self.mention.is_empty(),
            Section::ResumeContext => !self.resume_context.is_empty(),
        }
    }

    /// Store one field and settle its section: done when it has content, empty when not.
    fn set(&mut self, field: &SlowField) {
        self.started = true;
        let section = match field {
            SlowField::Deeper { items } => {
                self.deeper = items.clone();
                // A new Deeper Answer replaces the old one whole, solution included: `solution`
                // is the last field of the schema, so a phase that has one sends it after this.
                // A regenerate in hints mode (or a model that sent none) never shows the old code.
                self.solution = None;
                Section::Deeper
            }
            SlowField::Solution { code } => {
                self.solution = Some(code.clone());
                Section::Deeper
            }
            SlowField::Followups { items } => {
                self.followups = items.clone();
                Section::Followups
            }
            SlowField::Tradeoffs { items } => {
                self.tradeoffs = items.clone();
                Section::Tradeoffs
            }
            SlowField::Example { code } => {
                self.example = code.clone();
                Section::Example
            }
            SlowField::Mention { items } => {
                self.mention = items.clone();
                Section::Mention
            }
            SlowField::ResumeContext { items } => {
                self.resume_context = items.clone();
                Section::ResumeContext
            }
        };
        *self.states.get_mut(section) = if self.has_content(section) {
            SectionState::Done
        } else {
            SectionState::Empty
        };
    }

    /// Things I Should Mention with every open promise first (#57), each once. They come on top
    /// of the slow phase's own items: a promise is the candidate's, not the model's.
    fn add_promises(&mut self, promises: &[Promise]) {
        let items: Vec<String> = promises
            .iter()
            .filter(|p| !p.mentioned)
            .map(Promise::mention_item)
            .filter(|i| !self.mention.contains(i))
            .collect();
        if items.is_empty() {
            return;
        }
        self.mention.splice(0..0, items);
        self.settle_mention();
    }

    /// A promise was covered: its item goes.
    fn drop_promise(&mut self, promise: &Promise) {
        let item = promise.mention_item();
        let before = self.mention.len();
        self.mention.retain(|i| *i != item);
        if self.mention.len() != before {
            self.settle_mention();
        }
    }

    /// A decided Mention row follows its content: shown when it has some, hidden when not. A row
    /// still loading, slow or failed keeps its state.
    fn settle_mention(&mut self) {
        let has = self.has_content(Section::Mention);
        let state = &mut self.states.mention;
        match state {
            SectionState::Empty if has => *state = SectionState::Done,
            SectionState::Done if !has => *state = SectionState::Empty,
            _ => {}
        }
    }

    /// A new (or restarted) slow phase: every section is loading again, content kept.
    fn start(&mut self) {
        self.started = true;
        // Content stays until the new phase replaces it; a solution goes when the new Deeper
        // Answer arrives (see `set`).
        for section in Section::ALL {
            *self.states.get_mut(section) = SectionState::Loading;
        }
    }

    /// The slow phase ended: settle every section still loading.
    fn end(&mut self, end: &SlowEnd) {
        let settled = match end {
            SlowEnd::Done => SectionState::Empty,
            SlowEnd::TimedOut => SectionState::Slow,
            SlowEnd::Failed(error) => SectionState::Failed(error.clone()),
            SlowEnd::Stopped => return,
        };
        for section in Section::ALL {
            if *self.states.get(section) != SectionState::Loading {
                continue;
            }
            // Done with nothing new: a section that still shows earlier content is done.
            let next = if settled == SectionState::Empty && self.has_content(section) {
                SectionState::Done
            } else {
                settled.clone()
            };
            *self.states.get_mut(section) = next;
        }
    }
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
    /// The six expandable sections the slow phase fills (#55).
    #[serde(default, skip_serializing_if = "Sections::is_untouched")]
    pub sections: Sections,
    /// The one section expanded under this question's Approach (#56), set only by a key press
    /// ([`Action::ToggleSection`]). Per question, so a new question always starts collapsed:
    /// nothing expands on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<Section>,
    /// The stories matched to this behavioral question, best first (#58).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stories: Vec<String>,
    /// Which of `stories` the STAR block is built from; Swap moves it on.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub story: usize,
    /// Swap was pressed with no other story to swap to: the STAR header says so until the
    /// question changes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_other_story: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Question {
    /// The story the STAR block is built from, if any was matched.
    pub fn story_label(&self) -> Option<&str> {
        self.stories.get(self.story).map(String::as_str)
    }

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
            sections: Sections::default(),
            open: None,
            stories: Vec::new(),
            story: 0,
            no_other_story: false,
        }
    }

    /// Close the open section if its row is no longer on screen (it settled empty, or failed
    /// with nothing), so `open` always names a row the UI shows. Closing is not expanding.
    fn settle_open(&mut self) {
        if self.open.is_some_and(|s| !self.sections.visible(s)) {
            self.open = None;
        }
    }
}

/// The three status dots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub mic: Health,
    pub system: Health,
    pub model: Health,
    /// Ring-buffer frames dropped so far (#16); shown beside the dots while non-zero.
    #[serde(default)]
    pub overruns: u64,
    /// The provider answering (#59); `None` until the suggester has one (and in M1 logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderLabel>,
    /// Where whisper transcribes (#61); empty until the models are loaded (and in older logs).
    #[serde(default)]
    pub stt: String,
    /// A GPU was asked for and the CPU runs instead.
    #[serde(default)]
    pub stt_fallback: bool,
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
    /// What Deeper Answer may hold for coding questions (#50); shown in the footer.
    #[serde(default)]
    pub coding_solutions: CodingSolutions,
    /// #57, session memory: one line per answered question, in question order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<SummaryLine>,
    /// #57: what the candidate promised to come back to, open or since covered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promises: Vec<Promise>,
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

    /// The last [`memory::PROMPT_LINES`] summary lines, oldest first: what every prompt carries.
    pub fn summary_for_prompt(&self) -> Vec<String> {
        let skip = self.summary.len().saturating_sub(memory::PROMPT_LINES);
        self.summary[skip..]
            .iter()
            .map(|l| l.line.clone())
            .collect()
    }

    /// Every question on screen: the live one and those queued behind a pin.
    fn questions_mut(&mut self) -> impl Iterator<Item = &mut Question> {
        self.live.iter_mut().chain(self.queued.iter_mut())
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
            if *speaker == Speaker::You {
                let kept: Vec<Promise> = s
                    .promises
                    .iter_mut()
                    .filter(|p| !p.mentioned && memory::covers(&p.text, text))
                    .map(|p| {
                        p.mentioned = true;
                        p.clone()
                    })
                    .collect();
                for p in &kept {
                    s.questions_mut().for_each(|q| {
                        q.sections.drop_promise(p);
                        q.settle_open();
                    });
                }
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
            let mut q = Question::new(*candidate, text.clone(), kind.clone());
            // A regenerated reply replaces its own question where it is, pinned or queued. Its
            // sections stay until the new slow phase replaces them (#55).
            if let Some(existing) = s.question_mut(*candidate) {
                q.sections = std::mem::take(&mut existing.sections);
                q.open = existing.open;
                // A swap's (or `r`'s) reply keeps the ranking and which story it was built from.
                q.stories = std::mem::take(&mut existing.stories);
                q.story = existing.story;
                *existing = q;
                return;
            }
            // #57: a new question starts with the promises still open.
            q.sections.add_promises(&s.promises);
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
        Action::SlowStarted { candidate } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.sections.start();
            }
        }
        Action::Slow { candidate, field } => {
            let promises = s.promises.clone();
            if let Some(q) = s.question_mut(*candidate) {
                q.sections.set(field);
                if matches!(field, SlowField::Mention { .. }) {
                    q.sections.add_promises(&promises);
                }
                q.settle_open();
            }
        }
        Action::Summary {
            candidate,
            line,
            promise,
            ..
        } => {
            match s.summary.iter_mut().find(|l| l.candidate == *candidate) {
                Some(existing) => existing.line = line.clone(),
                None => {
                    let at = s.summary.partition_point(|l| l.candidate < *candidate);
                    s.summary.insert(
                        at,
                        SummaryLine {
                            candidate: *candidate,
                            line: line.clone(),
                        },
                    );
                }
            }
            let text = promise.as_deref().map(str::trim).unwrap_or_default();
            if !text.is_empty() && !s.promises.iter().any(|p| p.candidate == *candidate) {
                let promise = Promise {
                    candidate: *candidate,
                    text: text.to_owned(),
                    mentioned: false,
                };
                // It shows on the questions after the one it was made in.
                for q in s.questions_mut().filter(|q| q.candidate != *candidate) {
                    q.sections.add_promises(std::slice::from_ref(&promise));
                }
                s.promises.push(promise);
            }
        }
        Action::SlowEnded { candidate, end, .. } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.sections.end(end);
                q.settle_open();
            }
        }
        Action::ToggleSection { section } => {
            if let Some(q) = s.live.as_mut() {
                if q.open == Some(*section) {
                    q.open = None;
                } else if q.sections.visible(*section) {
                    // Opening one closes the other: exactly one open at a time.
                    q.open = Some(*section);
                }
            }
        }
        Action::Regenerate { candidate } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.regenerating = true;
                q.progress = Progress::Streaming;
                s.pending = Some(*candidate);
            }
        }
        Action::Stories { candidate, ranked } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.stories = ranked.clone();
                q.story = 0;
                q.no_other_story = false;
            }
        }
        Action::SwapStory { candidate } => {
            if let Some(q) = s.question_mut(*candidate) {
                if q.stories.len() < 2 {
                    q.no_other_story = true;
                } else {
                    // Round the ranking and back to the first.
                    q.story = (q.story + 1) % q.stories.len();
                    q.no_other_story = false;
                    q.regenerating = true;
                    q.progress = Progress::Streaming;
                    s.pending = Some(*candidate);
                }
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
        Action::Overruns { total } => s.status.overruns = *total,
        Action::Provider { label } => s.status.provider = Some(label.clone()),
        Action::Transcriber { device, fallback } => {
            s.status.stt.clone_from(device);
            s.status.stt_fallback = *fallback;
        }
        Action::CodingSolutions { mode } => s.coding_solutions = *mode,
        // Measurements for the session log; nothing on screen depends on them.
        Action::Timing { .. } | Action::Cpu { .. } | Action::EchoDropped { .. } => {}
    }
}

/// Labels the store and logs use for an [`IgnoreReason`].
pub fn ignore_label(reason: IgnoreReason) -> &'static str {
    match reason {
        IgnoreReason::NotAQuestion => "not_a_question",
        IgnoreReason::SmallTalk => "small_talk",
    }
}
