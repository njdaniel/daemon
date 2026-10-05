//! Chatterbox-TTS-Server client: OpenAI-compatible `POST /v1/audio/speech` for synthesis, the
//! server's voice-management endpoint for enrollment. The server is a local sidecar
//! (`[speak].tts_url`, default `http://localhost:4123`), like Ollama.
//!
//! Assumed server API (no live server was available; adjust against one when there is):
//! synthesis is the OpenAI shape — `{model, voice, input, response_format: "wav"}` plus an
//! optional `language`; enrollment is `POST /api/voices/add` with a multipart form
//! (`voice_name`, `voice_file`), which any 2xx answer accepts.
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
use crate::provider::{PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider};

const NAME: &str = "chatterbox";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole-request ceiling, headers and body: speech of a few sentences renders in seconds, so
/// this only bounds a stalled server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Chunks buffered between the decoder thread and the caller; 32 × 4800 frames is about 6 s of
/// audio at 24 kHz.
const CHANNEL_CAP: usize = 32;
/// How much of a non-2xx response body is read for the error message.
const ERROR_BODY_CAP: usize = 4096;

fn blocking_client(url: &str) -> Result<reqwest::blocking::Client, TtsError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| TtsError::Unreachable {
            url: url.to_owned(),
            detail: format!("cannot build HTTP client: {e}"),
        })
}

/// A non-2xx answer's body, capped, for the error message.
fn error_body(resp: reqwest::blocking::Response) -> String {
    let mut bytes = Vec::new();
    let _ = resp.take(ERROR_BODY_CAP as u64).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).chars().take(300).collect()
}

#[derive(Debug, Clone)]
pub struct ChatterboxProvider {
    base_url: String,
    model: String,
}

impl ChatterboxProvider {
    /// `base_url` like `http://localhost:4123`; a trailing slash is tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: "chatterbox".to_owned(),
        }
    }

    /// The `model` field of the speech request; default "chatterbox".
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// The `/v1/audio/speech` request body. Public so tests can assert its shape.
    pub fn build_speech_body(&self, req: &SpeakRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "voice": req.voice,
            "input": req.text,
            "response_format": "wav",
        });
        if let Some(language) = &req.language {
            body["language"] = language.clone().into();
        }
        body
    }
}

/// The decoder thread: POST the speech request, then decode the WAV body chunk by chunk into
/// `chunks`. Connect, status and header failures go out on `ready` (so `synthesize`'s future
/// can fail before returning); a mid-body decode failure is one `Err` chunk.
fn speech_job(
    url: String,
    body: serde_json::Value,
    ready: crossbeam_channel::Sender<Result<u32, TtsError>>,
    chunks: crossbeam_channel::Sender<Result<PcmChunk, TtsError>>,
) {
    let send_ready = |e: TtsError| {
        let _ = ready.send(Err(e));
    };
    let client = match blocking_client(&url) {
        Ok(c) => c,
        Err(e) => return send_ready(e),
    };
    let resp = match client.post(&url).json(&body).send() {
        Ok(r) => r,
        Err(e) => {
            return send_ready(TtsError::Unreachable {
                url,
                detail: e.to_string(),
            });
        }
    };
    let status = resp.status();
    if !status.is_success() {
        return send_ready(TtsError::HttpStatus {
            status: status.as_u16(),
            body: error_body(resp),
        });
    }
    tracing::debug!(url = %url, "tts stream open");
    decode_reader(
        resp,
        |rate| {
            let _ = ready.send(rate);
        },
        |chunk| {
            // A full channel blocks here until the caller drains it — that is the
            // backpressure. A disconnected receiver (caller gone) ends the job.
            let _ = chunks.send(chunk);
        },
    );
}

impl TtsProvider for ChatterboxProvider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>> {
        Box::pin(async move {
            let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
            let (chunk_tx, chunk_rx) = crossbeam_channel::bounded(CHANNEL_CAP);
            let url = self.url("/v1/audio/speech");
            let body = self.build_speech_body(&req);
            std::thread::Builder::new()
                .name("dmn-tts-decode".to_owned())
                .spawn(move || speech_job(url, body, ready_tx, chunk_tx))
                .map_err(|e| TtsError::Worker(format!("cannot spawn the decoder thread: {e}")))?;
            let ready = tokio::task::spawn_blocking(move || ready_rx.recv())
                .await
                .map_err(|e| TtsError::Worker(format!("decoder thread join: {e}")))?;
            match ready {
                Ok(Ok(sample_rate)) => Ok(PcmStream::new(chunk_rx, sample_rate)),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(TtsError::Worker(
                    "the decoder thread died before the server answered".into(),
                )),
            }
        })
    }

    fn enroll<'a>(&'a self, name: &'a str, wav: &'a Path) -> BoxFuture<'a, Result<String, TtsError>> {
        Box::pin(async move {
            let bytes = std::fs::read(wav).map_err(|e| TtsError::VoiceRead {
                path: wav.display().to_string(),
                detail: e.to_string(),
            })?;
            let file_name = wav
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "voice.wav".to_owned());
            let url = self.url("/api/voices/add");
            let name = name.to_owned();
            tokio::task::spawn_blocking(move || {
                let client = blocking_client(&url)?;
                let file = reqwest::blocking::multipart::Part::bytes(bytes)
                    .file_name(file_name)
                    .mime_str("audio/wav")
                    .map_err(|e| TtsError::Request(format!("voice clip MIME type: {e}")))?;
                let form = reqwest::blocking::multipart::Form::new()
                    .text("voice_name", name.clone())
                    .part("voice_file", file);
                let resp =
                    client
                        .post(&url)
                        .multipart(form)
                        .send()
                        .map_err(|e| TtsError::Unreachable {
                            url: url.clone(),
                            detail: e.to_string(),
                        })?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(TtsError::HttpStatus {
                        status: status.as_u16(),
                        body: error_body(resp),
                    });
                }
                tracing::info!(voice = %name, "voice enrolled");
                Ok(name)
            })
            .await
            .map_err(|e| TtsError::Worker(format!("enroll task join: {e}")))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{MockServer, serve_once};

    fn sine_wav() -> Vec<u8> {
        crate::decode::sine_wav(24_000, &[0.5], 220.0, 0.5)
    }

    fn speak_req() -> SpeakRequest {
        SpeakRequest::new("hola, ¿qué tal?", "voice")
    }

    #[test]
    fn speech_body_is_the_openai_shape_with_the_enrolled_voice() {
        let p = ChatterboxProvider::new("http://localhost:4123/");
        let body = p.build_speech_body(&speak_req().with_language("es"));
        assert_eq!(body["model"], "chatterbox");
        assert_eq!(body["voice"], "voice");
        assert_eq!(body["input"], "hola, ¿qué tal?");
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body["language"], "es");
        let plain = p.build_speech_body(&speak_req());
        assert!(plain.get("language").is_none(), "{plain}");
        let p = p.with_model("chatterbox-multilingual");
        assert_eq!(p.build_speech_body(&speak_req())["model"], "chatterbox-multilingual");
    }

    #[tokio::test]
    async fn synthesize_streams_the_decoded_wav_and_the_request_is_the_openai_shape() {
        let server = serve_once("200 OK", "audio/wav", sine_wav(), 4);
        let p = ChatterboxProvider::new(&server.url);
        let stream = p.synthesize(speak_req()).await.unwrap();
        assert_eq!(stream.sample_rate(), 24_000);
        let mut samples = 0usize;
        let mut sum_sq = 0.0f64;
        for chunk in stream {
            let chunk = chunk.unwrap();
            samples += chunk.samples.len();
            sum_sq += chunk
                .samples
                .iter()
                .map(|&v| f64::from(v) * f64::from(v))
                .sum::<f64>();
        }
        assert_eq!(samples, 12_000, "0.5 s at 24 kHz");
        let rms = (sum_sq / samples as f64).sqrt();
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 0.01, "rms {rms}");

        let requests = server.requests();
        let (head, body) = MockServer::head_and_body(&requests[0]);
        assert!(head.starts_with("POST /v1/audio/speech "), "{head}");
        let json: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(json["input"], "hola, ¿qué tal?");
        assert_eq!(json["voice"], "voice");
        assert_eq!(json["response_format"], "wav");
    }

    #[tokio::test]
    async fn connect_refused_is_unreachable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let p = ChatterboxProvider::new(format!("http://127.0.0.1:{port}"));
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_error_status_is_typed_and_carries_the_body() {
        let server = serve_once(
            "500 Internal Server Error",
            "text/plain",
            b"voice 'voice' is not enrolled".to_vec(),
            1,
        );
        let p = ChatterboxProvider::new(&server.url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        match err {
            TtsError::HttpStatus { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("not enrolled"), "{body}");
            }
            other => panic!("expected HttpStatus, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_non_wav_body_is_a_decode_error() {
        let server = serve_once("200 OK", "audio/wav", b"not a wav at all".to_vec(), 1);
        let p = ChatterboxProvider::new(&server.url);
        let err = p.synthesize(speak_req()).await.expect_err("must fail");
        assert!(matches!(err, TtsError::Decode { .. }), "{err}");
    }

    #[tokio::test]
    async fn enroll_uploads_the_clip_as_a_named_multipart() {
        let server = serve_once(
            "200 OK",
            "application/json",
            br#"{"status":"success"}"#.to_vec(),
            1,
        );
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("my-voice.wav");
        std::fs::write(&clip, sine_wav()).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let registered = p.enroll("narrator", &clip).await.unwrap();
        assert_eq!(registered, "narrator");

        let requests = server.requests();
        let (head, body) = MockServer::head_and_body(&requests[0]);
        assert!(head.starts_with("POST /api/voices/add "), "{head}");
        assert!(
            head.to_ascii_lowercase().contains("multipart/form-data"),
            "{head}"
        );
        let text = String::from_utf8_lossy(body);
        assert!(text.contains("voice_name"), "{text}");
        assert!(text.contains("narrator"), "{text}");
        assert!(text.contains("voice_file"), "{text}");
        assert!(text.contains("my-voice.wav"), "{text}");
        assert!(text.contains("RIFF"), "the clip's bytes went along: {text}");
    }

    #[tokio::test]
    async fn enroll_maps_an_error_status_and_a_missing_file() {
        let server = serve_once("404 Not Found", "text/plain", b"no such route".to_vec(), 1);
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        std::fs::write(&clip, sine_wav()).unwrap();
        let p = ChatterboxProvider::new(&server.url);
        let err = p.enroll("v", &clip).await.expect_err("must fail");
        assert!(
            matches!(err, TtsError::HttpStatus { status: 404, .. }),
            "{err}"
        );

        let err = p
            .enroll("v", &dir.path().join("gone.wav"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, TtsError::VoiceRead { .. }), "{err}");
    }

    /// The synthesize future resolves once the WAV header is read; after that the decoder
    /// thread only blocks on the channel, so a current-thread runtime is enough.
    #[tokio::test(flavor = "current_thread")]
    async fn synthesize_works_on_a_current_thread_runtime() {
        let server = serve_once("200 OK", "audio/wav", sine_wav(), 2);
        let p = ChatterboxProvider::new(&server.url);
        let stream = p.synthesize(speak_req()).await.unwrap();
        let mut frames = 0usize;
        for chunk in stream {
            frames += chunk.unwrap().samples.len();
        }
        assert_eq!(frames, 12_000);
    }
}
