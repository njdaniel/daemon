//! Client for chatterbox-tts-api (<https://github.com/travisvn/chatterbox-tts-api>), the
//! OpenAI-compatible server around Resemble AI's Chatterbox that the user runs as a sidecar on
//! `[speak].tts_url` (default `http://localhost:4123`), like Ollama.
//!
//! The requests, as the server's source defines them (`app/api/endpoints/speech.py`,
//! `app/api/endpoints/voices.py`, `app/models/requests.py`, `app/core/aliases.py`):
//!
//! - Synthesis: `POST /v1/audio/speech` with JSON `{"model", "input", "voice",
//!   "response_format": "wav"}` (plus `"language"` when the request has one). The server
//!   resolves `voice` in its voice library and speaks in the language that voice was enrolled
//!   with; `model` and `language` are not read by it (they are kept for other OpenAI-compatible
//!   servers). It answers with the whole WAV once rendered. [`ChatterboxProvider::streaming`]
//!   uses `POST /v1/audio/speech/stream` instead, same body: 16-bit mono at the model's rate,
//!   sent sentence by sentence behind a header whose `data` length is `0xFFFFFFFF`.
//! - Voice check: an unknown `voice` is not an error there; the server speaks in its default
//!   voice. So every synthesis first lists `GET /v1/voices` (`{"voices": [{"name", "aliases",
//!   ...}]}`) and refuses a name that is not in it. A server without that route (404/405) or
//!   with an unreadable listing is trusted as is.
//! - Enrollment: `POST /v1/voices`, `multipart/form-data` with `voice_name`, `language` and
//!   `voice_file` (a file name ending `.wav`). 201 on success; 409 when the name exists, after
//!   which the old voice is removed with `DELETE /v1/voices/{name}` and the upload repeated.
//!
//! Errors are read tolerantly: FastAPI's `{"detail": {"error": {"message"}}}`, a validation
//! error's `{"detail": [{"msg"}]}`, OpenAI's `{"error": {"message"}}`, or the raw text.
//!
//! The blocking client is built inside the decoder thread / the enroll task, never held on the
//! provider: a `reqwest::blocking::Client` embeds its own tokio runtime, and dropping one from
//! an async context panics, while a provider dropped at the end of a request future would do
//! exactly that.

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::decode::decode_reader;
use crate::provider::{Enrolled, PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider, Voice};

const NAME: &str = "chatterbox";
const SPEECH: &str = "/v1/audio/speech";
const SPEECH_STREAM: &str = "/v1/audio/speech/stream";
const VOICES: &str = "/v1/voices";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole-request ceiling, headers and body. Speech of a sentence or two renders in seconds on
/// a GPU and tens of seconds on a CPU; this only bounds a stalled server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Chunks buffered between the decoder thread and the caller; 32 × 4800 frames is about 6 s of
/// audio at 24 kHz.
const CHANNEL_CAP: usize = 32;
/// How much of a non-2xx response body is read for the error message.
const ERROR_BODY_CAP: u64 = 16 * 1024;
/// Characters of the server's error message kept.
const MESSAGE_CHARS: usize = 300;
/// The server's own limit on a voice name (`voices.py`: `max_length=100`).
const MAX_VOICE_NAME: usize = 100;

/// An error and its sources, `: `-joined: reqwest's top-level text ("error sending request")
/// hides the useful part ("Connection refused").
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let s_text = s.to_string();
        if !text.contains(&s_text) {
            text.push_str(": ");
            text.push_str(&s_text);
        }
        source = s.source();
    }
    text
}

fn blocking_client(url: &str) -> Result<reqwest::blocking::Client, TtsError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| TtsError::Unreachable {
            url: url.to_owned(),
            detail: format!("cannot build the HTTP client: {}", error_chain(&e)),
        })
}

fn unreachable(url: &str, e: &reqwest::Error) -> TtsError {
    TtsError::Unreachable {
        url: url.to_owned(),
        detail: error_chain(e),
    }
}

/// The server's own words from an error body, or the raw text, capped.
pub(crate) fn server_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| json_message(&v))
        .unwrap_or_else(|| text.trim().to_owned());
    let message = if message.is_empty() {
        "(no message)".to_owned()
    } else {
        message
    };
    message.chars().take(MESSAGE_CHARS).collect()
}

fn json_message(v: &serde_json::Value) -> Option<String> {
    for pointer in [
        "/detail/error/message",
        "/error/message",
        "/detail/message",
        "/message",
    ] {
        if let Some(s) = v.pointer(pointer).and_then(|m| m.as_str()) {
            return Some(s.to_owned());
        }
    }
    match v.get("detail") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        // A FastAPI validation error: one entry per bad field.
        Some(serde_json::Value::Array(items)) => {
            let msgs: Vec<String> = items
                .iter()
                .filter_map(|i| {
                    let msg = i.get("msg")?.as_str()?;
                    let field = i
                        .get("loc")
                        .and_then(|l| l.as_array())
                        .and_then(|l| l.last())
                        .and_then(|f| f.as_str());
                    Some(match field {
                        Some(f) => format!("{f}: {msg}"),
                        None => msg.to_owned(),
                    })
                })
                .collect();
            (!msgs.is_empty()).then(|| msgs.join("; "))
        }
        _ => None,
    }
}

/// A non-2xx answer as [`TtsError::HttpStatus`], with the server's message.
fn status_error(resp: reqwest::blocking::Response) -> TtsError {
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    let _ = resp.take(ERROR_BODY_CAP).read_to_end(&mut bytes);
    TtsError::HttpStatus {
        status,
        message: server_message(&bytes),
    }
}

/// A voice name the server can store and a URL path can carry: the server refuses
/// `/ \ : * ? " < > |` and more than 100 characters; control characters would break the
/// multipart form.
fn check_voice_name(name: &str) -> Result<(), TtsError> {
    let bad = |c: char| c.is_control() || "/\\:*?\"<>|".contains(c);
    if name.trim().is_empty() || name.chars().count() > MAX_VOICE_NAME || name.contains(bad) {
        return Err(TtsError::Request(format!(
            "voice name {name:?} cannot be stored by the TTS server (1-{MAX_VOICE_NAME} characters, none of / \\ : * ? \" < > |)"
        )));
    }
    Ok(())
}

/// `name` as one URL path segment.
fn path_segment(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The voices in a `GET /v1/voices` answer, with their aliases and the language each was
/// enrolled with; `None` when it is not that shape.
fn voice_list(body: &[u8]) -> Option<Vec<Voice>> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let entries = v.get("voices")?.as_array()?;
    let mut voices = Vec::new();
    for entry in entries {
        // Tolerant: an entry may be a bare name, or an object with a name and aliases.
        if let Some(n) = entry.as_str() {
            voices.push(Voice {
                name: n.to_owned(),
                aliases: Vec::new(),
                language: None,
            });
            continue;
        }
        let aliases: Vec<String> = entry
            .get("aliases")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|a| a.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let name = entry.get("name").and_then(|n| n.as_str());
        if name.is_none() && aliases.is_empty() {
            continue;
        }
        voices.push(Voice {
            name: name.unwrap_or_default().to_owned(),
            aliases,
            language: entry
                .get("language")
                .and_then(|l| l.as_str())
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned),
        });
    }
    Some(voices)
}

/// `GET /v1/voices`. `Ok(None)` when the server has no voice library (another
/// OpenAI-compatible one: an error status) or answers with something that is not a listing.
fn list_voices(
    client: &reqwest::blocking::Client,
    base: &str,
) -> Result<Option<Vec<Voice>>, TtsError> {
    let url = format!("{base}{VOICES}");
    let resp = client.get(&url).send().map_err(|e| unreachable(&url, &e))?;
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), url = %url, "cannot list the TTS server's voices");
        return Ok(None);
    }
    let mut body = Vec::new();
    resp.take(4 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|e| TtsError::BodyRead {
            detail: error_chain(&e),
        })?;
    let voices = voice_list(&body);
    if voices.is_none() {
        tracing::warn!(url = %url, "the TTS server's voice listing is not the expected shape");
    }
    Ok(voices)
}

/// Refuse a voice the server's library does not have (it would speak in its default voice).
/// A server with no library to list is trusted as is.
fn check_voice(
    client: &reqwest::blocking::Client,
    base: &str,
    voice: &str,
) -> Result<(), TtsError> {
    let Some(voices) = list_voices(client, base)? else {
        return Ok(());
    };
    if voices.iter().any(|v| v.answers_to(voice)) {
        return Ok(());
    }
    let names: Vec<&str> = voices
        .iter()
        .flat_map(|v| std::iter::once(v.name.as_str()).chain(v.aliases.iter().map(String::as_str)))
        .filter(|n| !n.is_empty())
        .collect();
    Err(TtsError::UnknownVoice {
        voice: voice.to_owned(),
        known: if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        },
    })
}

#[derive(Debug, Clone)]
pub struct ChatterboxProvider {
    base_url: String,
    model: String,
    route: &'static str,
}

impl ChatterboxProvider {
    /// `base_url` like `http://localhost:4123`; a trailing slash is tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_owned(),
            model: "chatterbox".to_owned(),
            route: SPEECH,
        }
    }

    /// The `model` field of the speech request; default "chatterbox" (chatterbox-tts-api does
    /// not read it).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Ask the server's streaming route, which sends audio sentence by sentence as it renders
    /// instead of the whole WAV at the end: the first audio sooner, for playback (#108).
    pub fn streaming(mut self) -> Self {
        self.route = SPEECH_STREAM;
        self
    }

    /// The speech request body. Public so tests can assert its shape.
    pub fn build_speech_body(&self, req: &SpeakRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "input": req.text,
            "voice": req.voice,
            "response_format": "wav",
        });
        if let Some(language) = &req.language {
            body["language"] = language.clone().into();
        }
        body
    }
}

/// The decoder thread: check the voice, POST the speech request, then decode the WAV body
/// chunk by chunk into `chunks`. Every failure up to the WAV header goes out on `ready` (so
/// `synthesize`'s future fails before returning); a later one is one `Err` chunk.
fn speech_job(
    base: String,
    route: &'static str,
    body: serde_json::Value,
    voice: String,
    ready: crossbeam_channel::Sender<Result<u32, TtsError>>,
    chunks: crossbeam_channel::Sender<Result<PcmChunk, TtsError>>,
) {
    let fail = |e: TtsError| {
        let _ = ready.send(Err(e));
    };
    let url = format!("{base}{route}");
    let client = match blocking_client(&url) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    if let Err(e) = check_voice(&client, &base, &voice) {
        return fail(e);
    }
    let resp = match client.post(&url).json(&body).send() {
        Ok(r) => r,
        Err(e) => return fail(unreachable(&url, &e)),
    };
    if !resp.status().is_success() {
        return fail(status_error(resp));
    }
    tracing::debug!(url = %url, voice = %voice, "tts answer arriving");
    decode_reader(
        resp,
        |rate| {
            let _ = ready.send(rate);
        },
        // A full channel blocks here until the caller drains it: that is the backpressure.
        // A send to a dropped stream fails, and the decoder stops reading.
        |chunk| chunks.send(chunk).is_ok(),
    );
}

/// The `multipart/form-data` body for `POST /v1/voices`, built by hand (three fields) rather
/// than through reqwest's `multipart` feature, which would add `mime_guess` and `unicase`.
fn voice_form(name: &str, language: Option<&str>, clip: &[u8]) -> (String, Vec<u8>) {
    // A boundary that does not occur in the clip.
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ u128::from(std::process::id());
    let boundary = loop {
        let b = format!("dmn-tts-{seed:032x}");
        if !clip.windows(b.len()).any(|w| w == b.as_bytes()) {
            break b;
        }
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    };
    let mut body = Vec::with_capacity(clip.len() + 512);
    let mut field = |key: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("voice_name", name);
    if let Some(lang) = language {
        field("language", lang);
    }
    // The server takes the stored file's format from this name's extension.
    let file_name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"voice_file\"; filename=\"{file_name}.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(clip);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The blocking half of [`ChatterboxProvider::enroll`].
fn enroll_job(
    base: &str,
    name: &str,
    language: Option<&str>,
    clip: Vec<u8>,
) -> Result<Enrolled, TtsError> {
    let url = format!("{base}{VOICES}");
    let client = blocking_client(&url)?;
    let (content_type, form) = voice_form(name, language, &clip);
    let upload = || {
        client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, content_type.as_str())
            .body(form.clone())
            .send()
            .map_err(|e| unreachable(&url, &e))
    };
    let mut resp = upload()?;
    let mut replaced = false;
    if resp.status() == reqwest::StatusCode::CONFLICT {
        // Re-enrolling under the same name: the server refuses to overwrite, so the old voice
        // goes first.
        let del_url = format!("{base}{VOICES}/{}", path_segment(name));
        let del = client
            .delete(&del_url)
            .send()
            .map_err(|e| unreachable(&del_url, &e))?;
        if !del.status().is_success() && del.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(status_error(del));
        }
        replaced = true;
        resp = upload()?;
    }
    if !resp.status().is_success() {
        return Err(status_error(resp));
    }
    let mut body = Vec::new();
    let _ = resp.take(64 * 1024).read_to_end(&mut body);
    let registered = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.pointer("/voice/name")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| name.to_owned());
    tracing::info!(voice = %registered, replaced, "voice enrolled");
    Ok(Enrolled {
        name: registered,
        replaced,
    })
}

impl TtsProvider for ChatterboxProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>> {
        Box::pin(async move {
            let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
            let (chunk_tx, chunk_rx) = crossbeam_channel::bounded(CHANNEL_CAP);
            let body = self.build_speech_body(&req);
            let (base, route, voice) = (self.base_url.clone(), self.route, req.voice);
            std::thread::Builder::new()
                .name("dmn-tts-decode".to_owned())
                .spawn(move || speech_job(base, route, body, voice, ready_tx, chunk_tx))
                .map_err(|e| TtsError::Worker(format!("cannot spawn the decoder thread: {e}")))?;
            let ready = tokio::task::spawn_blocking(move || ready_rx.recv())
                .await
                .map_err(|e| TtsError::Worker(format!("decoder thread join: {e}")))?;
            match ready {
                Ok(Ok(sample_rate)) => Ok(PcmStream::new(chunk_rx, sample_rate)),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(TtsError::Worker(
                    "the decoder thread ended before the server answered".into(),
                )),
            }
        })
    }

    fn enroll<'a>(
        &'a self,
        name: &'a str,
        wav: &'a Path,
        language: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Enrolled, TtsError>> {
        Box::pin(async move {
            check_voice_name(name)?;
            let clip = std::fs::read(wav).map_err(|e| TtsError::VoiceRead {
                path: wav.display().to_string(),
                detail: e.to_string(),
            })?;
            let (base, name, language) = (
                self.base_url.clone(),
                name.to_owned(),
                language.map(str::to_owned),
            );
            tokio::task::spawn_blocking(move || enroll_job(&base, &name, language.as_deref(), clip))
                .await
                .map_err(|e| TtsError::Worker(format!("enroll task join: {e}")))?
        })
    }

    fn voices<'a>(&'a self) -> BoxFuture<'a, Result<Option<Vec<Voice>>, TtsError>> {
        Box::pin(async move {
            let base = self.base_url.clone();
            tokio::task::spawn_blocking(move || {
                let url = format!("{base}{VOICES}");
                list_voices(&blocking_client(&url)?, &base)
            })
            .await
            .map_err(|e| TtsError::Worker(format!("voices task join: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{MockServer, Reply, closed_port_url, serve};

    fn sine_wav() -> Vec<u8> {
        crate::decode::sine_wav(24_000, &[0.5], 220.0, 0.5)
    }

    fn speak_req() -> SpeakRequest {
        SpeakRequest::new("hola, ¿qué tal?", "voice")
    }

    fn drain(stream: PcmStream) -> Vec<f32> {
        stream.flat_map(|c| c.unwrap().samples).collect()
    }

    fn body_json(server: &MockServer, i: usize) -> serde_json::Value {
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[i]);
        serde_json::from_slice(body).unwrap()
    }

    #[test]
    fn speech_body_is_the_openai_shape_with_the_enrolled_voice() {
        let p = ChatterboxProvider::new("http://localhost:4123/");
        assert_eq!(p.base_url, "http://localhost:4123");
        let body = p.build_speech_body(&speak_req().with_language("es"));
        assert_eq!(
            body,
            serde_json::json!({
                "model": "chatterbox",
                "input": "hola, ¿qué tal?",
                "voice": "voice",
                "response_format": "wav",
                "language": "es",
            })
        );
        let plain = p.build_speech_body(&speak_req());
        assert!(plain.get("language").is_none(), "{plain}");
        let p = p.with_model("chatterbox-multilingual");
        assert_eq!(
            p.build_speech_body(&speak_req())["model"],
            "chatterbox-multilingual"
        );
    }

    #[tokio::test]
    async fn synthesize_checks_the_voice_then_streams_the_decoded_wav() {
        let server = serve(vec![
            Reply::voices(&["other", "voice"]),
            Reply::wav(sine_wav()).in_pieces_of(7),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let stream = p.synthesize(speak_req().with_language("es")).await.unwrap();
        assert_eq!(stream.sample_rate(), 24_000);
        let samples = drain(stream);
        assert_eq!(samples.len(), 12_000, "0.5 s at 24 kHz");
        let rms = (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");

        assert_eq!(
            server.request_lines(),
            ["GET /v1/voices", "POST /v1/audio/speech"]
        );
        let requests = server.requests();
        let (head, _) = MockServer::head_and_body(&requests[1]);
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json"),
            "{head}"
        );
        let json = body_json(&server, 1);
        assert_eq!(json["input"], "hola, ¿qué tal?");
        assert_eq!(json["voice"], "voice");
        assert_eq!(json["response_format"], "wav");
        assert_eq!(json["language"], "es");
    }

    /// The streaming route's answer: chunked, header `data` length 0xFFFFFFFF, 16-bit mono,
    /// arriving in odd-sized pieces.
    #[tokio::test]
    async fn the_streaming_route_decodes_a_header_of_unknown_length() {
        let mut wav = sine_wav();
        wav[4..8].copy_from_slice(&0x7FFF_FFDBu32.to_le_bytes());
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::wav(wav).in_pieces_of(13).chunked(),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url).streaming();
        let stream = p.synthesize(speak_req()).await.unwrap();
        assert_eq!(drain(stream).len(), 12_000);
        assert_eq!(server.request_lines()[1], "POST /v1/audio/speech/stream");
    }

    #[tokio::test]
    async fn an_unknown_voice_is_refused_before_any_speech_is_asked() {
        let server = serve(vec![Reply::voices(&["alice"]), Reply::wav(sine_wav())]).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(matches!(err, TtsError::UnknownVoice { .. }), "{err}");
        assert!(err.to_string().contains("\"voice\""), "{err}");
        assert!(err.to_string().contains("alice"), "{err}");
        assert_eq!(server.request_lines(), ["GET /v1/voices"]);
    }

    #[tokio::test]
    async fn an_alias_counts_and_a_server_without_a_voice_library_is_trusted() {
        let server = serve(vec![
            Reply::json(r#"{"voices":[{"name":"me","aliases":["voice"]}],"count":1}"#),
            Reply::wav(sine_wav()),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        assert_eq!(
            drain(p.synthesize(speak_req()).await.unwrap()).len(),
            12_000
        );

        let server = serve(vec![
            Reply::new(
                "404 Not Found",
                "application/json",
                r#"{"detail":"Not Found"}"#,
            ),
            Reply::wav(sine_wav()),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        assert_eq!(
            drain(p.synthesize(speak_req()).await.unwrap()).len(),
            12_000
        );
    }

    #[tokio::test]
    async fn connect_refused_is_unreachable_and_says_why() {
        let url = closed_port_url().unwrap();
        let p = ChatterboxProvider::new(&url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains(&url), "{text}");
        assert!(text.to_lowercase().contains("refused"), "{text}");

        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        std::fs::write(&clip, sine_wav()).unwrap();
        let err = p.enroll("v", &clip, None).await.expect_err("must fail");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_error_status_carries_the_servers_own_message() {
        // FastAPI's HTTPException shape, as the speech route raises it.
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::new(
                "500 Internal Server Error",
                "application/json",
                r#"{"detail":{"error":{"message":"Model not loaded","type":"model_error"}}}"#,
            ),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        match p.synthesize(speak_req()).await.expect_err("must fail") {
            TtsError::HttpStatus { status, message } => {
                assert_eq!((status, message.as_str()), (500, "Model not loaded"));
            }
            other => panic!("expected HttpStatus, got {other}"),
        }

        // A validation error (text too long, say) names the field.
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::new(
                "422 Unprocessable Entity",
                "application/json",
                r#"{"detail":[{"loc":["body","input"],"msg":"String should have at most 3000 characters","type":"string_too_long"}]}"#,
            ),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(
            matches!(err, TtsError::HttpStatus { status: 422, .. }),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("input: String should have at most 3000"),
            "{err}"
        );
    }

    #[test]
    fn server_messages_are_read_tolerantly() {
        assert_eq!(
            server_message(br#"{"error":{"message":"bad voice"}}"#),
            "bad voice"
        );
        assert_eq!(server_message(br#"{"detail":"Not Found"}"#), "Not Found");
        assert_eq!(server_message(b"  plain text\n"), "plain text");
        assert_eq!(server_message(b""), "(no message)");
        assert_eq!(server_message(&[b'x'; 1000]).len(), MESSAGE_CHARS);
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_wav_is_a_decode_error() {
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::new("200 OK", "audio/wav", "not a wav at all"),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(matches!(err, TtsError::Decode { .. }), "{err}");
    }

    #[tokio::test]
    async fn enroll_uploads_the_clip_as_the_servers_multipart_form() {
        let server = serve(vec![Reply::new(
            "201 Created",
            "application/json",
            r#"{"message":"Voice uploaded successfully","voice":{"name":"narrator","language":"es"}}"#,
        )])
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("my-voice.wav");
        let clip_bytes = sine_wav();
        std::fs::write(&clip, &clip_bytes).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let enrolled = p.enroll("narrator", &clip, Some("es")).await.unwrap();
        assert_eq!(
            enrolled,
            Enrolled {
                name: "narrator".into(),
                replaced: false
            }
        );

        assert_eq!(server.request_lines(), ["POST /v1/voices"]);
        let requests = server.requests();
        let (head, body) = MockServer::head_and_body(&requests[0]);
        let boundary = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .starts_with("content-type: multipart/form-data; boundary=")
                    .then(|| l.split_once("boundary=").unwrap().1.trim().to_owned())
            })
            .expect(head);
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("name=\"voice_name\"\r\n\r\nnarrator\r\n"),
            "{text}"
        );
        assert!(text.contains("name=\"language\"\r\n\r\nes\r\n"), "{text}");
        assert!(
            text.contains(
                "name=\"voice_file\"; filename=\"narrator.wav\"\r\nContent-Type: audio/wav"
            ),
            "{text}"
        );
        assert!(
            body.windows(clip_bytes.len()).any(|w| w == clip_bytes),
            "the clip's bytes went along unchanged"
        );
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")), "{text}");
    }

    #[tokio::test]
    async fn re_enrolling_a_name_the_server_has_replaces_it() {
        let server = serve(vec![
            Reply::new(
                "409 Conflict",
                "application/json",
                r#"{"detail":{"error":{"message":"Voice 'my voice' already exists"}}}"#,
            ),
            Reply::json(r#"{"message":"Voice deleted successfully"}"#),
            Reply::new(
                "201 Created",
                "application/json",
                r#"{"voice":{"name":"my voice"}}"#,
            ),
        ])
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        std::fs::write(&clip, sine_wav()).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let enrolled = p.enroll("my voice", &clip, None).await.unwrap();
        assert!(enrolled.replaced);
        assert_eq!(
            server.request_lines(),
            [
                "POST /v1/voices",
                "DELETE /v1/voices/my%20voice",
                "POST /v1/voices"
            ]
        );
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[0]);
        assert!(
            !String::from_utf8_lossy(body).contains("name=\"language\""),
            "no language field when none was given: the server's default applies"
        );
    }

    #[tokio::test]
    async fn enroll_maps_an_error_status_a_missing_file_and_a_bad_name() {
        let server = serve(vec![Reply::new(
            "400 Bad Request",
            "application/json",
            r#"{"detail":{"error":{"message":"Unsupported language: xx"}}}"#,
        )])
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        std::fs::write(&clip, sine_wav()).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let err = p
            .enroll("v", &clip, Some("xx"))
            .await
            .expect_err("must fail");
        assert!(
            matches!(err, TtsError::HttpStatus { status: 400, .. }),
            "{err}"
        );
        assert!(
            err.to_string().contains("Unsupported language: xx"),
            "{err}"
        );

        let err = p
            .enroll("v", &dir.path().join("gone.wav"), None)
            .await
            .expect_err("must fail");
        assert!(matches!(err, TtsError::VoiceRead { .. }), "{err}");

        for bad in [
            "",
            "a/b",
            "a:b",
            "quote\"d",
            "line\nbreak",
            &"x".repeat(101),
        ] {
            let err = p.enroll(bad, &clip, None).await.expect_err(bad);
            assert!(matches!(err, TtsError::Request(_)), "{bad:?}: {err}");
        }
        assert_eq!(
            server.request_lines().len(),
            1,
            "bad names never reach the server"
        );
    }

    /// #109: the listing as chatterbox-tts-api answers it, each voice with the language it was
    /// enrolled with; a server without the route has no library.
    #[tokio::test]
    async fn voices_lists_names_aliases_and_enrolled_languages() {
        let server = serve(vec![
            Reply::json(
                r#"{"voices":[{"name":"me","language":"es","aliases":["voice"],"exists":true},{"name":"en-voice","language":"en"},"bare",{"aliases":["x"]},{"language":"fr"}],"count":5}"#,
            ),
            Reply::new("404 Not Found", "application/json", r#"{"detail":"Not Found"}"#),
            Reply::json(r#"{"unexpected":true}"#),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let voices = p.voices().await.unwrap().unwrap();
        assert_eq!(
            voices,
            [
                Voice {
                    name: "me".into(),
                    aliases: vec!["voice".into()],
                    language: Some("es".into()),
                },
                Voice {
                    name: "en-voice".into(),
                    aliases: vec![],
                    language: Some("en".into()),
                },
                Voice {
                    name: "bare".into(),
                    aliases: vec![],
                    language: None,
                },
                Voice {
                    name: String::new(),
                    aliases: vec!["x".into()],
                    language: None,
                },
            ]
        );
        assert!(voices[0].answers_to("voice") && voices[0].answers_to("me"));
        assert_eq!(p.voices().await.unwrap(), None, "no library on this server");
        assert_eq!(p.voices().await.unwrap(), None, "not a listing");
        assert_eq!(server.request_lines(), ["GET /v1/voices"; 3]);

        let p = ChatterboxProvider::new(closed_port_url().unwrap());
        let err = p.voices().await.expect_err("nothing listening");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
    }

    /// The synthesize future resolves once the WAV header is read; after that the decoder
    /// thread only blocks on the channel, so a current-thread runtime is enough.
    #[tokio::test(flavor = "current_thread")]
    async fn synthesize_works_on_a_current_thread_runtime() {
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::wav(sine_wav()).in_pieces_of(1000),
        ])
        .unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let stream = p.synthesize(speak_req()).await.unwrap();
        assert_eq!(drain(stream).len(), 12_000);
    }
}
