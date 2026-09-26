//! [`MockProvider`]: replays a scripted stream and records every request. Used by this crate's
//! tests and by later crates (#12 prompts, #13 session, #15 TUI) so nothing needs a live model.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::provider::{ChatRequest, Delta, DeltaStream, LlmError, Provider, guard};

/// One scripted step of a mock reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Text(String),
    Done,
    /// Yields `LlmError::Remote { provider: "mock", message }`.
    Error(String),
    /// Waits this long, then yields an empty delta: a reply that stalls mid-stream, as a slow
    /// phase's does while a new question needs the model (#55).
    Pause(Duration),
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    script: Vec<Step>,
    /// Replies for the second, third, ... request; the last one repeats.
    then: Vec<Vec<Step>>,
    delay: Duration,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl MockProvider {
    /// Replay `script` on every request.
    pub fn new(script: Vec<Step>) -> Self {
        Self {
            script,
            then: Vec::new(),
            delay: Duration::ZERO,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Reply with `value` serialised as JSON, streamed in chunks of a few characters, then `Done`.
    pub fn json(value: &serde_json::Value) -> Self {
        Self::new(json_script(value))
    }

    /// Reply to the next request with `value` instead of repeating the first script, e.g. a
    /// retry that must see a different answer.
    pub fn then_json(mut self, value: &serde_json::Value) -> Self {
        self.then.push(json_script(value));
        self
    }

    /// Reply to the next request with `script`, like [`MockProvider::then_json`].
    pub fn then(mut self, script: Vec<Step>) -> Self {
        self.then.push(script);
        self
    }

    /// Sleep this long before each step, to exercise cancellation and timeouts.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

/// `value` as JSON in chunks of a few characters, then `Done`. Chunks are cut on `char`
/// boundaries, so every delta is valid UTF-8 and the concatenation is byte-exact.
pub fn json_script(value: &serde_json::Value) -> Vec<Step> {
    const CHARS_PER_CHUNK: usize = 7;
    let chars: Vec<char> = value.to_string().chars().collect();
    let mut script: Vec<Step> = chars
        .chunks(CHARS_PER_CHUNK)
        .map(|c| Step::Text(c.iter().collect()))
        .collect();
    script.push(Step::Done);
    script
}

impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn chat<'a>(
        &'a self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<DeltaStream<'a>, LlmError>> {
        let timeout = req.timeout;
        let mut nth = 0;
        if let Ok(mut r) = self.requests.lock() {
            nth = r.len();
            r.push(req);
        }
        let delay = self.delay;
        let steps = match nth.checked_sub(1) {
            None => self.script.clone(),
            Some(i) => self
                .then
                .get(i.min(self.then.len().saturating_sub(1)))
                .cloned()
                .unwrap_or_else(|| self.script.clone()),
        };
        Box::pin(async move {
            let inner = futures_util::stream::iter(steps).then(move |step| async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                match step {
                    Step::Text(t) => Ok(Delta::text(t)),
                    Step::Done => Ok(Delta::done()),
                    Step::Error(message) => Err(LlmError::Remote {
                        provider: "mock",
                        message,
                    }),
                    Step::Pause(d) => {
                        tokio::time::sleep(d).await;
                        Ok(Delta::text(String::new()))
                    }
                }
            });
            Ok(guard(inner, cancel, timeout))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    async fn collect(s: &mut DeltaStream<'_>) -> (String, Vec<LlmError>) {
        let mut text = String::new();
        let mut errs = Vec::new();
        while let Some(item) = s.next().await {
            match item {
                Ok(d) => text.push_str(&d.text),
                Err(e) => errs.push(e),
            }
        }
        (text, errs)
    }

    #[tokio::test]
    async fn json_round_trips_and_requests_are_recorded() {
        let value =
            serde_json::json!({"question": "How would you design X?", "approach": ["a", "b"]});
        let mock = MockProvider::json(&value);
        let req = ChatRequest::new(vec![Message::user("hi")]).with_temperature(0.7);
        let mut s = mock
            .chat(req.clone(), CancellationToken::new())
            .await
            .unwrap();
        let (text, errs) = collect(&mut s).await;
        assert!(errs.is_empty());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            value
        );
        assert_eq!(mock.requests(), vec![req]);
    }

    #[tokio::test]
    async fn json_with_multibyte_chars_replays_byte_exact_across_several_deltas() {
        let value = serde_json::json!({
            "question": "caf\u{e9} \u{1F3A4} na\u{ef}ve r\u{e9}sum\u{e9} \u{1F3A4}\u{1F3A4}",
            "approach": ["\u{1F3A4}", "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"]
        });
        let expected = value.to_string();
        let mock = MockProvider::json(&value);
        let mut s = mock
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .unwrap();
        let mut deltas = 0;
        let mut text = String::new();
        while let Some(item) = s.next().await {
            let d = item.unwrap();
            if !d.done {
                deltas += 1;
                text.push_str(&d.text);
            }
        }
        assert!(deltas > 1, "consumers must still see a multi-delta stream");
        assert_eq!(text, expected, "concatenation is byte-exact");
        assert!(!text.contains('\u{FFFD}'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            value
        );
    }

    #[tokio::test]
    async fn cancellation_stops_a_slow_reply_promptly() {
        let mock = MockProvider::new(vec![
            Step::Text("one".into()),
            Step::Text("two".into()),
            Step::Done,
        ])
        .with_delay(Duration::from_millis(150));
        let cancel = CancellationToken::new();
        let mut s = mock
            .chat(ChatRequest::new(vec![]), cancel.clone())
            .await
            .unwrap();
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(first.text, "one");
        let started = std::time::Instant::now();
        cancel.cancel();
        let next = s.next().await;
        assert!(matches!(next, Some(Err(LlmError::Cancelled))), "{next:?}");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "cancel was not prompt"
        );
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn phase_timeout_ends_the_stream() {
        let mock = MockProvider::new(vec![Step::Text("late".into()), Step::Done])
            .with_delay(Duration::from_millis(300));
        let req = ChatRequest::new(vec![]).with_timeout(Duration::from_millis(40));
        let mut s = mock.chat(req, CancellationToken::new()).await.unwrap();
        let item = s.next().await;
        assert!(matches!(item, Some(Err(LlmError::Timeout(_)))), "{item:?}");
    }

    #[tokio::test]
    async fn scripted_error_is_surfaced_as_remote() {
        let mock = MockProvider::new(vec![Step::Text("x".into()), Step::Error("boom".into())]);
        let mut s = mock
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .unwrap();
        let (text, errs) = collect(&mut s).await;
        assert_eq!(text, "x");
        assert!(matches!(&errs[..], [LlmError::Remote { message, .. }] if message == "boom"));
    }
}
