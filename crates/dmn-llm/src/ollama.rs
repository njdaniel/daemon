//! Ollama `/api/chat` client: NDJSON streaming, JSON schema in `format`, `think: false` always,
//! `keep_alive` so the model stays loaded for the session. SPEC § Context and LLM, Providers.

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::ndjson::NdjsonParser;
use crate::provider::{ChatRequest, Delta, DeltaStream, LlmError, Provider, Usage, guard};

const NAME: &str = "ollama";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_KEEP_ALIVE: &str = "30m";

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    keep_alive: String,
}

impl OllamaProvider {
    /// `base_url` like `http://localhost:11434`; a trailing slash is tolerated.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self, LlmError> {
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
            keep_alive: DEFAULT_KEEP_ALIVE.to_owned(),
        })
    }

    /// From `[llm]` in config. `keep_alive` is not a config key (SPEC), so it stays a builder
    /// argument; see [`OllamaProvider::with_keep_alive`].
    pub fn from_config(llm: &dmn_core::config::Llm) -> Result<Self, LlmError> {
        Self::new(llm.base_url.clone(), llm.model.clone())
    }

    /// Ollama `keep_alive` duration string ("30m", "1h", "-1" for forever). Default "30m".
    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = keep_alive.into();
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// The `/api/chat` request body. Public so tests can assert its shape.
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
            // qwen3.6 thinks by default; the thinking tokens alone blow the fast-phase budget
            // (measured 2026-09-21). Never omit this.
            "think": false,
            "keep_alive": self.keep_alive,
            "options": { "temperature": req.temperature },
        });
        if let Some(schema) = &req.schema {
            body["format"] = schema.clone();
        }
        body
    }

    fn unreachable(&self, e: &reqwest::Error) -> LlmError {
        LlmError::Unreachable {
            provider: NAME,
            url: self.base_url.clone(),
            detail: e.to_string(),
        }
    }
}

/// One NDJSON line from `/api/chat`.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    message: Option<ChunkMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    /// Nanoseconds.
    #[serde(default)]
    eval_duration: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChunkMessage {
    #[serde(default)]
    content: String,
}

/// Parse one NDJSON line into a delta. `pub(crate)` for tests.
pub(crate) fn parse_line(line: &[u8]) -> Result<Delta, LlmError> {
    let chunk: Chunk = serde_json::from_slice(line).map_err(|e| LlmError::MalformedStream {
        provider: NAME,
        detail: format!("{e}: {}", String::from_utf8_lossy(line)),
    })?;
    if let Some(message) = chunk.error {
        return Err(LlmError::Remote {
            provider: NAME,
            message,
        });
    }
    let text = chunk.message.map(|m| m.content).unwrap_or_default();
    let usage = chunk.done.then(|| Usage {
        prompt_tokens: chunk.prompt_eval_count.unwrap_or(0),
        completion_tokens: chunk.eval_count.unwrap_or(0),
        generation: Duration::from_nanos(chunk.eval_duration.unwrap_or(0)),
    });
    Ok(Delta {
        text,
        done: chunk.done,
        usage,
    })
}

impl Provider for OllamaProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn chat<'a>(
        &'a self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<DeltaStream<'a>, LlmError>> {
        Box::pin(async move {
            let body = self.build_body(&req);
            let resp = self
                .client
                .post(self.url("/api/chat"))
                .json(&body)
                .send()
                .await
                .map_err(|e| self.unreachable(&e))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(LlmError::HttpStatus {
                    provider: NAME,
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            tracing::debug!(model = %self.model, "ollama stream open");

            // State: the byte stream, the line parser, lines parsed but not yet yielded, and
            // whether the body has ended.
            struct St<B> {
                bytes: B,
                parser: NdjsonParser,
                pending: VecDeque<Vec<u8>>,
                ended: bool,
            }
            let st = St {
                bytes: resp.bytes_stream(),
                parser: NdjsonParser::new(),
                pending: VecDeque::new(),
                ended: false,
            };
            let deltas = futures_util::stream::unfold(st, |mut st| async move {
                loop {
                    if let Some(line) = st.pending.pop_front() {
                        return Some((parse_line(&line), st));
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
            Ok(guard(deltas, cancel, req.timeout))
        })
    }

    /// An empty-messages chat loads the model and applies `keep_alive` without generating.
    fn warm_up<'a>(&'a self) -> BoxFuture<'a, Result<(), LlmError>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "model": self.model,
                "messages": [],
                "stream": false,
                "keep_alive": self.keep_alive,
            });
            let resp = self
                .client
                .post(self.url("/api/chat"))
                .json(&body)
                .send()
                .await
                .map_err(|e| self.unreachable(&e))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(LlmError::HttpStatus {
                    provider: NAME,
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            tracing::info!(model = %self.model, keep_alive = %self.keep_alive, "ollama warm");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    fn provider() -> OllamaProvider {
        OllamaProvider::new("http://localhost:11434/", "test-model").unwrap()
    }

    #[test]
    fn body_carries_think_false_stream_and_schema() {
        let schema =
            serde_json::json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let req = ChatRequest::new(vec![Message::system("s"), Message::user("u")])
            .with_schema(schema.clone())
            .with_temperature(0.7);
        let body = provider().with_keep_alive("1h").build_body(&req);
        assert_eq!(
            body["think"],
            serde_json::json!(false),
            "think:false is mandatory"
        );
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(body["format"], schema);
        assert_eq!(body["keep_alive"], "1h");
        assert_eq!(body["model"], "test-model");
        let temperature = body["options"]["temperature"].as_f64().unwrap();
        assert!((temperature - 0.7).abs() < 1e-6, "{temperature}");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "u");
    }

    #[test]
    fn body_without_schema_has_no_format_key() {
        let body = provider().build_body(&ChatRequest::new(vec![]));
        assert!(body.get("format").is_none());
        assert_eq!(body["keep_alive"], DEFAULT_KEEP_ALIVE);
        assert_eq!(body["think"], serde_json::json!(false));
    }

    #[test]
    fn lines_parse_into_deltas_and_usage() {
        let d = parse_line(
            br#"{"model":"m","message":{"role":"assistant","content":"Hel"},"done":false}"#,
        )
        .unwrap();
        assert_eq!(d, Delta::text("Hel"));
        let d = parse_line(
            br#"{"model":"m","message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":214,"eval_count":128,"eval_duration":740000000}"#,
        )
        .unwrap();
        assert!(d.done);
        let u = d.usage.unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens), (214, 128));
        assert_eq!(u.generation, Duration::from_millis(740));
    }

    #[test]
    fn error_and_garbage_lines_are_typed_errors() {
        let e = parse_line(br#"{"error":"model 'x' not found"}"#).unwrap_err();
        assert!(matches!(e, LlmError::Remote { message, .. } if message.contains("not found")));
        let e = parse_line(b"not json at all").unwrap_err();
        assert!(matches!(e, LlmError::MalformedStream { .. }), "{e}");
    }

    #[tokio::test]
    async fn unreachable_server_is_a_typed_error_not_a_panic() {
        // Bind then drop a port so nothing listens on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let p = OllamaProvider::new(format!("http://127.0.0.1:{port}"), "m").unwrap();
        let err = p
            .chat(ChatRequest::new(vec![]), CancellationToken::new())
            .await
            .map(|_| ())
            .expect_err("must fail");
        assert!(matches!(err, LlmError::Unreachable { .. }), "{err}");
        let err = p.warm_up().await.expect_err("must fail");
        assert!(matches!(err, LlmError::Unreachable { .. }), "{err}");
    }

    /// Needs Ollama on localhost with `qwen3.6:35b-a3b` pulled: `cargo test -p dmn-llm -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_ollama_streams_schema_constrained_json() {
        let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
        p.warm_up().await.unwrap();
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"]
        });
        let req = ChatRequest::new(vec![Message::user("Reply with JSON where ok is true.")])
            .with_schema(schema)
            .with_timeout(Duration::from_secs(10));
        let started = std::time::Instant::now();
        let mut s = p.chat(req, CancellationToken::new()).await.unwrap();
        let mut text = String::new();
        let mut done = false;
        let mut usage = None;
        while let Some(item) = s.next().await {
            let d = item.unwrap();
            text.push_str(&d.text);
            if d.done {
                done = true;
                usage = d.usage;
            }
        }
        let elapsed = started.elapsed();
        assert!(done, "stream must end with done");
        let v: serde_json::Value = serde_json::from_str(text.trim()).expect("valid JSON");
        assert_eq!(v["ok"], serde_json::json!(true));
        eprintln!("live: {text} in {elapsed:?}, usage {usage:?}");
    }
}
