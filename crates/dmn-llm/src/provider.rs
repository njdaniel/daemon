//! The provider-agnostic request, delta and error types, the [`Provider`] trait, and [`guard`].

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("{provider} unreachable at {url}: {detail}")]
    Unreachable {
        provider: &'static str,
        url: String,
        detail: String,
    },
    #[error("{provider} returned HTTP {status}: {body}")]
    HttpStatus {
        provider: &'static str,
        status: u16,
        body: String,
    },
    #[error("{provider} stream malformed: {detail}")]
    MalformedStream {
        provider: &'static str,
        detail: String,
    },
    #[error("{provider} reported an error: {message}")]
    Remote {
        provider: &'static str,
        message: String,
    },
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("request cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// One chat completion request. Providers translate it to their wire format.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    /// JSON schema the reply must satisfy. Ollama: `format`; OpenAI: `response_format`;
    /// Anthropic: a tool with this schema.
    pub schema: Option<serde_json::Value>,
    pub temperature: f32,
    /// Phase deadline for the whole stream (SPEC: fast 4 s, slow 20 s).
    pub timeout: Duration,
}

impl ChatRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            schema: None,
            temperature: 0.0,
            timeout: Duration::from_secs(4),
        }
    }
    pub fn with_schema(mut self, schema: serde_json::Value) -> Self {
        self.schema = Some(schema);
        self
    }
    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Token accounting reported by the provider on the final delta, when it has any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Time spent generating, as reported by the provider.
    pub generation: Duration,
}

/// One streamed chunk. `text` is empty and `done` is true on the terminal delta.
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub text: String,
    pub done: bool,
    pub usage: Option<Usage>,
}

impl Delta {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            done: false,
            usage: None,
        }
    }
    pub fn done() -> Self {
        Self {
            text: String::new(),
            done: true,
            usage: None,
        }
    }
}

pub type DeltaStream<'a> = BoxStream<'a, Result<Delta, LlmError>>;

/// An LLM backend. See the crate doc for why this is a boxed future returning a boxed stream.
pub trait Provider: Send + Sync {
    /// Stable name for logs and errors ("ollama", "openai", "anthropic", "mock").
    fn name(&self) -> &'static str;

    /// Start a streaming chat completion. The returned stream ends with a `done` delta, or with
    /// an error; after `cancel` fires it yields `Err(LlmError::Cancelled)` and ends.
    fn chat<'a>(
        &'a self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<DeltaStream<'a>, LlmError>>;

    /// Load the model so the first real request pays no cold start. Default: nothing to do.
    fn warm_up<'a>(&'a self) -> BoxFuture<'a, Result<(), LlmError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Wrap a provider's raw stream with the cancellation and deadline rules every provider shares:
/// the first poll after `cancel` fires yields `Err(Cancelled)`; the first poll after `timeout`
/// elapses yields `Err(Timeout)`; either way the stream then ends and the inner stream is dropped.
pub fn guard<'a, S>(inner: S, cancel: CancellationToken, timeout: Duration) -> DeltaStream<'a>
where
    S: Stream<Item = Result<Delta, LlmError>> + Send + 'a,
{
    guard_until(inner, cancel, Instant::now() + timeout, timeout)
}

/// [`guard`] with an absolute deadline, so a provider that spent part of the phase budget on
/// connecting and waiting for headers hands the *remaining* budget to the stream. `timeout` is
/// the whole phase budget, reported in [`LlmError::Timeout`].
pub fn guard_until<'a, S>(
    inner: S,
    cancel: CancellationToken,
    deadline: Instant,
    timeout: Duration,
) -> DeltaStream<'a>
where
    S: Stream<Item = Result<Delta, LlmError>> + Send + 'a,
{
    Box::pin(Guarded {
        inner: Some(Box::pin(inner)),
        cancelled: Box::pin(cancel.cancelled_owned()),
        timeout,
        deadline: Box::pin(tokio::time::sleep_until(deadline)),
    })
}

/// `err` with every occurrence of `secret` replaced by `<redacted>`, in each text an error
/// carries. A remote API may quote the key back in a 401 body or a mid-stream error frame, and
/// errors reach logs and the status line; the key must reach neither (#59).
pub(crate) fn redact(err: LlmError, secret: Option<&str>) -> LlmError {
    let Some(secret) = secret.filter(|s| !s.is_empty()) else {
        return err;
    };
    let scrub = |s: String| s.replace(secret, "<redacted>");
    match err {
        LlmError::Unreachable {
            provider,
            url,
            detail,
        } => LlmError::Unreachable {
            provider,
            url: scrub(url),
            detail: scrub(detail),
        },
        LlmError::HttpStatus {
            provider,
            status,
            body,
        } => LlmError::HttpStatus {
            provider,
            status,
            body: scrub(body),
        },
        LlmError::MalformedStream { provider, detail } => LlmError::MalformedStream {
            provider,
            detail: scrub(detail),
        },
        LlmError::Remote { provider, message } => LlmError::Remote {
            provider,
            message: scrub(message),
        },
        e @ (LlmError::Timeout(_) | LlmError::Cancelled) => e,
    }
}

/// How much of a non-2xx response body is read for the error message.
pub(crate) const ERROR_BODY_CAP: usize = 4096;

/// Read up to [`ERROR_BODY_CAP`] bytes of a non-2xx body under the same token and deadline
/// as the rest of the request. Whatever arrived before cancel / deadline is returned, so a
/// stalled error response still surfaces as `HttpStatus` within the phase budget.
pub(crate) async fn error_body_within(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    deadline: Instant,
) -> String {
    use futures_util::StreamExt;
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    let read = async {
        while let Some(Ok(chunk)) = stream.next().await {
            body.extend_from_slice(&chunk);
            if body.len() >= ERROR_BODY_CAP {
                break;
            }
        }
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        () = tokio::time::sleep_until(deadline) => {}
        () = read => {}
    }
    body.truncate(ERROR_BODY_CAP);
    String::from_utf8_lossy(&body).chars().take(300).collect()
}

/// Drive `send` (connect, request, response headers) under the same cancellation token and
/// absolute deadline the body stream will use, so no part of a request is unguarded.
pub(crate) async fn send_within(
    provider: &'static str,
    url: &str,
    send: impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    cancel: &CancellationToken,
    deadline: Instant,
    timeout: Duration,
) -> Result<reqwest::Response, LlmError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(LlmError::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(LlmError::Timeout(timeout)),
        r = send => r.map_err(|e| LlmError::Unreachable {
            provider,
            url: url.to_owned(),
            detail: e.to_string(),
        }),
    }
}

type Inner<'a> = Pin<Box<dyn Stream<Item = Result<Delta, LlmError>> + Send + 'a>>;

struct Guarded<'a> {
    /// `None` once the stream has finished; dropping it releases the HTTP response.
    inner: Option<Inner<'a>>,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    timeout: Duration,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Stream for Guarded<'_> {
    type Item = Result<Delta, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if self.cancelled.as_mut().poll(cx).is_ready() {
            self.inner = None;
            return Poll::Ready(Some(Err(LlmError::Cancelled)));
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.inner = None;
            return Poll::Ready(Some(Err(LlmError::Timeout(self.timeout))));
        }
        let Some(inner) = self.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(d))) => {
                if d.done {
                    self.inner = None;
                }
                Poll::Ready(Some(Ok(d)))
            }
            Poll::Ready(Some(Err(e))) => {
                self.inner = None;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                self.inner = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn guard_passes_deltas_through_and_ends_after_done() {
        let inner = futures_util::stream::iter(vec![
            Ok(Delta::text("a")),
            Ok(Delta::done()),
            Ok(Delta::text("never")),
        ]);
        let mut s = guard(inner, CancellationToken::new(), Duration::from_secs(1));
        assert_eq!(s.next().await.unwrap().unwrap(), Delta::text("a"));
        assert!(s.next().await.unwrap().unwrap().done);
        assert!(s.next().await.is_none(), "stream ends after done");
    }

    #[tokio::test]
    async fn guard_reports_cancellation_once_then_ends() {
        let inner = futures_util::stream::pending::<Result<Delta, LlmError>>();
        let cancel = CancellationToken::new();
        let mut s = guard(inner, cancel.clone(), Duration::from_secs(10));
        let waiter = tokio::spawn(async move { s.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let item = waiter.await.unwrap();
        assert!(matches!(item, Some(Err(LlmError::Cancelled))), "{item:?}");
    }

    #[tokio::test]
    async fn guard_reports_timeout() {
        let inner = futures_util::stream::pending::<Result<Delta, LlmError>>();
        let mut s = guard(inner, CancellationToken::new(), Duration::from_millis(30));
        let item = s.next().await;
        assert!(matches!(item, Some(Err(LlmError::Timeout(_)))), "{item:?}");
        assert!(s.next().await.is_none());
    }
}
