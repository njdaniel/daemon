//! The fast phase: one streamed request that confirms the candidate is a question, extracts it,
//! and fills the Live Question and the Approach (SPEC § Context and LLM).
//!
//! Fields are published as they complete, not when the whole object has arrived (#12 deviates
//! from SPEC § Threading rules here): [`run_fast_phase`] calls `on_event` with the question as
//! soon as `is_question`, `kind` and `question` are known, then with each approach line as it
//! closes. Nothing is published for `is_question=false` or `kind=small_talk`; the candidate is
//! logged with its detector score instead, and the stream is dropped early. A `clarification`
//! while a question is on screen is published as [`FastEvent::Clarification`] so it attaches
//! under the current question instead of replacing it. Every published field has already been
//! through `limits.rs`.
//!
//! On timeout or cancellation the events already published stand (the UI keeps them and shows
//! a "slow" tag); the error is returned.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::limits::{self, Flag, FlagKind};
use crate::prompt::{FastRequest, fast_messages};
use crate::provider::{ChatRequest, LlmError, Provider};
use crate::schema::{Kind, Star, Suggestion, fast_schema_with};
use crate::stream_json::{IncrementalObject, JsonEvent};

/// SPEC § Providers: fast phase 4 s.
pub const FAST_PHASE_TIMEOUT: Duration = Duration::from_secs(4);
/// SPEC § AI Assistance: Redo regenerates with `temperature=0.7`.
pub const REGENERATE_TEMPERATURE: f32 = 0.7;

/// The request the fast phase sends: messages, the fast schema, the 4 s deadline.
pub fn fast_chat_request(req: &FastRequest) -> ChatRequest {
    let temperature = if req.regenerate {
        REGENERATE_TEMPERATURE
    } else {
        0.0
    };
    ChatRequest::new(fast_messages(req))
        .with_schema(fast_schema_with(req.story.is_some()))
        .with_temperature(temperature)
        .with_timeout(FAST_PHASE_TIMEOUT)
}

/// One piece of the fast phase, published as soon as it is known.
#[derive(Debug, Clone, PartialEq)]
pub enum FastEvent {
    /// A new question for the Live Question area. Always the first event of a shown reply.
    Question { question: String, kind: Kind },
    /// A refinement of the question on screen: a sub-line under it, never a replacement.
    Clarification { text: String },
    /// The focus line; not sent when the model returned none.
    Focus { text: String },
    /// Approach line `index` (0-based, contiguous), already cut to 90 characters.
    ApproachLine { index: usize, text: String },
    /// STAR cards for a behavioral question.
    Star(Star),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    NotAQuestion,
    SmallTalk,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FastOutcome {
    /// Something was published. `suggestion` holds exactly what was published; `clarification`
    /// says whether it attached to the current question; `flags` lists every limit applied.
    Shown {
        suggestion: Box<Suggestion>,
        clarification: bool,
        flags: Vec<Flag>,
    },
    /// Nothing changed on screen.
    Ignored { reason: IgnoreReason, score: i32 },
}

/// Turns [`JsonEvent`]s from the streamed reply into [`FastEvent`]s, holding back everything
/// until the gate fields (`is_question`, `kind`, `question`) say the reply is to be shown.
/// Pure and synchronous; [`run_fast_phase`] drives it.
#[derive(Debug)]
pub struct FastAssembler {
    has_current: bool,
    is_question: Option<bool>,
    kind: Option<Kind>,
    question: Option<String>,
    started: bool,
    held: Vec<FastEvent>,
    ignored: Option<IgnoreReason>,
    focus: String,
    approach: Vec<String>,
    /// Approach items the model sent, including any past the limit.
    approach_items: usize,
    star: Option<Star>,
    flags: Vec<Flag>,
    /// What the detector proposed and what is on screen, for [`FastAssembler::echoed`].
    guard: Option<(String, String)>,
    echoed: bool,
    /// The matched story's label (#58), put on `star.story` in place of whatever the model said.
    story: Option<String>,
}

impl FastAssembler {
    /// `has_current`: a question is on screen, so a clarification attaches to it.
    pub fn new(has_current: bool) -> Self {
        Self {
            has_current,
            is_question: None,
            kind: None,
            question: None,
            started: false,
            held: Vec::new(),
            ignored: None,
            focus: String::new(),
            approach: Vec::new(),
            approach_items: 0,
            star: None,
            flags: Vec::new(),
            guard: None,
            echoed: false,
            story: None,
        }
    }

    /// Name `story` on the STAR cards (#58). The label is the prompt's matched story, set here
    /// rather than asked of the model, so the header can only ever name the story the cards were
    /// built from; with no story matched, the cards name none.
    pub fn with_story(mut self, story: Option<String>) -> Self {
        self.story = story;
        self
    }

    /// Hold back a reply whose `question` restates the question already on screen instead of
    /// what the Interviewer just said (`candidate`). Live session 5: "Design a ride-sharing
    /// matching system" came back as the Slack question from the `## Current question` section,
    /// and its Approach answered Slack. A clarification is exempt: it is about the current
    /// question by definition.
    pub fn with_echo_guard(mut self, candidate: &str, current: &str) -> Self {
        self.guard = Some((candidate.to_owned(), current.to_owned()));
        self
    }

    pub fn ignored(&self) -> Option<IgnoreReason> {
        self.ignored
    }

    /// The reply restated the current question instead of the new one; nothing was released.
    pub fn echoed(&self) -> bool {
        self.echoed
    }

    fn clarifies(&self) -> bool {
        self.has_current && self.kind == Some(Kind::Clarification)
    }

    /// Feed one completed JSON value; returns the events it releases, in order.
    pub fn push(&mut self, ev: JsonEvent) -> Vec<FastEvent> {
        if self.ignored.is_some() || self.echoed {
            return Vec::new();
        }
        let mut released = Vec::new();
        match ev {
            JsonEvent::Field { key, value } => match key.as_str() {
                "is_question" => self.is_question = value.as_bool(),
                "kind" => self.kind = serde_json::from_value(value).ok(),
                "question" => {
                    self.question = value.as_str().map(|q| q.trim().to_owned());
                }
                "focus" => {
                    let mut focus = value.as_str().unwrap_or_default().trim().to_owned();
                    limits::limit_text("focus", &mut focus, limits::FOCUS_CHARS, &mut self.flags);
                    if !focus.is_empty() {
                        self.focus = focus.clone();
                        released.push(FastEvent::Focus { text: focus });
                    }
                }
                "star" => {
                    if let Ok(mut star) = serde_json::from_value::<Star>(value) {
                        limits::limit_star(&mut star, &mut self.flags);
                        star.story = self.story.clone();
                        self.star = Some(star.clone());
                        released.push(FastEvent::Star(star));
                    }
                }
                _ => {}
            },
            JsonEvent::Item { key, index, value } if key == "approach" => {
                let raw = value.as_str().unwrap_or_default();
                self.approach_items = self.approach_items.max(index + 1);
                if index < limits::APPROACH_MAX {
                    let n = self.approach.len();
                    let line = limits::approach_line(n, raw, &mut self.flags);
                    if !line.is_empty() {
                        self.approach.push(line.clone());
                        released.push(FastEvent::ApproachLine {
                            index: n,
                            text: line,
                        });
                    }
                }
            }
            JsonEvent::Item { .. } => {}
        }

        if self.is_question == Some(false) {
            self.ignored = Some(IgnoreReason::NotAQuestion);
        } else if self.kind == Some(Kind::SmallTalk) {
            self.ignored = Some(IgnoreReason::SmallTalk);
        }
        if self.ignored.is_some() {
            self.held.clear();
            return Vec::new();
        }

        if self.started {
            return released;
        }
        self.held.extend(released);
        match (self.is_question, self.kind, &self.question) {
            (Some(true), Some(kind), Some(q)) => {
                if kind != Kind::Clarification
                    && let Some((candidate, current)) = &self.guard
                    && echoes(q, candidate, current)
                {
                    self.echoed = true;
                    self.held.clear();
                    return Vec::new();
                }
                self.started = true;
                let head = if self.clarifies() {
                    FastEvent::Clarification { text: q.clone() }
                } else {
                    FastEvent::Question {
                        question: q.clone(),
                        kind,
                    }
                };
                let mut out = vec![head];
                out.append(&mut self.held);
                out
            }
            _ => Vec::new(),
        }
    }

    /// The outcome once the stream has ended. `Err` carries why the reply was unusable.
    pub fn finish(mut self, score: i32) -> Result<FastOutcome, String> {
        if let Some(reason) = self.ignored {
            return Ok(FastOutcome::Ignored { reason, score });
        }
        if !self.started {
            return Err(format!(
                "reply ended without is_question/kind/question (is_question={:?}, kind={:?}, question={})",
                self.is_question,
                self.kind,
                self.question.is_some()
            ));
        }
        if self.approach_items > limits::APPROACH_MAX {
            self.flags.push(Flag::new(
                "approach",
                FlagKind::Dropped {
                    from: self.approach_items,
                    to: limits::APPROACH_MAX,
                },
            ));
        }
        limits::check_approach_min(self.kind, self.approach.len(), &mut self.flags);
        let clarification = self.clarifies();
        Ok(FastOutcome::Shown {
            suggestion: Box::new(Suggestion {
                question: self.question.unwrap_or_default(),
                kind: self.kind,
                focus: self.focus,
                approach: self.approach,
                star: self.star,
                ..Suggestion::default()
            }),
            clarification,
            flags: self.flags,
        })
    }
}

/// Content words of a question: lowercase, alphanumeric, longer than two characters.
fn content_words(s: &str) -> std::collections::HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_lowercase)
        .collect()
}

/// Share of the smaller word set found in the other (0.0–1.0).
fn overlap(a: &str, b: &str) -> f64 {
    let (a, b) = (content_words(a), content_words(b));
    let small = a.len().min(b.len());
    if small == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / small as f64
}

/// `question` is the current question again and not what the Interviewer just said.
fn echoes(question: &str, candidate: &str, current: &str) -> bool {
    overlap(question, current) >= 0.6 && overlap(question, candidate) < 0.3
}

/// Run the fast phase for `req` on `provider`, publishing each [`FastEvent`] through `on_event`
/// the moment it is known. See the module doc for the gating and error rules.
pub async fn run_fast_phase<P, F>(
    provider: &P,
    req: &FastRequest,
    cancel: CancellationToken,
    mut on_event: F,
) -> Result<FastOutcome, LlmError>
where
    P: Provider + ?Sized,
    F: FnMut(FastEvent),
{
    let mut asm = attempt(provider, req, cancel.clone(), &mut on_event, true).await?;
    if asm.echoed() {
        // Nothing was shown. Ask again without the `## Current question` section the model
        // copied; the transcript still carries the context.
        tracing::warn!(
            candidate = %req.candidate,
            "fast phase: reply restated the current question; retrying without it"
        );
        let mut again = req.clone();
        again.current = None;
        asm = attempt(provider, &again, cancel, &mut on_event, false).await?;
    }
    let outcome = asm
        .finish(req.score)
        .map_err(|detail| LlmError::MalformedStream {
            provider: provider.name(),
            detail,
        })?;
    match &outcome {
        FastOutcome::Ignored { reason, score } => tracing::info!(
            candidate = %req.candidate,
            score,
            reason = ?reason,
            "fast phase: candidate not shown"
        ),
        FastOutcome::Shown {
            suggestion,
            clarification,
            flags,
        } => tracing::debug!(
            kind = ?suggestion.kind,
            clarification,
            approach = suggestion.approach.len(),
            flags = flags.len(),
            "fast phase: shown"
        ),
    }
    Ok(outcome)
}

/// One streamed request, fed through a [`FastAssembler`] until it is decided.
async fn attempt<P, F>(
    provider: &P,
    req: &FastRequest,
    cancel: CancellationToken,
    on_event: &mut F,
    guard: bool,
) -> Result<FastAssembler, LlmError>
where
    P: Provider + ?Sized,
    F: FnMut(FastEvent),
{
    let mut stream = provider.chat(fast_chat_request(req), cancel).await?;
    let mut parser = IncrementalObject::new();
    let mut asm = FastAssembler::new(req.current.is_some())
        .with_story(req.story.as_ref().map(|s| s.label.clone()));
    if guard && let Some(cur) = &req.current {
        asm = asm.with_echo_guard(&req.candidate, &cur.question);
    }
    while let Some(item) = stream.next().await {
        let delta = item?;
        for ev in parser.feed(&delta.text) {
            for fe in asm.push(ev) {
                on_event(fe);
            }
        }
        if asm.ignored().is_some() || asm.echoed() || parser.is_closed() || delta.done {
            break;
        }
    }
    // Dropping the stream closes the connection, so an ignored candidate stops generating.
    drop(stream);
    Ok(asm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockProvider, Step};
    use crate::prompt::CurrentQuestion;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn rate_limiter_reply() -> serde_json::Value {
        json!({
            "is_question": true,
            "kind": "system_design",
            "question": "How would you design a rate limiter for a globally distributed API?",
            "focus": "architecture, consistency tradeoffs, Redis, failure handling",
            "approach": [
                "Clarify requirements and rate-limit scope.",
                "Pick an algorithm: token bucket per key.",
                "Store counters in Redis, sharded by key.",
                "Trade exactness for latency with local caches.",
                "Fail open when Redis is unreachable."
            ]
        })
    }

    async fn run(
        mock: &MockProvider,
        req: &FastRequest,
    ) -> (
        Vec<(Instant, FastEvent)>,
        Result<FastOutcome, LlmError>,
        Instant,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let outcome = run_fast_phase(mock, req, CancellationToken::new(), move |e| {
            sink.lock().unwrap().push((Instant::now(), e));
        })
        .await;
        let end = Instant::now();
        let events = events.lock().unwrap().clone();
        (events, outcome, end)
    }

    #[tokio::test]
    async fn question_is_published_before_the_approach_finishes_streaming() {
        let mock = MockProvider::json(&rate_limiter_reply()).with_delay(Duration::from_millis(3));
        let req = FastRequest::new("How would you design a rate limiter?", 5);
        let (events, outcome, end) = run(&mock, &req).await;

        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| match e {
                FastEvent::Question { .. } => "question",
                FastEvent::Clarification { .. } => "clarification",
                FastEvent::Focus { .. } => "focus",
                FastEvent::ApproachLine { .. } => "line",
                FastEvent::Star(_) => "star",
            })
            .collect();
        assert_eq!(
            kinds,
            ["question", "focus", "line", "line", "line", "line", "line"]
        );
        // Lines arrive one by one, in order, each at its own moment.
        let lines: Vec<(Instant, usize)> = events
            .iter()
            .filter_map(|(t, e)| match e {
                FastEvent::ApproachLine { index, .. } => Some((*t, *index)),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines.iter().map(|l| l.1).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(lines.windows(2).all(|w| w[0].0 < w[1].0));
        let q_at = events[0].0;
        assert!(q_at < lines[0].0, "question before the first line");
        assert!(
            end.duration_since(q_at) > Duration::from_millis(30),
            "question must land well before the stream ends, not with it"
        );

        let Ok(FastOutcome::Shown {
            suggestion,
            clarification,
            flags,
        }) = outcome
        else {
            panic!("{outcome:?}");
        };
        assert!(!clarification);
        assert!(flags.is_empty(), "{flags:?}");
        assert_eq!(suggestion.kind, Some(Kind::SystemDesign));
        assert_eq!(suggestion.approach.len(), 5);
        assert_eq!(mock.requests()[0].timeout, FAST_PHASE_TIMEOUT);
        assert_eq!(
            mock.requests()[0].schema,
            Some(crate::schema::fast_schema())
        );
    }

    #[tokio::test]
    async fn over_long_output_is_truncated_and_flagged() {
        let wall = "Consider the tradeoffs between consistency and availability in every region. "
            .repeat(6);
        let reply = json!({
            "is_question": true,
            "kind": "knowledge",
            "question": "What is CAP?",
            "focus": wall,
            "approach": [wall, "Two.", "Three.", "Four.", "Five.", "Six.", "Seven."]
        });
        let mock = MockProvider::json(&reply);
        let (events, outcome, _) = run(&mock, &FastRequest::new("What is CAP?", 4)).await;
        for (_, e) in &events {
            match e {
                FastEvent::ApproachLine { text, .. } => {
                    assert!(
                        text.chars().count() <= limits::APPROACH_LINE_CHARS,
                        "{text}"
                    )
                }
                FastEvent::Focus { text } => {
                    assert!(text.chars().count() <= limits::FOCUS_CHARS, "{text}")
                }
                _ => {}
            }
        }
        let lines = events
            .iter()
            .filter(|(_, e)| matches!(e, FastEvent::ApproachLine { .. }))
            .count();
        assert_eq!(lines, limits::APPROACH_MAX);
        let Ok(FastOutcome::Shown {
            flags, suggestion, ..
        }) = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            suggestion.approach[0],
            "Consider the tradeoffs between consistency and availability in every region."
        );
        let fields: Vec<&str> = flags.iter().map(|f| f.field.as_str()).collect();
        assert!(fields.contains(&"focus"), "{fields:?}");
        assert!(fields.contains(&"approach[0]"), "{fields:?}");
        assert!(
            flags
                .iter()
                .any(|f| f.field == "approach" && f.kind == FlagKind::Dropped { from: 7, to: 5 }),
            "{fields:?}"
        );
    }

    #[tokio::test]
    async fn not_a_question_publishes_nothing_and_stops_early() {
        let reply = json!({
            "is_question": false,
            "kind": "knowledge",
            "question": "Right?",
            "focus": "",
            "approach": ["a", "b", "c"]
        });
        let mock = MockProvider::json(&reply);
        let (events, outcome, _) = run(&mock, &FastRequest::new("That's fair, right?", 2)).await;
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            outcome.unwrap(),
            FastOutcome::Ignored {
                reason: IgnoreReason::NotAQuestion,
                score: 2
            }
        );
    }

    #[tokio::test]
    async fn small_talk_publishes_nothing_even_if_marked_a_question() {
        let reply = json!({
            "is_question": true,
            "kind": "small_talk",
            "question": "How was your weekend?",
            "focus": "",
            "approach": ["Say it was good.", "Ask theirs.", "Move on."]
        });
        let mock = MockProvider::json(&reply);
        let (events, outcome, _) = run(&mock, &FastRequest::new("How was your weekend?", 3)).await;
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            outcome.unwrap(),
            FastOutcome::Ignored {
                reason: IgnoreReason::SmallTalk,
                score: 3
            }
        );
    }

    #[tokio::test]
    async fn clarification_attaches_to_the_current_question() {
        let reply = json!({
            "is_question": true,
            "kind": "clarification",
            "question": "What if Redis goes down?",
            "focus": "failure handling",
            "approach": ["Fail open with local limits.", "Alert and shed load.", "Recover counters lazily."]
        });
        let mock = MockProvider::json(&reply);
        let mut req = FastRequest::new("And what if Redis goes down?", 4);
        req.current = Some(CurrentQuestion {
            question: "How would you design a rate limiter?".into(),
            kind: Kind::SystemDesign,
        });
        let (events, outcome, _) = run(&mock, &req).await;
        assert_eq!(
            events[0].1,
            FastEvent::Clarification {
                text: "What if Redis goes down?".into()
            }
        );
        assert!(
            !events
                .iter()
                .any(|(_, e)| matches!(e, FastEvent::Question { .. })),
            "a clarification never replaces the question"
        );
        let Ok(FastOutcome::Shown { clarification, .. }) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(clarification);
    }

    #[tokio::test]
    async fn a_reply_that_restates_the_current_question_is_retried_without_it() {
        // Live session 5: the new line was a ride-sharing question; the model returned the
        // Slack question from `## Current question` and answered that.
        let slack = "How would you design a chat app like Slack for a company with 50,000 people?";
        let echo = json!({
            "is_question": true,
            "kind": "system_design",
            "question": slack,
            "focus": "real-time messaging",
            "approach": ["Use websockets per client.", "Shard channels.", "Fan out on write."]
        });
        let right = json!({
            "is_question": true,
            "kind": "system_design",
            "question": "How would you design a ride-sharing matching system?",
            "focus": "geo indexing, matching latency",
            "approach": ["Index drivers by geohash.", "Match nearest available.", "Lock the driver."]
        });
        let mock = MockProvider::json(&echo).then_json(&right);
        let mut req = FastRequest::new("Design a ride-sharing device matching system.", 5);
        req.current = Some(CurrentQuestion {
            question: slack.into(),
            kind: Kind::SystemDesign,
        });
        let (events, outcome, _) = run(&mock, &req).await;
        assert!(
            !events
                .iter()
                .any(|(_, e)| format!("{e:?}").contains("Slack")
                    || format!("{e:?}").contains("websockets")),
            "nothing from the echoed reply reached the screen: {events:?}"
        );
        assert_eq!(
            events[0].1,
            FastEvent::Question {
                question: "How would you design a ride-sharing matching system?".into(),
                kind: Kind::SystemDesign,
            }
        );
        assert!(
            matches!(outcome, Ok(FastOutcome::Shown { .. })),
            "{outcome:?}"
        );
        let sent = mock.requests();
        assert_eq!(sent.len(), 2, "one retry");
        let retry = &sent[1].messages[1].content;
        assert!(
            !retry.contains("## Current question"),
            "the retry leaves out the section the model copied: {retry}"
        );
    }

    #[test]
    fn the_echo_guard_spares_a_repeated_question_and_a_real_new_one() {
        let slack = "How would you design a chat app like Slack?";
        assert!(echoes(
            slack,
            "Design a ride-sharing matching system.",
            slack
        ));
        assert!(
            !echoes(slack, "How do you design a chat app like slack?", slack),
            "the interviewer asking the same question again is not an echo"
        );
        assert!(!echoes(
            "How would you design a ride-sharing system?",
            "Design a ride sharing device.",
            slack
        ));
    }

    #[tokio::test]
    async fn clarification_with_nothing_on_screen_is_shown_as_a_question() {
        let reply = json!({
            "is_question": true,
            "kind": "clarification",
            "question": "What if Redis goes down?",
            "focus": "",
            "approach": ["a", "b", "c"]
        });
        let mock = MockProvider::json(&reply);
        let (events, _, _) = run(&mock, &FastRequest::new("What if Redis goes down?", 4)).await;
        assert!(matches!(
            &events[0].1,
            FastEvent::Question {
                kind: Kind::Clarification,
                ..
            }
        ));
    }

    #[test]
    fn fields_before_the_gate_are_held_then_released_in_order() {
        let mut asm = FastAssembler::new(false);
        let item = |i: usize, s: &str| JsonEvent::Item {
            key: "approach".into(),
            index: i,
            value: json!(s),
        };
        let field = |k: &str, v: serde_json::Value| JsonEvent::Field {
            key: k.into(),
            value: v,
        };
        assert!(asm.push(item(0, "1. First")).is_empty());
        assert!(asm.push(field("question", json!("Q?"))).is_empty());
        assert!(asm.push(field("is_question", json!(true))).is_empty());
        let out = asm.push(field("kind", json!("coding")));
        assert_eq!(
            out,
            vec![
                FastEvent::Question {
                    question: "Q?".into(),
                    kind: Kind::Coding
                },
                FastEvent::ApproachLine {
                    index: 0,
                    text: "First".into()
                },
            ]
        );
        assert_eq!(
            asm.push(item(1, "Second")),
            vec![FastEvent::ApproachLine {
                index: 1,
                text: "Second".into()
            }]
        );
    }

    #[tokio::test]
    async fn behavioral_reply_publishes_star() {
        let reply = json!({
            "is_question": true,
            "kind": "behavioral",
            "question": "Tell me about a conflict.",
            "focus": "ownership",
            "approach": ["Situation.", "Task.", "Action.", "Result."],
            "star": {"situation": "s", "task": "t", "action": "a", "result": "r"}
        });
        let mock = MockProvider::json(&reply);
        let (events, _, _) = run(&mock, &FastRequest::new("Tell me about a conflict.", 4)).await;
        assert!(matches!(events.last().unwrap().1, FastEvent::Star(_)));
    }

    #[tokio::test]
    async fn the_star_names_the_matched_story_and_never_one_the_model_made_up() {
        let reply = json!({
            "is_question": true,
            "kind": "behavioral",
            "question": "Tell me about a conflict.",
            "focus": "ownership",
            "approach": ["Situation.", "Task.", "Action.", "Result."],
            "star": {"situation": "s", "task": "t", "action": "a", "result": "r",
                     "story": "stories/invented.md"}
        });
        let star_of = |events: &[(Instant, FastEvent)]| {
            events.iter().find_map(|(_, e)| match e {
                FastEvent::Star(s) => Some(s.clone()),
                _ => None,
            })
        };
        let mut req = FastRequest::new("Tell me about a conflict.", 4);
        req.story = Some(crate::prompt::StoryText {
            label: "stories/broker.md".into(),
            text: "We chose a broker.".into(),
        });
        let mock = MockProvider::json(&reply);
        let (events, outcome, _) = run(&mock, &req).await;
        assert_eq!(
            star_of(&events).unwrap().story.as_deref(),
            Some("stories/broker.md")
        );
        assert_eq!(
            mock.requests()[0].schema,
            Some(crate::schema::fast_schema_with(true)),
            "a matched story makes the STAR cards required"
        );
        let Ok(FastOutcome::Shown { suggestion, .. }) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(
            suggestion.star.unwrap().story.as_deref(),
            Some("stories/broker.md")
        );

        let (events, _, _) = run(
            &MockProvider::json(&reply),
            &FastRequest::new("Tell me about a conflict.", 4),
        )
        .await;
        assert_eq!(
            star_of(&events).unwrap().story,
            None,
            "no story matched, none named"
        );
    }

    #[tokio::test]
    async fn truncated_reply_is_malformed() {
        let mock = MockProvider::new(vec![
            Step::Text(r#"{"is_question":true,"#.into()),
            Step::Done,
        ]);
        let (_, outcome, _) = run(&mock, &FastRequest::new("q", 3)).await;
        assert!(
            matches!(outcome, Err(LlmError::MalformedStream { .. })),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn cancel_after_the_question_keeps_what_was_published() {
        // Timeout and cancellation end the stream the same way (`guard`); cancellation is
        // deterministic to trigger, so it stands in for the 4 s deadline here.
        let mock = MockProvider::json(&rate_limiter_reply()).with_delay(Duration::from_millis(2));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let mut events = Vec::new();
        let outcome = run_fast_phase(&mock, &FastRequest::new("q", 3), cancel, |e| {
            if matches!(e, FastEvent::Question { .. }) {
                trigger.cancel();
            }
            events.push(e);
        })
        .await;
        assert!(matches!(outcome, Err(LlmError::Cancelled)), "{outcome:?}");
        assert!(
            matches!(events[0], FastEvent::Question { .. }),
            "{events:?}"
        );
        assert!(events.len() < 7, "stream stopped early: {events:?}");
    }
}
