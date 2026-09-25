//! Anthropic `<base_url>/v1/messages` client: SSE streaming, structured output as a forced
//! `tool_use` whose `input_schema` is the phase's JSON schema, so the reply's
//! `content_block_delta` events carry the answer as `partial_json` strings
//! (SPEC § Context and LLM, Providers). `base_url` defaults to `https://api.anthropic.com`.
//!
//! The API key, when `[llm].api_key_env` resolves to one, goes into the `x-api-key` header and
//! nowhere else: it is never logged, and [`AnthropicProvider`] deliberately has no `Debug` impl
//! so the key cannot leak through a format string.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::provider::{
    ChatRequest, Delta, DeltaStream, LlmError, Provider, Role, Usage, error_body_within,
    guard_until, send_within,
};
use crate::sse::SseParser;

const NAME: &str = "anthropic";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The required `anthropic-version` header.
const API_VERSION: &str = "2023-06-01";
/// The forced tool carrying the phase's schema.
const TOOL_NAME: &str = "suggestion";
/// `max_tokens` is mandatory on this API. A fast-phase reply is a few hundred tokens.
const MAX_TOKENS: u32 = 1024;

/// The Anthropic provider. `base_url` like `https://api.anthropic.com`; a trailing slash is
/// tolerated.
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    /// From the env var `[llm].api_key_env` names; `None` when it resolves to nothing. Sent as
    /// the `x-api-key` header, never logged.
    api_key: Option<String>,
}

impl AnthropicProvider {
    pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

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
        format!("{}/v1/messages", self.base_url)
    }

    /// The request body. Public so tests can assert its shape.
    ///
    /// `system` is a top-level parameter on this API, so system messages move out of
    /// `messages` (several join with a blank line). With a schema, the reply is a forced
    /// `suggestion` tool call: `tools` carries the schema and `tool_choice` forces the tool.
    pub fn build_body(&self, req: &ChatRequest) -> serde_json::Value {
        let mut system: Vec<&str> = Vec::new();
        let mut messages: Vec<serde_json::Value> = Vec::new();
        for m in &req.messages {
            match m.role {
                Role::System => system.push(m.content.as_str()),
                Role::User | Role::Assistant => messages
                    .push(serde_json::json!({ "role": m.role.as_str(), "content": m.content })),
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "messages": messages,
            "stream": true,
            "temperature": req.temperature,
        });
        if !system.is_empty() {
            body["system"] = serde_json::json!(system.join("\n\n"));
        }
        if let Some(schema) = &req.schema {
            body["tools"] = serde_json::json!([{
                "name": TOOL_NAME,
                "description": "Answer with the structured suggestion.",
                "input_schema": schema,
            }]);
            body["tool_choice"] = serde_json::json!({ "type": "tool", "name": TOOL_NAME });
        }
        body
    }
}

/// One SSE `data:` payload. Only the fields the stream reading needs are mapped.
#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<EventDelta>,
    #[serde(default)]
    message: Option<StartMessage>,
    #[serde(default)]
    usage: Option<EventUsage>,
    #[serde(default)]
    error: Option<EventError>,
}

#[derive(Debug, Deserialize)]
struct EventDelta {
    // `content_block_delta` carries `type`; a `message_delta`'s delta does not.
    #[serde(default, rename = "type")]
    kind: String,
    /// `input_json_delta`: the forced tool call's arguments, piece by piece.
    #[serde(default)]
    partial_json: Option<String>,
    /// `text_delta`: plain text (no schema was forced).
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: Option<EventUsage>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub(crate) struct EventUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct EventError {
    message: String,
}

/// Parse one `data:` payload into at most one delta. Token counts are folded into `usage`
/// (reported on the terminal delta); `pub(crate)` for tests.
pub(crate) fn parse_event(data: &str, usage: &mut EventUsage) -> Result<Option<Delta>, LlmError> {
    let event: Event = serde_json::from_str(data).map_err(|e| LlmError::MalformedStream {
        provider: NAME,
        detail: format!("{e}: {data}"),
    })?;
    match event.kind.as_str() {
        "message_start" => {
            if let Some(u) = event.message.and_then(|m| m.usage) {
                usage.input_tokens = u.input_tokens;
                usage.output_tokens = u.output_tokens;
            }
            Ok(None)
        }
        "content_block_delta" => {
            let text = event.delta.and_then(|d| match d.kind.as_str() {
                "input_json_delta" => d.partial_json,
                _ => d.text,
            });
            Ok(text.map(Delta::text))
        }
        "message_delta" => {
            if let Some(u) = event.usage {
                usage.output_tokens = u.output_tokens;
            }
            Ok(None)
        }
        "message_stop" => {
            let mut done = Delta::done();
            done.usage = Some(Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                generation: Duration::ZERO,
            });
            Ok(Some(done))
        }
        "error" => {
            let message = event
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown error".into());
            Err(LlmError::Remote {
                provider: NAME,
                message,
            })
        }
        // ping, content_block_start, content_block_stop: no payload for the delta stream.
        _ => Ok(None),
    }
}

impl Provider for AnthropicProvider {
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
            let mut http = self
                .client
                .post(&url)
                .header("anthropic-version", API_VERSION)
                .json(&body);
            if let Some(key) = &self.api_key {
                http = http.header("x-api-key", key);
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
            tracing::debug!(model = %self.model, "anthropic stream open");

            // State: the byte stream, the SSE parser, events parsed but not yet yielded, whether
            // the body has ended, and the token counts the terminal delta reports.
            struct St<B> {
                bytes: B,
                parser: SseParser,
                pending: VecDeque<String>,
                ended: bool,
                usage: EventUsage,
            }
            let st = St {
                bytes: resp.bytes_stream(),
                parser: SseParser::new(),
                pending: VecDeque::new(),
                ended: false,
                usage: EventUsage::default(),
            };
            let deltas = futures_util::stream::unfold(st, |mut st| async move {
                loop {
                    if let Some(data) = st.pending.pop_front() {
                        match parse_event(&data, &mut st.usage) {
                            Ok(Some(delta)) => return Some((Ok(delta), st)),
                            Ok(None) => {}
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

    const KEY: &str = "sk-ant-test-never-log-me";

    fn provider(url: &str) -> AnthropicProvider {
        AnthropicProvider::new(url, "test-model", Some(KEY.to_owned())).unwrap()
    }

    #[test]
    fn body_forces_the_schema_tool_and_moves_system_out_of_messages() {
        let schema =
            serde_json::json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let req = ChatRequest::new(vec![
            Message::system("s1"),
            Message::system("s2"),
            Message::user("u"),
            Message::assistant("a"),
        ])
        .with_schema(schema.clone())
        .with_temperature(0.7);
        let body = provider("https://api.anthropic.com/").build_body(&req);
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["max_tokens"], serde_json::json!(MAX_TOKENS));
        assert_eq!(body["system"], "s1\n\ns2");
        assert_eq!(
            body["messages"],
            serde_json::json!([{"role": "user", "content": "u"}, {"role": "assistant", "content": "a"}])
        );
        assert_eq!(body["tools"][0]["name"], TOOL_NAME);
        assert_eq!(body["tools"][0]["input_schema"], schema);
        assert_eq!(
            body["tool_choice"],
            serde_json::json!({"type": "tool", "name": TOOL_NAME})
        );
        let t = body["temperature"].as_f64().unwrap();
        assert!((t - 0.7).abs() < 1e-6, "{t}");
    }

    #[test]
    fn body_without_schema_has_no_tools_and_no_system_when_none_given() {
        let body = provider("https://api.anthropic.com")
            .build_body(&ChatRequest::new(vec![Message::user("u")]));
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn events_parse_into_deltas_and_usage() {
        let mut usage = EventUsage::default();
        let d = parse_event(
            r#"{"type":"message_start","message":{"id":"m1","usage":{"input_tokens":214,"output_tokens":1}}}"#,
            &mut usage,
        )
        .unwrap();
        assert!(d.is_none());
        assert_eq!(usage.input_tokens, 214);
        let d = parse_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"is_qu"}}"#,
            &mut usage,
        )
        .unwrap();
        assert_eq!(d, Some(Delta::text("{\"is_qu")));
        let d = parse_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            &mut usage,
        )
        .unwrap();
        assert_eq!(d, Some(Delta::text("hi")));
        assert!(
            parse_event(r#"{"type":"ping"}"#, &mut usage)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_event(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use"}}"#,
                &mut usage,
            )
            .unwrap()
            .is_none()
        );
        let d = parse_event(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":128}}"#,
            &mut usage,
        )
        .unwrap();
        assert!(d.is_none());
        let d = parse_event(r#"{"type":"message_stop"}"#, &mut usage).unwrap();
        let done = d.expect("message_stop ends the stream");
        assert!(done.done);
        let u = done.usage.unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (214, 128));
    }

    #[test]
    fn error_and_garbage_events_are_typed_errors() {
        let mut usage = EventUsage::default();
        let e = parse_event(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            &mut usage,
        )
        .unwrap_err();
        assert!(matches!(e, LlmError::Remote { message, .. } if message == "Overloaded"));
        let e = parse_event("{not json", &mut usage).unwrap_err();
        assert!(matches!(e, LlmError::MalformedStream { .. }), "{e}");
    }

    #[tokio::test]
    async fn the_key_rides_the_x_api_key_header_with_the_api_version() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
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
        assert!(done);
        assert_eq!(text, "{}");
        let req = server.requests();
        assert_eq!(req.len(), 1);
        assert!(
            req[0].starts_with("POST /v1/messages HTTP/1.1"),
            "{}",
            req[0]
        );
        let low = req[0].to_lowercase();
        assert!(low.contains(&format!("x-api-key: {KEY}")), "{}", req[0]);
        assert!(
            low.contains(&format!("anthropic-version: {API_VERSION}")),
            "{}",
            req[0]
        );
    }

    #[tokio::test]
    async fn errors_never_contain_the_api_key() {
        let server = testserver::serve_once(
            "401 Unauthorized",
            "application/json",
            "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}".to_owned(),
            1,
        );
        let p = provider(&server.url);
        let err = p
            .chat(
                ChatRequest::new(vec![Message::user("hi")]),
                CancellationToken::new(),
            )
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
    async fn an_error_event_mid_stream_is_remote() {
        let server = testserver::serve_once(
            "200 OK",
            "text/event-stream",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n".to_owned(),
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
            Some(LlmError::Remote { message, .. }) => assert_eq!(message, "Overloaded"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    /// A recorded Anthropic SSE reply with the schema forced as a tool: `message_start` (with
    /// usage), the `tool_use` block start, a `ping`, then the answer JSON as
    /// `input_json_delta` pieces — `is_question`, `kind`, `question` and `focus` first, then one
    /// approach item per event — and the block/message closing events.
    const RECORDED: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01Xy\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"test-model\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":214,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01A\",\"name\":\"suggestion\",\"input\":{}}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"is_question\\\":true,\\\"kind\\\":\\\"system_design\\\",\\\"question\\\":\\\"How would you design a rate limiter for a globally distributed API?\\\",\\\"focus\\\":\\\"architecture, consistency tradeoffs, Redis, failure handling\\\",\\\"approach\\\":[\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Clarify requirements and rate-limit scope.\\\",\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Pick an algorithm: token bucket per key.\\\",\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Store counters in Redis, sharded by key.\\\",\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Trade exactness for latency with local caches.\\\",\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Fail open when Redis is unreachable.\\\"]}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":128}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );

    /// The fast phase over the recorded reply: the question is published before the Approach,
    /// the lines land one by one in order, and the request forced the fast schema as a tool.
    /// Mirrors `fast.rs`'s MockProvider test, through HTTP and SSE.
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

        // The request: system prompt out of `messages`, the fast schema as a forced tool.
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        let (head, body) = testserver::MockServer::head_and_body(&requests[0]);
        assert!(head.starts_with("POST /v1/messages HTTP/1.1"), "{head}");
        let low = head.to_lowercase();
        assert!(low.contains("x-api-key: sk-ant-test"), "{head}");
        assert!(low.contains("anthropic-version: 2023-06-01"), "{head}");
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["tools"][0]["name"], "suggestion");
        assert_eq!(body["tools"][0]["input_schema"], fast_schema());
        assert_eq!(
            body["tool_choice"],
            serde_json::json!({"type": "tool", "name": "suggestion"})
        );
        assert!(
            body["system"]
                .as_str()
                .unwrap()
                .contains("three to five lines"),
            "{}",
            body["system"]
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
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
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": reply.to_string()}
        });
        let body = format!(
            "event: content_block_delta\ndata: {frame}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
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
