//! The [`Session`] struct and its reducer.
//!
//! [`reduce`] is the only thing that changes a `Session`, and it is pure: it reads the action and
//! the current state, never a clock, a random number or anything outside. Replaying the same
//! actions in the same order therefore rebuilds the same `Session` ([`Session::replay`]).

use serde::{Deserialize, Serialize};

use crate::action::{
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, Dot, Health, IgnoreReason,
    PickerList, PickerOp, ProviderLabel, QuestionSource, Section, SectionState, SlowEnd, SlowField,
    SpeakLanguage, Speaker, StarCards, TradeoffItem, VoiceMode,
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
    /// #114: the line went out of `dmn-voice` translated into this language (`de`): the
    /// transcript labels it `You → de`. Left out of the JSON while unset, as the two below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken_as: Option<String>,
    /// #114: the translation the call heard, on the last line of the turn, dim under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
    /// #114: a marker the session wrote (`— translate mode on: → de —`), not speech: no
    /// speaker label, dim, never a candidate.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub note: bool,
}

impl Line {
    /// A spoken line: the fields a segment fills.
    pub fn spoken(speaker: Speaker, start: f64, end: f64, text: String) -> Self {
        Self {
            speaker,
            start,
            end,
            text,
            spoken_as: None,
            translation: None,
            note: false,
        }
    }
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
    /// #101: the web search the fast phase named because the answer depends on what is current;
    /// while it is set the Approach block says `may be out of date`. Empty for every other
    /// question, and then left out of the JSON, so every session saved before #101 loads and
    /// serialises unchanged. A regenerate or Swap starts the question afresh, so the tag follows
    /// the newest reply.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lookup: String,
    /// #102: the Approach on screen was rebuilt on web search results; the first result's host
    /// (`go.dev`). The Approach block then says `checked · <domain>`. Empty otherwise, and left
    /// out of the JSON, so every saved session loads and serialises unchanged.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked: String,
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
            lookup: String::new(),
            checked: String::new(),
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

    /// The next question takes the screen from this one (#94): it is kept as it stands and
    /// nothing fills it from here on. A section still loading (the next question cancelled its
    /// slow phase) settles on what had arrived, so its row never waits on a reply that is not
    /// coming, and a retry or Swap that had not started is over.
    fn archive(&mut self) {
        if self.regenerating {
            // As in `Session::settle`: what is on screen is the earlier, complete reply.
            self.regenerating = false;
            self.progress = Progress::Done;
        }
        self.no_other_story = false;
        if self.sections.started {
            self.sections.end(&SlowEnd::Done);
        }
        self.settle_open();
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
    /// The language whisper last detected in a final (#77), ISO 639-1; empty on the
    /// English-only path and until the first final. The status line shows it when it is not
    /// `en`.
    #[serde(default)]
    pub language: String,
    /// Percent of the local LLM in system memory rather than on the GPU (#79); 0 when it all
    /// fits, or before it is loaded.
    #[serde(default, skip_serializing_if = "no_offload")]
    pub llm_cpu_percent: u8,
    /// #102: the web lookup is on; the status line says `web` beside the model dot. Off in every
    /// log from before it, and left out of the JSON while off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub web: bool,
}

fn no_offload(cpu_percent: &u8) -> bool {
    *cpu_percent == 0
}

/// Where a `t` press (#109) is: the status line's tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeakStage {
    /// The user's last turn is with the model: `translating…`.
    Translating,
    /// The translation is with the TTS server, then playing into `dmn-voice`: `speaking…`.
    Speaking,
}

/// The `t` press in flight (#109). Only one at a time: a press while one is in flight does
/// nothing (#114: in translate mode the next turn waits for it instead).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Speech {
    pub id: u64,
    pub stage: SpeakStage,
    /// The translation being spoken, once it has arrived; the one dim line under the question.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Whole seconds since the press, for the status line.
    #[serde(default)]
    pub secs: u32,
    /// #114: the You lines this press speaks, `(from, to)` in session seconds, once
    /// [`Action::SpeakTurn`] has said; they are marked when the press is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<(f64, f64)>,
    /// #115: the language this press speaks, fixed when it started: a language chosen on the
    /// picker meanwhile applies to the next press, and the lines this one spoke are marked
    /// with this one. `None` in a log from before #115 (then the voice's target at the end).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// #130: a re-voice press: the voice its lines went out in (`pirate`, or [`YOUR_VOICE`]),
    /// fixed when it started. Its lines are marked `You → pirate`, with no translation under
    /// them (the call heard the words as said). `None` for a translating press.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoice: Option<String>,
}

/// #115: the language picker while it is open: an overlay over the three areas listing the
/// session's languages, the cursor on one of them. #130: or the voices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picker {
    /// The row the cursor is on: an index into [`Session::languages`], or for the voice
    /// picker into [`Session::voice_rows`] (0 is `your voice`).
    pub cursor: usize,
    /// #130: which list it shows; the languages in every log from before it.
    #[serde(default, skip_serializing_if = "is_languages")]
    pub list: PickerList,
}

fn is_languages(list: &PickerList) -> bool {
    *list == PickerList::Languages
}

impl Picker {
    /// #115: the language picker with its cursor on row `cursor`.
    pub fn languages(cursor: usize) -> Self {
        Self {
            cursor,
            list: PickerList::Languages,
        }
    }

    /// #130: the voice picker with its cursor on row `cursor` (0 is `your voice`).
    pub fn voices(cursor: usize) -> Self {
        Self {
            cursor,
            list: PickerList::Voices,
        }
    }
}

/// #130: how the voice picker, the status line and the transcript name the user's own voice.
pub const YOUR_VOICE: &str = "your voice";

/// #115: the transcript's marker line for a change of language.
pub fn language_marker(target: &str) -> String {
    format!("— language: → {target} —")
}

/// #114: what `dmn-voice` carries, for the status line's first tag and the transcript's marks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voice {
    pub mode: VoiceMode,
    /// `[speak].target`: the language translate mode speaks (`de`).
    pub target: String,
    /// The language the user speaks, when the config says (`en`); the tag says `you (en)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<String>,
    /// #130: the library voice chosen with `o` (`[speak].voice`), which speaks translations
    /// and re-voiced turns; `None` is the user's own voice. Kept through a change of mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<String>,
}

impl Voice {
    /// #130: the chosen voice's name, or [`YOUR_VOICE`].
    pub fn label(&self) -> &str {
        self.chosen.as_deref().unwrap_or(YOUR_VOICE)
    }
}

/// #114: the transcript's marker line for a mode change; #130: re-voice names its voice.
pub fn voice_marker(mode: VoiceMode, target: &str, voice: &str) -> String {
    match mode {
        VoiceMode::Translate => format!("— translate mode on: → {target} —"),
        VoiceMode::Revoice => format!("— re-voice on: {voice} —"),
        VoiceMode::Passthrough => "— translate mode off: your own voice —".to_owned(),
    }
}

/// #130: the transcript's marker line for a change of voice.
pub fn voice_choice_marker(voice: &str) -> String {
    format!("— voice: {voice} —")
}

/// A `t` press that spoke nothing (#109), on the status line for 5 s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakNotice {
    pub id: u64,
    pub reason: String,
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
    /// The last thing said on a lane [`Session::question_source`] selects, for the `LISTENING`
    /// state before any question (#90). Keeps its `last_them` wire name: with the default
    /// source it is exactly the last Them line, which is every session log before #90.
    #[serde(rename = "last_them")]
    pub last_heard: Option<String>,
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
    /// #63: the latest continuation, `(candidate, continued)`: that candidate's question
    /// replaces the continued one's where it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuing: Option<(u64, u64)>,
    /// Interview assistance is off (#79): the pipeline runs no detector and no LLM request, and
    /// the UI shows the transcript alone. Stored inverted so a default session, and every log
    /// from before #79, has assistance on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assist_off: bool,
    /// Whose questions this session detects and answers (#90). Fixed for the session; left out
    /// of the log at its default, so every session log from before #90 reads as `them`.
    #[serde(default, skip_serializing_if = "is_default_source")]
    pub question_source: QuestionSource,
    /// #96: the candidates Enter sent whose reply is still on its way, oldest first. They are
    /// told apart from the detector's only so that one the model declines is answered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asked: Vec<u64>,
    /// #96: the model answered `is_question: false` or `small_talk` to this candidate, which
    /// Enter sent. The status line says `not a question` while it is set: 5 s, until
    /// [`Action::NoticeExpired`] or the next Enter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_a_question: Option<u64>,
    /// #96: the request for this candidate, which Enter sent, failed (a timeout, an unreachable
    /// model, a malformed reply). The key is for the moment the tool let the user down, so the
    /// pipeline reads this to let Enter send the same turn again; the next Enter clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_failed: Option<u64>,
    /// #102: the web lookup for this question failed (a timeout, an error, no results, no key).
    /// The status line says `lookup failed` while it is set: 5 s, until
    /// [`Action::NoticeExpired`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookup_failed: Option<u64>,
    /// #94: every question shown before the live one, oldest first, each as it stood when the
    /// next one took the screen from it (its focus, Approach, STAR cards, clarifications and
    /// sections). In memory only: the store has its own rows and does not read these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Question>,
    /// #94: the earlier question on screen, as its place in `history`; `None` on the live one.
    /// While it is set it holds the screen the way a pin does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browsing: Option<usize>,
    /// #94: questions that arrived while an earlier one held the screen; the status line counts
    /// them (`2 new`) until the user is back on the live question.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unseen: usize,
    /// #94: rows the open section of the question on screen is scrolled down
    /// ([`Action::Scroll`]). 0 whenever no section is open, and again when the section closes
    /// or another question takes the screen.
    #[serde(default, skip_serializing_if = "not_scrolled")]
    pub scroll: u16,
    /// #109: `t` speaks in this session (`[speak].enabled`). Off in every log from before it,
    /// and left out of the JSON while off, as are the two fields after it while empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub speak: bool,
    /// #109: the `t` press in flight, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<Speech>,
    /// #109: why the last `t` press spoke nothing, for 5 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speak_failed: Option<SpeakNotice>,
    /// #114: what `dmn-voice` carries, once `dmn run` has said; `None` in every log from
    /// before it and while `[speak]` is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<Voice>,
    /// #115: the languages the session can speak, each with its enrolled voice, in the config's
    /// order; empty in every log from before it and while `[speak]` is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<SpeakLanguage>,
    /// #115: the language picker, while it is open. #130: or the voice picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picker: Option<Picker>,
    /// #130: the TTS server's voices the voice picker offers after `your voice`
    /// ([`Action::SpeakVoices`]); empty in every log from before it and while `[speak]` is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<String>,
}

fn not_scrolled(rows: &u16) -> bool {
    *rows == 0
}

/// The default question source is left out of a serialised session.
fn is_default_source(source: &QuestionSource) -> bool {
    *source == QuestionSource::default()
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

    /// The earlier question `←` stepped back to (#94), while one holds the screen.
    pub fn earlier(&self) -> Option<&Question> {
        self.history.get(self.browsing?)
    }

    /// #115: the language `dmn-voice` translates into from the next utterance on (`es`): the
    /// picker's last choice, else `[speak].target`. `None` while `[speak]` is off.
    pub fn speak_target(&self) -> Option<&str> {
        self.voice.as_ref().map(|v| v.target.as_str())
    }

    /// #115: the language the picker's cursor is on, while it is open.
    pub fn picked(&self) -> Option<&SpeakLanguage> {
        let picker = self.picker?;
        if picker.list != PickerList::Languages {
            return None;
        }
        self.languages.get(picker.cursor)
    }

    /// #130: the voice picker's rows: `None` (the user's own voice, first) and each of
    /// [`Session::voices`].
    pub fn voice_rows(&self) -> Vec<Option<&str>> {
        std::iter::once(None)
            .chain(self.voices.iter().map(|v| Some(v.as_str())))
            .collect()
    }

    /// #130: the chosen voice (`None`: the user's own), while `[speak]` is on.
    pub fn chosen_voice(&self) -> Option<&str> {
        self.voice.as_ref()?.chosen.as_deref()
    }

    /// #130: where the voice picker's cursor starts: on the current voice, else `your voice`.
    fn current_voice_row(&self) -> usize {
        let chosen = self.chosen_voice();
        self.voice_rows()
            .iter()
            .position(|row| *row == chosen)
            .unwrap_or(0)
    }

    /// #115: where the picker's cursor starts: on the current language, else the first.
    fn current_language(&self) -> usize {
        let target = self.speak_target();
        self.languages
            .iter()
            .position(|l| Some(l.code.as_str()) == target)
            .unwrap_or(0)
    }

    /// #115: one marker row after everything said so far (#114's mode marker is one too).
    fn push_marker(&mut self, text: String) {
        let at = self
            .transcript
            .iter()
            .chain(&self.partials)
            .map(|l| l.end)
            .fold(0.0, f64::max);
        let mut marker = Line::spoken(Speaker::Unknown, at, at, text);
        marker.note = true;
        self.transcript.push(marker);
        self.prune_transcript(at);
    }

    /// The question on screen: the earlier one while browsing (#94), else the live one.
    pub fn shown(&self) -> Option<&Question> {
        self.earlier().or(self.live.as_ref())
    }

    fn shown_mut(&mut self) -> Option<&mut Question> {
        match self.browsing {
            Some(at) if at < self.history.len() => self.history.get_mut(at),
            _ => self.live.as_mut(),
        }
    }

    /// Where the earlier question on screen is among every question shown, counted from 1, and
    /// how many there are with the live one (#94): `(2, 5)` is "QUESTION 2 of 5".
    pub fn place(&self) -> Option<(usize, usize)> {
        let at = self.browsing.filter(|at| *at < self.history.len())?;
        Some((
            at + 1,
            self.history.len() + usize::from(self.live.is_some()),
        ))
    }

    /// `next` takes the live area (#94): the question it replaces joins the history.
    fn show(&mut self, next: Question) {
        if let Some(mut old) = self.live.replace(next) {
            old.archive();
            self.history.push(old);
        }
    }

    /// What the scroll offset belongs to (#94): one open section of one question.
    fn scrolled(&self) -> Option<(u64, Section)> {
        let q = self.shown()?;
        Some((q.candidate, q.open?))
    }

    /// Every question still being filled: the live one and those queued behind a pin.
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

    /// Keep the transcript to its window behind the newest end, `end` included.
    fn prune_transcript(&mut self, end: f64) {
        let latest = self.transcript.iter().map(|l| l.end).fold(end, f64::max);
        self.transcript
            .retain(|l| l.end >= latest - TRANSCRIPT_WINDOW_S);
    }

    /// Candidate `candidate`'s reply has ended: if Enter sent it (#96), it is no longer waiting.
    /// True when it was.
    fn take_asked(&mut self, candidate: u64) -> bool {
        let before = self.asked.len();
        self.asked.retain(|c| *c != candidate);
        self.asked.len() != before
    }

    fn settle(&mut self, candidate: u64, progress: &Progress) {
        // #96: a key press whose request failed can be pressed again for the same turn.
        if self.take_asked(candidate) && matches!(progress, Progress::Failed(_)) {
            self.ask_failed = Some(candidate);
        }
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
    let scrolled = s.scrolled();
    apply(s, action);
    // #94: whatever closed the section or changed the question on screen (a key, a new
    // question, an unpin, a row that settled empty), the next thing opened starts at its top.
    if s.scrolled() != scrolled {
        s.scroll = 0;
    }
}

fn apply(s: &mut Session, action: &Action) {
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
            language,
            ..
        } => {
            s.partials
                .retain(|p| !(p.speaker == *speaker && p.start == *start));
            let line = Line::spoken(*speaker, *start, *end, text.clone());
            if !*is_final {
                if !text.is_empty() {
                    s.partials.push(line);
                }
                return;
            }
            // #77: finals only, so a partial's early guess never shows.
            if let Some(lang) = language {
                s.status.language.clone_from(lang);
            }
            if text.is_empty() {
                return;
            }
            // #90: the LISTENING screen shows the last line from a lane that can ask, so under
            // `you` or `both` your own last line is what the user sees Daemon heard.
            if s.question_source.allows(*speaker) {
                s.last_heard = Some(text.clone());
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
            s.prune_transcript(*end);
        }
        Action::Candidate { id, .. } => {
            s.counts.candidates += 1;
            s.pending = Some(*id);
        }
        Action::Continues {
            candidate,
            continued,
        } => {
            s.continuing = Some((*candidate, *continued));
        }
        Action::Asked { candidate } => {
            s.asked.push(*candidate);
            // A new press is a new attempt: the answer to the last one goes, and so does its
            // failure.
            s.not_a_question = None;
            s.ask_failed = None;
        }
        Action::NoticeExpired { candidate } => {
            if s.not_a_question == Some(*candidate) {
                s.not_a_question = None;
            }
            if s.lookup_failed == Some(*candidate) {
                s.lookup_failed = None;
            }
        }
        Action::Question {
            candidate,
            text,
            kind,
        } => {
            let mut q = Question::new(*candidate, text.clone(), kind.clone());
            // #63: the rest of a question split at a pause takes the first part's place, pinned
            // or queued, as a fresh question: the first part's reply was for half a question.
            if let Some((c, continued)) = s.continuing
                && c == *candidate
                && let Some(existing) = s.question_mut(continued)
            {
                *existing = q;
                s.continuing = None;
                return;
            }
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
            // #94: behind an earlier question it is answered as any is, and counted.
            if s.earlier().is_some() {
                s.unseen += 1;
            }
            if s.pinned && s.live.is_some() {
                s.queued.push(q);
            } else {
                s.show(q);
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
        Action::Lookup { candidate, query } => {
            // A clarification's reply keeps its query in its stored payload only: the tag belongs
            // to the question whose Approach is on screen.
            if let Some(q) = s.question_mut(*candidate) {
                q.lookup.clone_from(query);
            }
        }
        Action::Checked { candidate, domain } => {
            if let Some(q) = s.question_mut(*candidate) {
                q.checked.clone_from(domain);
            }
        }
        Action::LookupFailed { candidate } => s.lookup_failed = Some(*candidate),
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
            // #96: a key press gets an answer either way; a detector candidate stays silent.
            if s.take_asked(*candidate) {
                s.not_a_question = Some(*candidate);
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
            if let Some(q) = s.shown_mut() {
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
                s.show(newest);
            }
            s.queued.clear();
        }
        Action::Earlier => {
            let from = s.place().map_or(s.history.len(), |(at, _)| at - 1);
            // On the first question of the session there is nothing before it.
            if let Some(at) = from.checked_sub(1) {
                s.browsing = Some(at);
            }
        }
        Action::Later => {
            // `at` counts from 1, so it is the next one's place in `history`; past the newest
            // earlier question is the live one.
            s.browsing = s
                .place()
                .map(|(at, _)| at)
                .filter(|at| *at < s.history.len());
            if s.browsing.is_none() {
                s.unseen = 0;
            }
        }
        Action::Scroll { offset } => {
            if s.scrolled().is_some() {
                s.scroll = *offset;
            }
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
        Action::Assist { enabled } => s.assist_off = !enabled,
        Action::QuestionSource { source } => s.question_source = *source,
        Action::LlmOffload { cpu_percent } => s.status.llm_cpu_percent = *cpu_percent,
        Action::WebLookup { enabled } => s.status.web = *enabled,
        Action::SpeakEnabled { enabled } => s.speak = *enabled,
        Action::Voice {
            mode,
            target,
            own,
            chosen,
        } => {
            let was = s.voice.as_ref().map(|v| v.mode);
            // #130: the start-up voice comes with the first; after that the chosen voice is
            // [`Action::SpeakVoice`]'s, and a change of mode keeps it.
            let chosen = match &s.voice {
                Some(v) => v.chosen.clone(),
                None => chosen.clone(),
            };
            let voice = Voice {
                mode: *mode,
                target: target.clone(),
                own: own.clone(),
                chosen,
            };
            // A change (not the start-up mode) is a line in the history, after everything
            // said so far.
            if was.is_some_and(|m| m != *mode) {
                s.push_marker(voice_marker(*mode, target, voice.label()));
            }
            s.voice = Some(voice);
        }
        Action::SpeakLanguages { languages } => {
            s.languages.clone_from(languages);
            // The list changed under the cursor: the picker starts afresh next time.
            if s.picker.is_some_and(|p| p.list == PickerList::Languages) {
                s.picker = None;
            }
        }
        Action::Picker { op } => {
            if !s.speak {
                return;
            }
            // #130: the voice picker always has a row (`your voice`); it needs a voice state.
            let rows = |s: &Session, list: PickerList| match list {
                PickerList::Languages => s.languages.len(),
                PickerList::Voices if s.voice.is_some() => s.voices.len() + 1,
                PickerList::Voices => 0,
            };
            s.picker = match (*op, s.picker) {
                (PickerOp::Open, None) if rows(s, PickerList::Languages) > 0 => {
                    Some(Picker::languages(s.current_language()))
                }
                (PickerOp::OpenVoices, None) if rows(s, PickerList::Voices) > 0 => {
                    Some(Picker::voices(s.current_voice_row()))
                }
                (PickerOp::Open | PickerOp::OpenVoices, open) => open,
                (PickerOp::Up, Some(p)) => Some(Picker {
                    cursor: p
                        .cursor
                        .saturating_sub(1)
                        .min(rows(s, p.list).saturating_sub(1)),
                    ..p
                }),
                (PickerOp::Down, Some(p)) => Some(Picker {
                    cursor: (p.cursor + 1).min(rows(s, p.list).saturating_sub(1)),
                    ..p
                }),
                (PickerOp::Up | PickerOp::Down, None) => None,
                (PickerOp::Close, _) => None,
            };
        }
        Action::SpeakVoices { voices } => {
            s.voices.clone_from(voices);
            // Refreshed while the voice picker is open: the cursor stays on a row.
            if let Some(p) = s.picker.as_mut().filter(|p| p.list == PickerList::Voices) {
                p.cursor = p.cursor.min(voices.len());
            }
        }
        Action::SpeakVoice { voice } => {
            if s.picker.is_some_and(|p| p.list == PickerList::Voices) {
                s.picker = None;
            }
            let Some(v) = s.voice.as_mut() else {
                return;
            };
            if v.chosen != *voice {
                v.chosen.clone_from(voice);
                let label = v.label().to_owned();
                s.push_marker(voice_choice_marker(&label));
            }
        }
        Action::SpeakTarget { target } => {
            if s.picker.is_some_and(|p| p.list == PickerList::Languages) {
                s.picker = None;
            }
            let known = s.languages.is_empty() || s.languages.iter().any(|l| &l.code == target);
            if !known {
                return;
            }
            let Some(v) = s.voice.as_mut() else {
                return;
            };
            if v.target != *target {
                v.target.clone_from(target);
                s.push_marker(language_marker(target));
            }
        }
        Action::SpeakStarted { id } => {
            // #130: in re-voice mode the voice it speaks in; nothing is translated, so the
            // press is speaking from the start.
            let revoice = s
                .voice
                .as_ref()
                .filter(|v| v.mode == VoiceMode::Revoice)
                .map(|v| v.label().to_owned());
            s.speech = Some(Speech {
                id: *id,
                stage: if revoice.is_some() {
                    SpeakStage::Speaking
                } else {
                    SpeakStage::Translating
                },
                text: String::new(),
                secs: 0,
                span: None,
                // #115: the language this press speaks, whatever the picker chooses meanwhile.
                target: s.speak_target().map(str::to_owned),
                revoice,
            });
            // A new press is a new attempt: the last one's failure goes.
            s.speak_failed = None;
        }
        Action::SpeakTurn { id, from, to } => {
            if let Some(sp) = s.speech.as_mut().filter(|sp| sp.id == *id) {
                sp.span = Some((*from, *to));
            }
        }
        Action::SpeakTranslated { id, text } => {
            if let Some(sp) = s.speech.as_mut().filter(|sp| sp.id == *id) {
                sp.stage = SpeakStage::Speaking;
                sp.text.clone_from(text);
            }
        }
        Action::SpeakElapsed { id, secs } => {
            if let Some(sp) = s.speech.as_mut().filter(|sp| sp.id == *id) {
                sp.secs = *secs;
            }
        }
        Action::SpeakDone { id } => {
            if s.speech.as_ref().is_some_and(|sp| sp.id == *id) {
                let done = s.speech.take();
                // #114: the lines the call heard translated are marked so, the translation
                // under the last of them. #115: in the language the press started in, not
                // one chosen while it was in flight.
                if let Some(sp) = done
                    && let Some((from, to)) = sp.span
                    && let Some(target) = sp
                        .revoice
                        .clone()
                        .or_else(|| sp.target.clone())
                        .or_else(|| s.voice.as_ref().map(|v| v.target.clone()))
                    && !sp.text.is_empty()
                {
                    // #130: a re-voiced turn went out as said: no translation under it.
                    let under = sp.revoice.is_none().then_some(sp.text);
                    let mut last = None;
                    for (i, l) in s.transcript.iter_mut().enumerate() {
                        if l.speaker == Speaker::You && !l.note && l.start >= from && l.start <= to
                        {
                            l.spoken_as = Some(target.clone());
                            l.translation = None;
                            last = Some(i);
                        }
                    }
                    if let Some(i) = last {
                        s.transcript[i].translation = under;
                    }
                }
            }
        }
        Action::SpeakFailed { id, reason } => {
            if s.speech.as_ref().is_some_and(|sp| sp.id == *id) {
                s.speech = None;
            }
            s.speak_failed = Some(SpeakNotice {
                id: *id,
                reason: reason.clone(),
            });
        }
        Action::SpeakNoticeExpired { id } => {
            if s.speak_failed.as_ref().is_some_and(|n| n.id == *id) {
                s.speak_failed = None;
            }
        }
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
