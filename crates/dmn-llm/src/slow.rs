//! The slow phase (#55): after the fast phase has put a question and its Approach on screen, a
//! second request fills the six expandable sections (SPEC § Prompt contract, § AI Assistance):
//! `deeper`, `followups`, `tradeoffs`, `example`, `mention`, `resume_context`, and for a coding
//! question in the `full` coding-solutions mode (#50) a worked `solution`.
//!
//! The reply streams like the fast phase's and is parsed incrementally ([`IncrementalObject`]),
//! so each section is published ([`SlowField`]) the moment its field completes, in the order the
//! UI shows them. Every field is cut in Rust ([`limits`]) before it is published: list sizes,
//! per-item lengths, code line counts. `resume_context` keeps only items citing a file the
//! request carried, and `solution` is dropped unless the question is coding in `full` mode, so
//! hints mode never carries complete code whatever the model sends.
//!
//! The deadline is 20 s ([`SLOW_PHASE_TIMEOUT`]). A timeout or a cancel ends the stream with
//! the error; what was already published stays with the caller, which keeps it on screen and
//! marks the rest (SPEC: "on timeout the UI keeps the previous section content and shows a small
//! 'slow' tag").
//!
//! #57, session memory: when the candidate has answered an earlier question (the question shown
//! before this one), the request carries that question and what the candidate said, and the
//! reply adds a one-line `summary` of the answer and the `promise` the candidate made, if any
//! ("I'll come back to clock skew"). They are published as [`SlowField::Summary`] and
//! [`SlowField::Promise`], after the six sections, and belong to the earlier question: the
//! caller turns them into its session summary line. The slow phase is the only place a question's
//! answer is summarised, so session memory costs no extra request.
//!
//! The system prompt is the fast phase's own, word for word, and the user message starts as the
//! fast phase's does; only the instructions after it differ. Ollama keeps the last prompt's KV
//! cache, so a slow request between two questions leaves the next fast request's shared prefix
//! cached instead of making it re-read the whole system prompt.
//!
//! #101: so the slow phase gets the fast phase's date (`## Today`) and its rule for answers that
//! depend on what is current; `slow.md` repeats that rule for the sections, and a question the
//! fast phase flagged (`lookup` not empty) says so under the Approach, so no section builds on a
//! remembered version.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::limits::{self, Flag, FlagKind};
use crate::prompt::{self, FastRequest};
use crate::provider::{ChatRequest, LlmError, Message, Provider};
use crate::schema::{Deeper, Example, Kind, ResumeContext, Star, Tradeoff, slow_schema};
use crate::stream_json::{IncrementalObject, JsonEvent};

const INSTRUCTIONS: &str = include_str!("../prompts/slow.md");
const SUMMARY_INSTRUCTIONS: &str = include_str!("../prompts/summary.md");

/// SPEC § Providers: "Timeouts: fast phase 4 s, slow phase 20 s".
pub const SLOW_PHASE_TIMEOUT: Duration = Duration::from_secs(20);

/// What Deeper Answer may hold for a coding question (#50): `dmn-llm`'s own copy of the
/// session's mode, so this crate depends on no session type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CodingMode {
    #[default]
    Hints,
    Full,
}

/// The question shown before this one and what the candidate said while it was on screen: what
/// the slow phase summarises for session memory (#57).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviousAnswer {
    pub question: String,
    /// The candidate's own words, oldest first.
    pub answer: String,
}

/// Everything the slow phase is asked about: the question on screen as the fast phase left it,
/// and the same context the fast phase had (re-retrieved with the question and its focus).
#[derive(Debug, Clone, PartialEq)]
pub struct SlowRequest {
    /// Job requirements, retrieved chunks, transcript, and in `candidate` what the Interviewer
    /// said, rendered as the fast phase renders them. `current` and `regenerate` are ignored.
    pub context: FastRequest,
    /// The question on screen, as the fast phase extracted it.
    pub question: String,
    pub kind: Kind,
    pub focus: String,
    /// The Approach already on screen, so the sections add to it instead of repeating it.
    pub approach: Vec<String>,
    pub star: Option<Star>,
    /// #101: the fast phase's `lookup` for this question; not empty when it flagged the answer as
    /// depending on what is current, and then the sections must not build on a remembered version.
    pub lookup: String,
    pub coding: CodingMode,
    /// The file of each chunk in `context.chunks`, in order: the only sources `resume_context`
    /// may cite.
    pub chunk_files: Vec<String>,
    /// The earlier question to summarise (#57); `None` asks for no summary.
    pub previous: Option<PreviousAnswer>,
    pub timeout: Duration,
}

impl SlowRequest {
    pub fn new(question: impl Into<String>, kind: Kind) -> Self {
        Self {
            context: FastRequest::default(),
            question: question.into(),
            kind,
            focus: String::new(),
            approach: Vec::new(),
            star: None,
            lookup: String::new(),
            coding: CodingMode::Hints,
            chunk_files: Vec::new(),
            previous: None,
            timeout: SLOW_PHASE_TIMEOUT,
        }
    }

    /// A summary of the previous answer is asked for, and accepted, only when there is one with
    /// something the candidate said.
    pub fn wants_summary(&self) -> bool {
        self.previous
            .as_ref()
            .is_some_and(|p| !p.answer.trim().is_empty())
    }

    /// A worked solution is asked for, and accepted, only for a coding question in `full` mode.
    pub fn wants_solution(&self) -> bool {
        self.kind == Kind::Coding && self.coding == CodingMode::Full
    }
}

/// The kind-specific line after the shared instructions.
fn kind_note(req: &SlowRequest) -> &'static str {
    match (req.kind, req.coding) {
        (Kind::Coding, CodingMode::Hints) => {
            "Coding, hints mode: deeper gives the key insight, the data structure, edge cases and \
             pitfalls, and the time and space complexity. No complete code anywhere: example is \
             at most a signature or the few lines of the core step. Code is in the language the \
             job requirements name, else Python."
        }
        (Kind::Coding, CodingMode::Full) => {
            "Coding, full mode: deeper gives the key insight, the data structure, edge cases and \
             pitfalls, and the time and space complexity; also fill solution {language, code}: a \
             complete, working solution of at most 40 lines. Code is in the language the job \
             requirements name, else Python."
        }
        (Kind::SystemDesign, _) => {
            "System design: tradeoffs compares the main design choices; example can be a capacity \
             estimate or a schema sketch."
        }
        (Kind::Behavioral, _) => {
            "Behavioral: deeper names what makes a strong answer (the stakes, the candidate's own \
             actions, a measurable result); tradeoffs is usually empty; example is \"text\"."
        }
        (Kind::Knowledge, _) => {
            "Knowledge: example is one concrete instance; tradeoffs compares it with its closest \
             alternative."
        }
        (Kind::Clarification | Kind::SmallTalk, _) => "",
    }
}

/// System + user messages for the slow phase. See the module doc for why the system prompt and
/// the start of the user message are the fast phase's.
pub fn slow_messages(req: &SlowRequest) -> Vec<Message> {
    let shared = FastRequest {
        current: None,
        regenerate: false,
        swap: false,
        ..req.context.clone()
    };
    let mut user = prompt::user_message(&shared);
    user.push_str(&format!(
        "\n## Question on screen ({})\n{}\n",
        req.kind.as_str(),
        req.question.trim()
    ));
    if !req.focus.trim().is_empty() {
        user.push_str(&format!("Focus: {}\n", req.focus.trim()));
    }
    if !req.approach.is_empty() {
        user.push_str("Approach already shown:\n");
        for (i, line) in req.approach.iter().enumerate() {
            user.push_str(&format!("{}. {}\n", i + 1, line.trim()));
        }
    }
    if let Some(star) = &req.star {
        user.push_str(&format!(
            "STAR already shown: situation: {}; task: {}; action: {}; result: {}\n",
            star.situation, star.task, star.action, star.result
        ));
    }
    if let Some(facts) = req.context.facts.as_ref().filter(|f| !f.facts.is_empty()) {
        // #102: the Approach above was rebuilt on the search results, which this request
        // carries too.
        user.push_str(&format!(
            "Checked on the web (search: {}): the Approach was built on the Fresh facts above. \
             Build every section on them too, not on what you remember; where they say nothing, \
             say so.\n",
            facts.query.trim()
        ));
    } else if !req.lookup.trim().is_empty() {
        user.push_str(&format!(
            "Flagged as depending on what is current (to look up: {}): the Approach may be out \
             of date. No section names a version, release or date as the current one or builds \
             on one.\n",
            req.lookup.trim()
        ));
    }
    if let Some(prev) = req.previous.as_ref().filter(|_| req.wants_summary()) {
        user.push_str(&format!(
            "\n## Previous question and the candidate's answer\nPrevious question: {}\nCandidate said: {}\n",
            prev.question.trim(),
            prev.answer.trim()
        ));
    }
    user.push('\n');
    user.push_str(INSTRUCTIONS.trim_end());
    user.push('\n');
    let note = kind_note(req);
    if !note.is_empty() {
        user.push_str(note);
        user.push('\n');
    }
    if req.wants_summary() {
        user.push('\n');
        user.push_str(SUMMARY_INSTRUCTIONS.trim_end());
        user.push('\n');
    }
    vec![
        // #90: the lane that asked chooses the base rules, as it does for the fast phase.
        Message::system(prompt::system_prompt(None, req.context.asker)),
        Message::user(user),
    ]
}

/// The request the slow phase sends: messages, the slow schema, temperature 0, its deadline.
pub fn slow_chat_request(req: &SlowRequest) -> ChatRequest {
    ChatRequest::new(slow_messages(req))
        .with_schema(slow_schema(req.wants_solution(), req.wants_summary()))
        .with_temperature(0.0)
        .with_timeout(req.timeout)
}

/// One section's content, published as soon as its field completes, already within limits.
#[derive(Debug, Clone, PartialEq)]
pub enum SlowField {
    Deeper(Vec<Deeper>),
    Followups(Vec<String>),
    Tradeoffs(Vec<Tradeoff>),
    /// `None`: the model had no example (empty code).
    Example(Option<Example>),
    Mention(Vec<String>),
    ResumeContext(Vec<ResumeContext>),
    /// Coding in `full` mode only; part of Deeper Answer.
    Solution(Example),
    /// #57: one line on how the previous question was answered (not a section).
    Summary(String),
    /// #57: what the candidate promised to come back to in that answer; empty when nothing.
    Promise(String),
}

impl SlowField {
    /// The schema field this came from.
    pub fn name(&self) -> &'static str {
        match self {
            SlowField::Deeper(_) => "deeper",
            SlowField::Followups(_) => "followups",
            SlowField::Tradeoffs(_) => "tradeoffs",
            SlowField::Example(_) => "example",
            SlowField::Mention(_) => "mention",
            SlowField::ResumeContext(_) => "resume_context",
            SlowField::Solution(_) => "solution",
            SlowField::Summary(_) => "summary",
            SlowField::Promise(_) => "promise",
        }
    }
}

/// Every slow field received so far: what the store keeps and `dmn replay` prints.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SlowFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deeper: Option<Vec<Deeper>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub followups: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tradeoffs: Option<Vec<Tradeoff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<Option<Example>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_context: Option<Vec<ResumeContext>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution: Option<Example>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promise: Option<String>,
}

impl SlowFields {
    /// Record one published field.
    pub fn apply(&mut self, field: &SlowField) {
        match field.clone() {
            SlowField::Deeper(v) => self.deeper = Some(v),
            SlowField::Followups(v) => self.followups = Some(v),
            SlowField::Tradeoffs(v) => self.tradeoffs = Some(v),
            SlowField::Example(v) => self.example = Some(v),
            SlowField::Mention(v) => self.mention = Some(v),
            SlowField::ResumeContext(v) => self.resume_context = Some(v),
            SlowField::Solution(v) => self.solution = Some(v),
            SlowField::Summary(v) => self.summary = Some(v),
            SlowField::Promise(v) => self.promise = Some(v),
        }
    }

    /// The prompt-contract JSON of what arrived (absent fields left out).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }

    pub fn is_empty(&self) -> bool {
        *self == SlowFields::default()
    }
}

/// How the slow phase ended when it ran to the end of its reply.
#[derive(Debug, Clone, PartialEq)]
pub struct SlowOutcome {
    pub fields: SlowFields,
    pub flags: Vec<Flag>,
    /// Sections whose field never arrived (the reply closed without them).
    pub missing: Vec<&'static str>,
}

/// The six section fields, in schema order.
const SECTIONS: [&str; 6] = [
    "deeper",
    "followups",
    "tradeoffs",
    "example",
    "mention",
    "resume_context",
];

/// Turns [`JsonEvent`]s from the streamed reply into limited [`SlowField`]s. Pure and
/// synchronous; [`run_slow_phase`] drives it.
#[derive(Debug)]
pub struct SlowAssembler {
    wants_solution: bool,
    wants_summary: bool,
    chunk_files: Vec<String>,
    fields: SlowFields,
    flags: Vec<Flag>,
    seen: Vec<&'static str>,
}

impl SlowAssembler {
    pub fn new(req: &SlowRequest) -> Self {
        Self {
            wants_solution: req.wants_solution(),
            wants_summary: req.wants_summary(),
            chunk_files: req.chunk_files.clone(),
            fields: SlowFields::default(),
            flags: Vec::new(),
            seen: Vec::new(),
        }
    }

    /// A field whose value did not parse is published empty and flagged, so its section is
    /// decided instead of loading for ever.
    fn parse<T: serde::de::DeserializeOwned + Default>(
        &mut self,
        field: &str,
        value: serde_json::Value,
    ) -> T {
        serde_json::from_value(value).unwrap_or_else(|_| {
            self.flags.push(Flag::new(field, FlagKind::Rejected));
            T::default()
        })
    }

    /// Feed one completed JSON value; returns the section it completes, if any.
    pub fn push(&mut self, ev: JsonEvent) -> Option<SlowField> {
        let JsonEvent::Field { key, value } = ev else {
            // Sections are published whole; array items arriving early are not used.
            return None;
        };
        let flags = &mut Vec::new();
        let field = match key.as_str() {
            "deeper" => {
                let mut v: Vec<Deeper> = self.parse("deeper", value);
                limits::limit_deeper(&mut v, flags);
                SlowField::Deeper(v)
            }
            "followups" => {
                let mut v: Vec<String> = self.parse("followups", value);
                limits::limit_items(
                    "followups",
                    &mut v,
                    limits::LIST_MAX,
                    limits::SLOW_ITEM_CHARS,
                    flags,
                );
                SlowField::Followups(v)
            }
            "tradeoffs" => {
                let mut v: Vec<Tradeoff> = self.parse("tradeoffs", value);
                limits::limit_tradeoffs(&mut v, flags);
                SlowField::Tradeoffs(v)
            }
            "example" => {
                let v: Example = self.parse("example", value);
                SlowField::Example(limits::limit_code(
                    "example.code",
                    v,
                    limits::EXAMPLE_LINES,
                    flags,
                ))
            }
            "mention" => {
                let mut v: Vec<String> = self.parse("mention", value);
                limits::limit_items(
                    "mention",
                    &mut v,
                    limits::LIST_MAX,
                    limits::SLOW_ITEM_CHARS,
                    flags,
                );
                SlowField::Mention(v)
            }
            "resume_context" => {
                let mut v: Vec<ResumeContext> = self.parse("resume_context", value);
                limits::limit_citations(&mut v, &self.chunk_files, flags);
                SlowField::ResumeContext(v)
            }
            "solution" => {
                if !self.wants_solution {
                    // Hints mode (or not a coding question): complete code is never shown.
                    self.flags.push(Flag::new("solution", FlagKind::Rejected));
                    return None;
                }
                let v: Example = self.parse("solution", value);
                let code = limits::limit_code("solution.code", v, limits::SOLUTION_LINES, flags)?;
                SlowField::Solution(code)
            }
            "summary" | "promise" if !self.wants_summary => {
                // Nothing was answered yet: a summary here would be made up.
                self.flags.push(Flag::new(key.clone(), FlagKind::Rejected));
                return None;
            }
            "summary" => {
                let v: String = self.parse("summary", value);
                SlowField::Summary(limits::limit_line(
                    "summary",
                    &v,
                    limits::SUMMARY_CHARS,
                    flags,
                ))
            }
            "promise" => {
                let v: String = self.parse("promise", value);
                SlowField::Promise(limits::limit_line(
                    "promise",
                    &v,
                    limits::PROMISE_CHARS,
                    flags,
                ))
            }
            _ => return None,
        };
        self.flags.append(flags);
        if let Some(name) = SECTIONS.iter().find(|s| **s == field.name()) {
            self.seen.push(name);
        }
        self.fields.apply(&field);
        Some(field)
    }

    /// The outcome once the reply has closed.
    pub fn finish(self) -> SlowOutcome {
        let missing = SECTIONS
            .iter()
            .copied()
            .filter(|s| !self.seen.contains(s))
            .collect();
        SlowOutcome {
            fields: self.fields,
            flags: self.flags,
            missing,
        }
    }
}

/// Run the slow phase for `req` on `provider`, handing each [`SlowField`] to `on_field` the
/// moment it is known. `Err` for a timeout, a cancel or a provider error; whatever was handed out
/// before stays valid (the caller keeps it on screen).
pub async fn run_slow_phase<P, F>(
    provider: &P,
    req: &SlowRequest,
    cancel: CancellationToken,
    mut on_field: F,
) -> Result<SlowOutcome, LlmError>
where
    P: Provider + ?Sized,
    F: FnMut(SlowField),
{
    let mut stream = provider.chat(slow_chat_request(req), cancel).await?;
    let mut parser = IncrementalObject::new();
    let mut asm = SlowAssembler::new(req);
    while let Some(item) = stream.next().await {
        let delta = item?;
        for ev in parser.feed(&delta.text) {
            if let Some(field) = asm.push(ev) {
                on_field(field);
            }
        }
        if parser.is_closed() || delta.done {
            break;
        }
    }
    // Dropping the stream closes the connection, so the model stops generating.
    drop(stream);
    let outcome = asm.finish();
    tracing::debug!(
        fields = 6 - outcome.missing.len(),
        flags = outcome.flags.len(),
        "slow phase done"
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{EXAMPLE_LINES, SLOW_ITEM_CHARS, SOLUTION_LINES};
    use crate::mock::MockProvider;
    use serde_json::json;
    use std::time::Instant;

    fn reply() -> serde_json::Value {
        json!({
            "deeper": [
                {"label": "Algorithm", "text": "A token bucket per key refills at the rate limit. Bursts up to the bucket size pass."},
                {"label": "State", "text": "Counters live in Redis, sharded by key."}
            ],
            "followups": ["What happens when Redis is down?", "How do you handle clock skew?"],
            "tradeoffs": [{"option": "Central Redis", "pro": "exact counts", "con": "one hop, a SPOF"}],
            "example": {"language": "text", "code": "100 req/s per key, bucket 200"},
            "mention": ["Fail open, and alert."],
            "resume_context": [
                {"source": "resume.md", "text": "Built the rate limiter for the payments API."},
                {"source": "made-up.pdf", "text": "Led a team of 40."}
            ]
        })
    }

    fn request(kind: Kind) -> SlowRequest {
        let mut req = SlowRequest::new("How would you design a rate limiter?", kind);
        req.focus = "consistency, failure handling".into();
        req.approach = vec!["Clarify scope.".into(), "Token bucket per key.".into()];
        req.context.candidate = "So how would you design a rate limiter?".into();
        req.context.chunks = vec!["resume (resume.md): Built the rate limiter.".into()];
        req.chunk_files = vec!["resume.md".into()];
        req
    }

    async fn run(
        mock: &MockProvider,
        req: &SlowRequest,
    ) -> (Vec<(Instant, SlowField)>, Result<SlowOutcome, LlmError>) {
        let mut events = Vec::new();
        let outcome = run_slow_phase(mock, req, CancellationToken::new(), |f| {
            events.push((Instant::now(), f));
        })
        .await;
        (events, outcome)
    }

    #[tokio::test]
    async fn sections_are_published_in_order_as_their_fields_complete() {
        let mock = MockProvider::json(&reply());
        let (events, outcome) = run(&mock, &request(Kind::SystemDesign)).await;
        let names: Vec<&str> = events.iter().map(|(_, f)| f.name()).collect();
        assert_eq!(
            names,
            [
                "deeper",
                "followups",
                "tradeoffs",
                "example",
                "mention",
                "resume_context"
            ]
        );
        let outcome = outcome.unwrap();
        assert!(outcome.missing.is_empty());
        // The made-up source is dropped; only what the request carried can be cited.
        let SlowField::ResumeContext(cited) = &events[5].1 else {
            panic!("{:?}", events[5]);
        };
        assert_eq!(cited.len(), 1);
        assert_eq!(cited[0].source, "resume.md");
        assert!(
            outcome
                .flags
                .iter()
                .any(|f| f.field == "resume_context[1]" && f.kind == FlagKind::Rejected)
        );
        assert!(outcome.fields.solution.is_none());
    }

    fn with_summary(mut v: serde_json::Value) -> serde_json::Value {
        v["summary"] = json!("two sum —\n covered the hash map, one pass, O(n)");
        v["promise"] = json!("the sorted-input variant");
        v
    }

    #[tokio::test]
    async fn a_previous_answer_is_summarised_after_the_sections() {
        let mock = MockProvider::json(&with_summary(reply()));
        let mut req = request(Kind::SystemDesign);
        req.previous = Some(PreviousAnswer {
            question: "Find two numbers that add up to a target.".into(),
            answer: "I'd use a hash map in one pass. I'll come back to the sorted-input variant."
                .into(),
        });
        let (events, outcome) = run(&mock, &req).await;
        let names: Vec<&str> = events.iter().map(|(_, f)| f.name()).collect();
        assert_eq!(names[6..], ["summary", "promise"], "{names:?}");
        assert_eq!(
            events[6].1,
            SlowField::Summary("two sum — covered the hash map, one pass, O(n)".into()),
            "one line"
        );
        assert_eq!(
            events[7].1,
            SlowField::Promise("the sorted-input variant".into())
        );
        let outcome = outcome.unwrap();
        assert!(outcome.missing.is_empty(), "not sections: never missing");
        assert_eq!(
            outcome.fields.promise.as_deref(),
            Some("the sorted-input variant")
        );

        let sent = &mock.requests()[0];
        assert_eq!(sent.schema, Some(slow_schema(false, true)));
        let user = &sent.messages[1].content;
        assert!(
            user.contains("## Previous question and the candidate's answer"),
            "{user}"
        );
        assert!(
            user.contains("Candidate said: I'd use a hash map"),
            "{user}"
        );
        assert!(
            user.contains("promise:"),
            "the summary instructions ride along"
        );
    }

    #[tokio::test]
    async fn with_nothing_answered_yet_no_summary_is_asked_for_or_kept() {
        let mock = MockProvider::json(&with_summary(reply()));
        let mut req = request(Kind::SystemDesign);
        // A previous question the candidate said nothing to counts as none.
        req.previous = Some(PreviousAnswer {
            question: "Q".into(),
            answer: "  ".into(),
        });
        let (events, outcome) = run(&mock, &req).await;
        assert!(
            events
                .iter()
                .all(|(_, f)| !matches!(f, SlowField::Summary(_) | SlowField::Promise(_))),
            "{events:?}"
        );
        let sent = &mock.requests()[0];
        assert_eq!(sent.schema, Some(slow_schema(false, false)));
        assert!(!sent.messages[1].content.contains("Previous question"));
        let flags = outcome.unwrap().flags;
        assert!(
            flags
                .iter()
                .any(|f| f.field == "summary" && f.kind == FlagKind::Rejected),
            "{flags:?}"
        );
    }

    #[tokio::test]
    async fn a_question_from_the_you_lane_keeps_its_rules_and_header_in_the_slow_request() {
        // #90: the asker rides in `context`; the caller (`dmn-cli`'s `SlowJob`) sets it.
        let mock = MockProvider::json(&reply());
        let mut req = request(Kind::SystemDesign);
        req.context.asker = prompt::Asker::You;
        let _ = run(&mock, &req).await;
        let sent = &mock.requests()[0];
        assert_eq!(
            sent.messages[0].content,
            prompt::system_prompt(None, prompt::Asker::You)
        );
        assert_ne!(
            sent.messages[0].content,
            prompt::system_prompt(None, prompt::Asker::Interviewer)
        );
        let user = &sent.messages[1].content;
        assert!(user.contains("## You just said\n"), "{user}");
        assert!(!user.contains("## Interviewer just said"), "{user}");
    }

    #[tokio::test]
    async fn the_request_shares_the_fast_prompt_and_adds_the_question_on_screen() {
        let mock = MockProvider::json(&reply());
        let req = request(Kind::SystemDesign);
        let _ = run(&mock, &req).await;
        let sent = &mock.requests()[0];
        assert_eq!(sent.timeout, SLOW_PHASE_TIMEOUT);
        assert_eq!(sent.temperature, 0.0);
        assert_eq!(sent.schema, Some(slow_schema(false, false)));
        assert_eq!(
            sent.messages[0].content,
            prompt::system_prompt(None, req.context.asker),
            "the fast phase's system prompt, word for word"
        );
        let user = &sent.messages[1].content;
        let fast_user = prompt::user_message(&req.context);
        assert!(
            user.starts_with(fast_user.as_str()),
            "the user message starts as the fast phase's"
        );
        for want in [
            "## Question on screen (system_design)",
            "Focus: consistency, failure handling",
            "1. Clarify scope.",
            "resume_context",
            "System design:",
        ] {
            assert!(user.contains(want), "missing {want:?}:\n{user}");
        }
    }

    #[tokio::test]
    async fn the_slow_request_carries_the_date_the_rule_and_a_flag_from_the_fast_phase() {
        let mock = MockProvider::json(&reply());
        let mut req = request(Kind::Knowledge);
        req.context.today = "2026-10-05".into();
        let _ = run(&mock, &req).await;
        let plain = mock.requests()[0].messages[1].content.clone();
        assert!(plain.starts_with("## Today\n2026-10-05\n\n"), "{plain}");
        assert!(plain.contains("- Current facts hold here too"), "{plain}");
        assert!(!plain.contains("Flagged as depending"), "{plain}");
        assert!(
            mock.requests()[0].messages[0]
                .content
                .contains("- Current facts:"),
            "the fast phase's rule rides in the shared system prompt"
        );

        let mock = MockProvider::json(&reply());
        req.lookup = "latest stable Go release".into();
        let _ = run(&mock, &req).await;
        let flagged = &mock.requests()[0].messages[1].content;
        let note = flagged
            .find("Flagged as depending on what is current (to look up: latest stable Go release)")
            .expect(flagged);
        assert!(note > flagged.find("Approach already shown:").unwrap());
        assert!(note < flagged.find("## Now fill").unwrap(), "{flagged}");
    }

    #[tokio::test]
    async fn lookup_results_reach_the_slow_phase_and_replace_the_out_of_date_note() {
        use crate::lookup::{FreshFact, FreshFacts};
        let mock = MockProvider::json(&reply());
        let mut req = request(Kind::Knowledge);
        req.lookup = "latest stable Go release".into();
        req.context.facts = Some(FreshFacts {
            query: "latest stable Go release".into(),
            facts: vec![FreshFact {
                title: "All releases".into(),
                snippet: "go1.25.1 (released 2025-09-03)".into(),
                domain: "go.dev".into(),
            }],
        });
        let _ = run(&mock, &req).await;
        let user = &mock.requests()[0].messages[1].content;
        assert!(user.contains("## Fresh facts\n"), "{user}");
        assert!(user.contains("[1] All releases (go.dev)"), "{user}");
        assert!(
            user.contains("Checked on the web (search: latest stable Go release)"),
            "{user}"
        );
        assert!(!user.contains("Flagged as depending"), "{user}");
    }

    #[tokio::test]
    async fn hints_mode_never_carries_a_solution_whatever_the_model_sends() {
        let mut with_code = reply();
        with_code["solution"] =
            json!({"language": "python", "code": "def two_sum(a, t):\n    return []"});
        let mock = MockProvider::json(&with_code);
        let req = request(Kind::Coding);
        let (events, outcome) = run(&mock, &req).await;
        assert!(!events.iter().any(|(_, f)| f.name() == "solution"));
        let outcome = outcome.unwrap();
        assert!(outcome.fields.solution.is_none());
        assert!(
            outcome
                .flags
                .iter()
                .any(|f| f.field == "solution" && f.kind == FlagKind::Rejected)
        );
        let sent = &mock.requests()[0];
        assert_eq!(
            sent.schema,
            Some(slow_schema(false, false)),
            "not even asked for"
        );
        assert!(
            sent.messages[1]
                .content
                .contains("No complete code anywhere")
        );
    }

    #[tokio::test]
    async fn full_mode_gives_a_coding_question_a_solution_of_at_most_40_lines() {
        let code: Vec<String> = (0..55).map(|i| format!("    step_{i}()")).collect();
        let mut with_code = reply();
        with_code["solution"] =
            json!({"language": "python", "code": format!("def solve():\n{}", code.join("\n"))});
        let mock = MockProvider::json(&with_code);
        let mut req = request(Kind::Coding);
        req.coding = CodingMode::Full;
        let (events, outcome) = run(&mock, &req).await;
        let Some((_, SlowField::Solution(solution))) =
            events.iter().find(|(_, f)| f.name() == "solution")
        else {
            panic!("no solution: {events:?}");
        };
        assert_eq!(solution.code.lines().count(), SOLUTION_LINES);
        assert_eq!(solution.language, "python");
        assert_eq!(mock.requests()[0].schema, Some(slow_schema(true, false)));
        assert!(outcome.unwrap().fields.solution.is_some());
    }

    #[tokio::test]
    async fn over_long_output_is_truncated_and_flagged_never_a_wall() {
        let wall =
            "Shard the counters by key so that no single node sees every request. ".repeat(8);
        let lines: Vec<String> = (0..30).map(|i| format!("row {i}")).collect();
        let long = json!({
            "deeper": (0..7).map(|i| json!({"label": format!("L{i}"), "text": wall})).collect::<Vec<_>>(),
            "followups": vec![wall.clone(); 8],
            "tradeoffs": [],
            "example": {"language": "text", "code": lines.join("\n")},
            "mention": [wall],
            "resume_context": []
        });
        let mock = MockProvider::json(&long);
        let (events, outcome) = run(&mock, &request(Kind::Knowledge)).await;
        for (_, f) in &events {
            match f {
                SlowField::Deeper(d) => {
                    assert_eq!(d.len(), limits::DEEPER_MAX);
                    assert!(
                        d.iter()
                            .all(|e| e.text.chars().count() <= limits::DEEPER_CHARS)
                    );
                    assert!(d.iter().all(|e| e.text.ends_with('.')), "cut at a sentence");
                }
                SlowField::Followups(v) | SlowField::Mention(v) => {
                    assert!(v.len() <= limits::LIST_MAX);
                    assert!(v.iter().all(|i| i.chars().count() <= SLOW_ITEM_CHARS));
                }
                SlowField::Example(Some(e)) => {
                    assert_eq!(e.code.lines().count(), EXAMPLE_LINES);
                }
                _ => {}
            }
        }
        let flags: Vec<String> = outcome
            .unwrap()
            .flags
            .into_iter()
            .map(|f| f.field)
            .collect();
        for want in [
            "deeper",
            "deeper[0]",
            "followups",
            "followups[0]",
            "example.code",
            "mention[0]",
        ] {
            assert!(
                flags.iter().any(|f| f == want),
                "missing flag {want}: {flags:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_timeout_keeps_what_was_published_and_returns_the_error() {
        // 7 characters every 30 ms: the deeper field (~250 characters) completes in about a
        // second; the deadline passes long before the rest.
        let mock = MockProvider::json(&reply()).with_delay(Duration::from_millis(30));
        let mut req = request(Kind::SystemDesign);
        req.timeout = Duration::from_millis(1_500);
        let (events, outcome) = run(&mock, &req).await;
        assert!(matches!(outcome, Err(LlmError::Timeout(_))), "{outcome:?}");
        assert!(
            events.iter().any(|(_, f)| f.name() == "deeper"),
            "the first section arrived before the deadline: {events:?}"
        );
        assert!(!events.iter().any(|(_, f)| f.name() == "resume_context"));
    }

    #[tokio::test]
    async fn a_reply_that_closes_early_reports_the_missing_sections() {
        let mock = MockProvider::json(&json!({"deeper": [], "followups": []}));
        let (_, outcome) = run(&mock, &request(Kind::Knowledge)).await;
        assert_eq!(
            outcome.unwrap().missing,
            ["tradeoffs", "example", "mention", "resume_context"]
        );
    }

    #[test]
    fn fields_serialise_as_the_prompt_contract_leaving_out_what_never_arrived() {
        let mut f = SlowFields::default();
        assert!(f.is_empty());
        f.apply(&SlowField::Followups(vec!["Why?".into()]));
        f.apply(&SlowField::Example(None));
        assert_eq!(f.to_json(), json!({"followups": ["Why?"], "example": null}));
    }
}
