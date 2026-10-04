//! Everything that can change a [`Session`](crate::Session), as plain serialisable data.
//!
//! Actions carry their own facts: times are in the action (never read from a clock by the
//! reducer) and ids are assigned by whoever dispatches (candidate ids by the pipeline, in order).
//! That is what makes a recorded action log replay to an identical `Session`, and what lets the
//! store writer turn each action into rows without asking anyone.
//!
//! The types here are this crate's own, not `dmn-transcript`'s, `dmn-detect`'s or `dmn-llm`'s:
//! `dmn-core` maps pipeline values into actions, so this crate depends on none of them.

use serde::{Deserialize, Serialize};

/// Which capture stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Interviewer,
    You,
    Unknown,
}

/// Whose questions are detected and answered (#90), as `[detect].question_source` spells it. It
/// selects eligible lanes, not voice identities: a line's [`Speaker`] never depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuestionSource {
    #[default]
    Interviewer,
    You,
    Both,
}

impl QuestionSource {
    /// Whether a final from `speaker` is scored at all.
    pub fn allows(self, speaker: Speaker) -> bool {
        match self {
            QuestionSource::Interviewer => speaker == Speaker::Interviewer,
            QuestionSource::You => speaker == Speaker::You,
            QuestionSource::Both => matches!(speaker, Speaker::Interviewer | Speaker::You),
        }
    }

    /// The config-file spelling, which is also the status line's tag (`asks: you`).
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionSource::Interviewer => "interviewer",
            QuestionSource::You => "you",
            QuestionSource::Both => "both",
        }
    }
}

/// A status dot (SPEC § Transcript & status): mic, system audio, model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dot {
    Mic,
    System,
    Model,
}

/// The state of one status dot. `Error` carries the tooltip text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "detail")]
pub enum Health {
    #[default]
    Unknown,
    Ok,
    Muted,
    /// Streaming, but from the default sink monitor: every sound the machine plays, not just the
    /// call app, because the app was not found or none was chosen. The SPEC's "capturing all
    /// system audio" badge.
    AllAudio,
    Error(String),
}

/// Which LLM provider answers, for the model dot (#59).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLabel {
    /// The config spelling: `ollama`, `openai` or `anthropic`.
    pub name: String,
    pub model: String,
    /// Transcript text leaves this machine (SPEC § Providers): a remote provider, or Ollama on
    /// another host.
    pub remote: bool,
}

/// STAR cards for a behavioral question (SPEC § AI Assistance).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarCards {
    pub situation: String,
    pub task: String,
    pub action: String,
    pub result: String,
    pub story: Option<String>,
}

/// What Deeper Answer may hold for a coding question (#50, SPEC § AI Assistance): hints only
/// (insight, data structure, edge cases, complexity) or a worked solution as well. Set from
/// `[ui].coding_solutions` or `dmn run --solutions`, toggled with `s`; the M2 slow phase reads
/// it. Nothing in M1 generates either, so today it shows only in the footer and the log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodingSolutions {
    #[default]
    Hints,
    Full,
}

impl CodingSolutions {
    /// The other mode: what `s` switches to.
    pub fn toggled(self) -> Self {
        match self {
            CodingSolutions::Hints => CodingSolutions::Full,
            CodingSolutions::Full => CodingSolutions::Hints,
        }
    }

    /// The name used in the config, on the command line, in the footer and in the store.
    pub fn label(self) -> &'static str {
        match self {
            CodingSolutions::Hints => "hints",
            CodingSolutions::Full => "full",
        }
    }

    /// Parse a [`label`](Self::label).
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "hints" => Some(CodingSolutions::Hints),
            "full" => Some(CodingSolutions::Full),
            _ => None,
        }
    }
}

/// Why the fast phase put nothing on screen for a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    NotAQuestion,
    SmallTalk,
}

/// A moment in a candidate's life that `dmn bench --live` times (#16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The detector emitted the candidate.
    Emitted,
    /// The fast phase produced its question line (or a clarification's `↳` line).
    Question,
    /// The fast phase produced its first Approach line (or the STAR cards).
    Approach,
}

/// One of the six expandable sections under the Approach (SPEC § AI Assistance), in the fixed
/// order the UI shows them. The slow phase (#55) fills them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Deeper,
    Followups,
    Tradeoffs,
    Example,
    Mention,
    ResumeContext,
}

impl Section {
    /// All six, in on-screen order (keys `1`–`6`).
    pub const ALL: [Section; 6] = [
        Section::Deeper,
        Section::Followups,
        Section::Tradeoffs,
        Section::Example,
        Section::Mention,
        Section::ResumeContext,
    ];

    /// The row title (SPEC § AI Assistance).
    pub fn title(self) -> &'static str {
        match self {
            Section::Deeper => "Deeper Answer",
            Section::Followups => "Follow-up Questions",
            Section::Tradeoffs => "Tradeoffs",
            Section::Example => "Example",
            Section::Mention => "Things I Should Mention",
            Section::ResumeContext => "Resume Context",
        }
    }
}

/// Where one section's slow-phase content is. A section keeps whatever content it had through
/// every state: a regenerate, a restart or a timeout never blanks it (SPEC: "on timeout the UI
/// keeps the previous section content and shows a small 'slow' tag").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "error")]
pub enum SectionState {
    /// Not filled yet: the UI shows the "…" hint.
    #[default]
    Loading,
    /// Filled.
    Done,
    /// The slow phase finished with nothing for this section: the UI hides the row.
    Empty,
    /// The 20 s deadline passed first: the UI keeps what it has and shows the "slow" tag.
    Slow,
    /// The request failed.
    Failed(String),
}

/// A Deeper Answer entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeeperItem {
    pub label: String,
    pub text: String,
}

/// A Tradeoffs row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeoffItem {
    pub option: String,
    pub pro: String,
    pub con: String,
}

/// An Example, or a worked solution in Deeper Answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeBlock {
    pub language: String,
    pub code: String,
}

/// A Resume Context entry: a source file the prompt carried, and what from it fits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub source: String,
    pub text: String,
}

/// One slow-phase field, already within limits (`dmn-llm`'s `limits.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum SlowField {
    Deeper {
        items: Vec<DeeperItem>,
    },
    Followups {
        items: Vec<String>,
    },
    Tradeoffs {
        items: Vec<TradeoffItem>,
    },
    Example {
        code: Option<CodeBlock>,
    },
    Mention {
        items: Vec<String>,
    },
    ResumeContext {
        items: Vec<Citation>,
    },
    /// A worked solution, part of Deeper Answer: coding questions in `full` mode only (#50).
    Solution {
        code: CodeBlock,
    },
}

/// How a slow phase ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "end", content = "error")]
pub enum SlowEnd {
    /// The reply closed; any section still loading had nothing and is empty.
    Done,
    /// The 20 s deadline passed; sections still loading are marked slow.
    TimedOut,
    /// The request failed; sections still loading are marked failed.
    Failed(String),
    /// The run was stopped (`q`); sections are left as they were.
    Stopped,
}

/// One change to the session. Serialised with a `type` tag, one JSON object per action, so an
/// action log is JSON Lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// A session begins. `at` is unix epoch seconds.
    Started { label: String, at: i64 },
    /// A transcript segment from the bus: a partial of an open utterance, or its final. Times are
    /// seconds from the session start. An empty final closes a dropped utterance.
    Segment {
        speaker: Speaker,
        start: f64,
        end: f64,
        text: String,
        is_final: bool,
        avg_logprob: f64,
        /// The language whisper detected in the audio (ISO 639-1) when `[stt].language` is not
        /// `en` (#77); the text is English either way. Absent in older logs and on the
        /// English-only path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// The detector proposed the final interviewer segment that starts at `start` as a question.
    /// `id` is assigned by the pipeline and tags every fast-phase action for this candidate.
    Candidate {
        id: u64,
        speaker: Speaker,
        start: f64,
        end: f64,
        text: String,
        score: i32,
    },
    /// #63: candidate `candidate` continues `continued`, the same question split at a pause
    /// ("And if the state lived in Redis … and Redis went down, what then?"). Its reply replaces
    /// `continued`'s question where it is (live, pinned or queued) instead of arriving as a
    /// second question. Dispatched right after that candidate's [`Action::Candidate`].
    Continues { candidate: u64, continued: u64 },
    /// Enter (#96): candidate `candidate` was sent by the key, not found by the detector: the
    /// asking lane's last turn, for a question the detector missed. Dispatched right after that
    /// candidate's [`Action::Candidate`]. Its reply takes the path any candidate's does, except
    /// that a key press gets an answer either way: on [`Action::Ignored`] the status line says
    /// `not a question` until [`Action::NoticeExpired`].
    Asked { candidate: u64 },
    /// The `not a question` notice for candidate `candidate` has been up for its 5 s (#96). The
    /// pipeline, which has a clock, says when; the reducer has none.
    NoticeExpired { candidate: u64 },
    /// Fast phase: the extracted question, first field of a shown reply.
    Question {
        candidate: u64,
        text: String,
        kind: String,
    },
    /// Fast phase: a refinement of the question on screen.
    Clarification { candidate: u64, text: String },
    /// Fast phase: the focus line.
    Focus { candidate: u64, text: String },
    /// Fast phase: approach line `index` (0-based, contiguous), already length-limited.
    ApproachLine {
        candidate: u64,
        index: usize,
        text: String,
    },
    /// Fast phase: STAR cards for a behavioral question.
    Star { candidate: u64, cards: StarCards },
    /// The fast phase finished and showed something. `payload` is the prompt-contract JSON of
    /// exactly what was shown, for the store.
    Answered {
        candidate: u64,
        clarification: bool,
        payload: String,
        at: i64,
    },
    /// The fast phase decided the candidate is not to be shown. Nothing on screen changes for a
    /// detector candidate; one Enter sent (#96) gets `not a question` on the status line.
    Ignored {
        candidate: u64,
        reason: IgnoreReason,
        score: i32,
    },
    /// The fast phase failed (timeout, unreachable, malformed). Anything already shown stays.
    Failed { candidate: u64, error: String },
    /// The slow phase (#55) starts (or restarts) for a question: every section of it is loading
    /// again, keeping the content it has until new content replaces it.
    SlowStarted { candidate: u64 },
    /// One slow-phase field arrived for a question; its section is done (or empty).
    Slow { candidate: u64, field: SlowField },
    /// The slow phase for a question ended. `payload` is the prompt-contract JSON of the fields
    /// that arrived, for the store; `at` is unix epoch seconds.
    SlowEnded {
        candidate: u64,
        end: SlowEnd,
        payload: String,
        at: i64,
    },
    /// #57, session memory: the one summary line for question `candidate`, written once the
    /// candidate has answered it ("03:12 rate limiter — covered token bucket, Redis, fail-open;
    /// promised to return to clock skew"), and what they promised to come back to, if anything.
    /// An open promise shows in Things I Should Mention on every later question until a You
    /// segment covers it. `at` is unix epoch seconds.
    Summary {
        candidate: u64,
        line: String,
        promise: Option<String>,
        at: i64,
    },
    /// `r`: ask the fast phase again for a question already on screen (or queued), with the
    /// "different angle" note at temperature 0.7. The new fields arrive tagged with the same
    /// candidate id and replace that question in place, pinned or not; until the new question
    /// arrives the old Approach stays, dimmed.
    Regenerate { candidate: u64 },
    /// The stories matched to a behavioral question (#58), best first, as `stories/<file>`
    /// labels. The first is the one its STAR cards were built from; Swap walks down the list.
    Stories { candidate: u64, ranked: Vec<String> },
    /// Swap (#58): build the question's STAR block from the next-ranked story, wrapping round to
    /// the first. The new cards arrive tagged with the same candidate id, as after `r`; until
    /// then the old ones stay, dimmed. With fewer than two stories nothing is asked of the
    /// model, and the STAR header says there is no other story.
    SwapStory { candidate: u64 },
    /// Keys `1`–`6` (#56): open `section` under the Approach of the question on screen (the live
    /// one, or the earlier one `←` stepped back to, #94), closing the one that was open, or close
    /// it if it is the open one. A row that is not on screen (no slow phase yet, or nothing for
    /// this question) does not open.
    ToggleSection { section: Section },
    /// `←` (#94): show the question before the one on screen, as it stood when the next one
    /// replaced it. It holds the screen the way a pin does: questions that arrive meanwhile are
    /// answered behind it and counted. On the first question of the session nothing changes.
    Earlier,
    /// `→` (#94): show the question after the earlier one on screen; from the newest earlier one
    /// that is the live question again. On the live question nothing changes.
    Later,
    /// `↑ ↓ PgUp PgDn` (#94): the open section of the question on screen is scrolled `offset`
    /// rows down. The offset is absolute and already within the body: the UI thread, which knows
    /// how many rows the pane gives the section, works it out; the reducer knows no pane. It goes
    /// back to 0 when the section closes or another question takes the screen.
    Scroll { offset: u16 },
    /// `p`: pin the live question; new questions queue behind it.
    Pin,
    /// The coding-solutions mode from now on: dispatched once after `Started` (from the config
    /// or `--solutions`) and again on each `s`.
    CodingSolutions { mode: CodingSolutions },
    /// Unpin: the newest queued question (if any) becomes live.
    Unpin,
    /// Interview assistance on or off from now on (#79): dispatched once after `Started` (from
    /// `[assist].enabled` or `--no-assist`) and again on each Ctrl+Shift+A. Off, the pipeline runs no
    /// detector and asks the LLM nothing, and the screen is the transcript alone.
    Assist { enabled: bool },
    /// Whose questions this session detects and answers (#90): dispatched once after `Started`,
    /// from `[detect].question_source` or `--question-source`. Fixed for the session, so
    /// rendering stays a function of session state. Labels, routing and the transcript are the
    /// same whatever it is.
    QuestionSource { source: QuestionSource },
    /// A status dot changed.
    Status { dot: Dot, health: Health },
    /// The LLM provider now answering (#59): at start and after a config reload switched it.
    /// The model dot is named after it, and a remote one says transcript text leaves the machine.
    Provider { label: ProviderLabel },
    /// How much of the local model Ollama holds in system memory rather than on the GPU, in
    /// percent (#79), once it is loaded: non-zero when whisper took the GPU first (a session
    /// started with assistance off, then Ctrl+Shift+A). The status line names it.
    LlmOffload { cpu_percent: u8 },
    /// Ring-buffer frames dropped so far, over every capture lane (SPEC § Threading rules: a
    /// full ring drops the incoming frame and counts it). Shown beside the dots while non-zero.
    Overruns { total: u64 },
    /// Mic finals the echo guard has dropped so far as copies of the loopback (#60, SPEC §
    /// Audio capture). For the session log and `dmn bench --live`; the screen does not change.
    EchoDropped { total: u64 },
    /// Where whisper transcribes (#61), once the models are loaded: `device` is a short label
    /// ("GPU NVIDIA GeForce RTX 4090 (Vulkan0, …)", "CPU"); `fallback` says a GPU was asked for
    /// and the CPU runs instead. Shown on the status line.
    Transcriber { device: String, fallback: bool },
    /// When candidate `candidate` reached `stage`, in audio seconds from the session start (the
    /// clock segment times use). For the session log and `dmn bench --live`; the screen does not
    /// change.
    Timing {
        candidate: u64,
        stage: Stage,
        at: f64,
    },
    /// The process had used `cpu` seconds of CPU, whisper `whisper` seconds of inference, at
    /// audio time `at`, `wall` seconds after the session started. For the session log and
    /// `dmn bench --live`; the screen does not change.
    Cpu {
        at: f64,
        wall: f64,
        cpu: f64,
        whisper: f64,
    },
    /// The session ends. `at` is unix epoch seconds.
    Ended { at: i64 },
}
