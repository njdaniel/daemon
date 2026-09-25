//! OpenAI-compatible `<base_url>/v1/chat/completions` client: SSE streaming, JSON schema in
//! `response_format` (`json_schema`), deltas in `choices[].delta.content`, terminated by a
//! `[DONE]` frame (SPEC § Context and LLM, Providers). Covers OpenAI, OpenRouter, vLLM and
//! LM Studio: they differ only in `base_url` and model name.
//!
//! The API key, when `[llm].api_key_env` resolves to one, goes into the `Authorization: Bearer`
//! header and nowhere else: it is never logged, and [`OpenAiProvider`] deliberately has no
//! `Debug` impl so the key cannot leak through a format string.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::provider::{
    ChatRequest, Delta, DeltaStream, LlmError, Provider, Usage, error_body_within, guard_until,
    send_within,
};
use crate::sse::SseParser;

const NAME: &str = "openai";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The OpenAI-compatible provider. `base_url` like `https://api.openai.com` or
/// `http://localhost:1234`; a trailing slash is tolerated.
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    /// From the env var `[llm].api_key_env` names; `None` when it resolves to nothing
    /// (LM Studio and vLLM need no key). Sent as a header, never logged.
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self, LlmError> {
        let base_url = base_url.into();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| LlmError::Unreachable {
                provider: NAME,
                url: base_url.clone(),
                detail: format!("cannot build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    /// The request body. Public so tests can assert its shape.
    pub fn build_body(&self, req: &ChatRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role.as_str(), "content": m.content }))
            .collect();
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": req.temperature,
        });
        if let Some(schema) = &req.schema {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "suggestion",
                    "strict": true,
                    "schema": schema,
                }
            });
        }
        body
    }
}

/// One `data:` frame from the SSE stream.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    /// Mid-stream errors arrive as `{"error": {...}}` frames (OpenRouter does this).
    #[serde(default)]
    error: Option<ChunkError>,
    /// Present only on a final usage chunk when the server was asked for it; never asked for,
    /// but mapped when it shows up anyway.
    #[serde(default)]
    usage: Option<ChunkUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
}

#[derive(Debug, Deserialize)]
struct ChunkDelta {
    // Missing on the role-only first chunk, null on some servers' last.
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// What one frame means for the delta stream.
#[derive(Debug)]
pub(crate) enum Frame {
    Text(String),
    /// The `[DONE]` terminator.
    Done,
    /// A usage-only chunk; held and attached to the terminal delta.
    Usage(Usage),
}

/// Parse one `data:` payload. `pub(crate)` for tests.
pub(crate) fn parse_frame(data: &str) -> Result<Frame, LlmError> {
    if data.trim() == "[DONE]" {
        return Ok(Frame::Done);
    }
    let chunk: Chunk = serde_json::from_str(data).map_err(|e| LlmError::MalformedStream {
        provider: NAME,
        detail: format!("{e}: {data}"),
    })?;
    if let Some(error) = chunk.error {
        return Err(LlmError::Remote {
            provider: NAME,
            message: error.message,
        });
    }
    if let Some(usage) = chunk.usage {
        return Ok(Frame::Usage(Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            generation: Duration::ZERO,
        }));
    }
    let text = chunk
        .choices
        .into_iter()
        .filter_map(|c| c.delta.and_then(|d| d.content))
        .collect();
    Ok(Frame::Text(text))
}

impl Provider for OpenAiProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn chat<'a>(
        &'a self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<DeltaStream<'a>, LlmError>> {
        Box::pin(async move {
            // One budget for the whole request: connect + headers here, the body in `guard_until`.
            let deadline = Instant::now() + req.timeout;
            let body = self.build_body(&req);
            let url = self.url();
            let mut http = self.client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                http = http.bearer_auth(key);
            }
            let resp = send_within(NAME, &url, http.send(), &cancel, deadline, req.timeout).await?;
            let status = resp.status();
            if !status.is_success() {
                let body = error_body_within(resp, &cancel, deadline).await;
                return Err(LlmError::HttpStatus {
                    provider: NAME,
                    status: status.as_u16(),
                    body,
                });
            }
            tracing::debug!(model = %self.model, "openai stream open");

            // State: the byte stream, the SSE parser, frames parsed but not yet yielded, whether
            // the body has ended, and the usage to attach to the terminal delta.
            struct St<B> {
                bytes: B,
                parser: SseParser,
                pending: VecDeque<String>,
                ended: bool,
                usage: Option<Usage>,
            }
            let st = St {
                bytes: resp.bytes_stream(),
                parser: SseParser::new(),
                pending: VecDeque::new(),
                ended: false,
                usage: None,
            };
            let deltas = futures_util::stream::unfold(st, |mut st| async move {
                loop {
                    if let Some(data) = st.pending.pop_front() {
                        match parse_frame(&data) {
                            Ok(Frame::Text(text)) => return Some((Ok(Delta::text(text)), st)),
                            Ok(Frame::Done) => {
                                let mut done = Delta::done();
                                done.usage = st.usage;
                                return Some((Ok(done), st));
                            }
                            Ok(Frame::Usage(u)) => st.usage = Some(u),
                            Err(e) => return Some((Err(e), st)),
                        }
                        continue;
                    }
                    if st.ended {
                        return None;
                    }
                    match st.bytes.next().await {
                        Some(Ok(chunk)) => st.pending.extend(st.parser.feed(&chunk)),
                        Some(Err(e)) => {
                            st.ended = true;
                            return Some((
                                Err(LlmError::MalformedStream {
                                    provider: NAME,
                                    detail: format!("body read failed: {e}"),
                                }),
                                st,
                            ));
                        }
                        None => {
                            st.ended = true;
                            if let Some(tail) = st.parser.finish() {
                                st.pending.push_back(tail);
                            }
                        }
                    }
                }
            });
            Ok(guard_until(deltas, cancel, deadline, req.timeout))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use crate::testserver;

    const KEY: &str = "sk-test-openai-never-log-me";

    fn provider(url: &str) -> OpenAiProvider {
        OpenAiProvider::new(url, "test-model", Some(KEY.to_owned())).unwrap()
    }

    #[test]
    fn body_carries_stream_temperature_and_the_json_schema_format() {
        let schema =
            serde_json::json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let req = ChatRequest::new(vec![Message::system("s"), Message::user("u")])
            .with_schema(schema.clone())
            .with_temperature(0.7);
        let body = provider("http://localhost:1234/").build_body(&req);
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        let t = body["temperature"].as_f64().unwrap();
        assert!((t - 0.7).abs() < 1e-6, "{t}");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "u");
    }

    #[test]
    fn body_without_schema_has_no_response_format_key() {
        let body = provider("http://localhost:1234").build_body(&ChatRequest::new(vec![]));
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn frames_parse_into_deltas() {
        let f = parse_frame(
            r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert!(
            matches!(f, Frame::Text(t) if t.is_empty()),
            "role-only chunk"
        );
        let f = parse_frame(
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"{\"is_qu"},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert!(matches!(f, Frame::Text(t) if t == "{\"is_qu"));
        let f = parse_frame(
            r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":214,"completion_tokens":128}}"#,
        )
        .unwrap();
        assert!(
            matches!(f, Frame::Usage(u) if u.prompt_tokens == 214 && u.completion_tokens == 128)
        );
        assert!(matches!(parse_frame("[DONE]").unwrap(), Frame::Done));
    }

    #[test]
    fn error_and_garbage_frames_are_typed_errors() {
        let e = parse_frame(r#"{"error":{"message":"rate limited","type":"tokens"}}"#).unwrap_err();
        assert!(matches!(e, LlmError::Remote { message, .. } if message == "rate limited"));
        let e = parse_frame("{not json").unwrap_err();
        assert!(matches!(e, LlmError::MalformedStream { .. }), "{e}");
    }

    #[tokio::test]
    async fn errors_never_contain_the_api_key() {
        // Bind then drop a port so nothing listens on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let p = provider(&format!("http://127.0.0.1:{port}"));
        let err = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .map(|_| ())
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Unreachable { .. }), "{err}");
        assert!(!err.to_string().contains(KEY), "{err}");

        let server = testserver::serve_once(
            "401 Unauthorized",
            "application/json",
            "{\"error\":{\"message\":\"invalid key\"}}".to_owned(),
            1,
        );
        let p = provider(&server.url);
        let err = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .map(|_| ())
            .expect_err("must fail");
        assert!(
            matches!(err, LlmError::HttpStatus { status: 401, .. }),
            "{err}"
        );
        assert!(!err.to_string().contains(KEY), "{err}");
    }

    #[tokio::test]
    async fn the_key_rides_the_authorization_header_and_nowhere_else() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{}\"}}]}\n\ndata: [DONE]\n\n".to_owned(),
            1,
        );
        let p = provider(&server.url);
        let mut s = p
            .chat(
                ChatRequest::new(vec![Message::user("hi")]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut text = String::new();
        let mut done = false;
        while let Some(item) = s.next().await {
            let d = item.unwrap();
            text.push_str(&d.text);
            done |= d.done;
        }
        assert!(done, "stream ends with [DONE]");
        assert_eq!(text, "{}");
        let req = server.requests();
        assert_eq!(req.len(), 1);
        assert!(
            req[0].starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{}",
            req[0]
        );
        assert!(
            req[0]
                .to_lowercase()
                .contains(&format!("authorization: bearer {KEY}")),
            "{}",
            req[0]
        );
    }

    #[tokio::test]
    async fn no_key_means_no_authorization_header() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "data: [DONE]\n\n".to_owned(),
            1,
        );
        let p = OpenAiProvider::new(&server.url, "m", None).unwrap();
        let mut s = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .unwrap();
        while s.next().await.is_some() {}
        let req = server.requests();
        assert!(
            !req[0].to_lowercase().contains("authorization"),
            "{}",
            req[0]
        );
    }

    #[tokio::test]
    async fn an_error_frame_mid_stream_is_remote() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\"}}]}\n\ndata: {\"error\":{\"message\":\"provider overloaded\"}}\n\n".to_owned(),
            1,
        );
        let p = provider(&server.url);
        let mut s = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .unwrap();
        let mut err = None;
        while let Some(item) = s.next().await {
            if let Err(e) = item {
                err = Some(e);
                break;
            }
        }
        match &err {
            Some(LlmError::Remote { message, .. }) => assert_eq!(message, "provider overloaded"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_malformed_frame_is_a_typed_error() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "data: not json at all\n\n".to_owned(),
            1,
        );
        let p = provider(&server.url);
        let mut s = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .unwrap();
        let item = s.next().await;
        assert!(
            matches!(item, Some(Err(LlmError::MalformedStream { .. }))),
            "{item:?}"
        );
    }

    /// A recorded OpenAI-compatible SSE reply (the wire shape OpenRouter, vLLM and LM Studio
    /// share): a role-only first chunk, then the JSON in pieces — `is_question`, `kind`,
    /// `question` and `focus` first, then one approach item per chunk — a finish chunk, and the
    /// `[DONE]` terminator.
    const RECORDED: &str = concat!(
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"is_question\\\":true,\\\"kind\\\":\\\"system_design\\\",\\\"question\\\":\\\"How would you design a rate limiter for a globally distributed API?\\\",\\\"focus\\\":\\\"architecture, consistency tradeoffs, Redis, failure handling\\\",\\\"approach\\\":[\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\\"Clarify requirements and rate-limit scope.\\\",\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\\"Pick an algorithm: token bucket per key.\\\",\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\\"Store counters in Redis, sharded by key.\\\",\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\\"Trade exactness for latency with local caches.\\\",\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\\\"Fail open when Redis is unreachable.\\\"]}\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-9f2c\",\"object\":\"chat.completion.chunk\",\"created\":1764026400,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    /// The fast phase over the recorded reply: the question is published before the Approach,
    /// the lines land one by one in order, and the request carried the fast schema as
    /// `response_format`. Mirrors `fast.rs`'s MockProvider test, through HTTP and SSE.
    #[tokio::test]
    async fn fast_phase_streams_the_question_first_then_lines_one_by_one() {
        use crate::fast::{FastEvent, FastOutcome, run_fast_phase};
        use crate::prompt::FastRequest;
        use crate::schema::fast_schema;
        use std::time::Instant;

        let server = testserver::serve_once("200 OK", "text/event-stream", RECORDED.to_owned(), 24);
        let p = provider(&server.url);
        let req = FastRequest::new("How would you design a rate limiter?", 5);
        let mut events: Vec<(Instant, FastEvent)> = Vec::new();
        let outcome = run_fast_phase(&p, &req, CancellationToken::new(), |e| {
            events.push((Instant::now(), e));
        })
        .await;

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
        assert!(lines.windows(2).all(|w| w[0].0 < w[1].0), "one by one");
        assert!(events[0].0 < lines[0].0, "question before the first line");

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
        assert_eq!(suggestion.approach.len(), 5);
        assert_eq!(
            suggestion.question,
            "How would you design a rate limiter for a globally distributed API?"
        );

        // The request: the fast schema as response_format, streaming, the key as a header.
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let (head, body) = testserver::MockServer::head_and_body(&requests[0]);
        assert!(
            head.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{head}"
        );
        assert!(
            head.to_lowercase()
                .contains("authorization: bearer sk-test")
        );
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["model"], "test-model");
        assert_eq!(
            body["response_format"],
            serde_json::json!({
                "type": "json_schema",
                "json_schema": {"name": "suggestion", "strict": true, "schema": fast_schema()}
            })
        );
    }

    /// A reply past the field limits is cut and flagged, through the same SSE path.
    #[tokio::test]
    async fn fast_phase_applies_the_limits_to_the_streamed_reply() {
        use crate::fast::{FastEvent, FastOutcome, run_fast_phase};
        use crate::limits;
        use crate::prompt::FastRequest;

        let wall = "Consider the tradeoffs between consistency and availability in every region. "
            .repeat(6);
        let reply = serde_json::json!({
            "is_question": true,
            "kind": "knowledge",
            "question": "What is CAP?",
            "focus": wall,
            "approach": ["One.", "Two.", "Three.", "Four.", "Five.", "Six.", "Seven."]
        });
        let frame = serde_json::json!({
            "id": "chatcmpl-lim",
            "choices": [{"index": 0, "delta": {"content": reply.to_string()}, "finish_reason": null}]
        });
        let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
        let server = testserver::serve_once("200 OK", "text/event-stream", body, 4);
        let p = provider(&server.url);
        let mut events = Vec::new();
        let outcome = run_fast_phase(
            &p,
            &FastRequest::new("What is CAP?", 4),
            CancellationToken::new(),
            |e| events.push(e),
        )
        .await;

        for e in &events {
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
        let Ok(FastOutcome::Shown {
            flags, suggestion, ..
        }) = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(suggestion.approach.len(), limits::APPROACH_MAX);
        let fields: Vec<&str> = flags.iter().map(|f| f.field.as_str()).collect();
        assert!(fields.contains(&"focus"), "{fields:?}");
        assert!(
            flags
                .iter()
                .any(|f| f.field == "approach"
                    && f.kind == limits::FlagKind::Dropped { from: 7, to: 5 }),
            "{fields:?}"
        );
    }
}
