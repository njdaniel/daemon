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
}

#[derive(Debug, Clone)]
pub struct MockProvider {
    script: Vec<Step>,
    delay: Duration,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl MockProvider {
    /// Replay `script` on every request.
    pub fn new(script: Vec<Step>) -> Self {
        Self {
            script,
            delay: Duration::ZERO,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Reply with `value` serialised as JSON, streamed in small chunks, then `Done`.
    pub fn json(value: &serde_json::Value) -> Self {
        let text = value.to_string();
        let mut script: Vec<Step> = text
            .as_bytes()
            .chunks(7)
            .map(|c| Step::Text(String::from_utf8_lossy(c).into_owned()))
            .collect();
        script.push(Step::Done);
        Self::new(script)
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
        if let Ok(mut r) = self.requests.lock() {
            r.push(req);
        }
        let delay = self.delay;
        let steps = self.script.clone();
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
