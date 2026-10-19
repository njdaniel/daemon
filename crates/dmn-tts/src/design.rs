//! #130: a voice made from a description. Qwen3-TTS has a sister checkpoint,
//! `Qwen3-TTS-12Hz-1.7B-VoiceDesign`, that speaks a line in a voice the request describes in
//! words. Qwen's own pattern is design, then clone: render one sample with VoiceDesign, then
//! enroll that sample into the Base model ([`crate::Qwen3Provider::enroll_as`] with
//! [`crate::Consent::Designed`]), which keeps the voice the same in every later sentence.
//! VoiceDesign is the second service in `sidecars/qwen3-tts/compose.yaml` (profile `design`,
//! `[speak].design_url`), started only while a voice is made.
//!
//! The request, as vLLM-Omni 0.30.0 defines it (`protocol/audio.py`, `serving_speech.py`,
//! `tts_adapters/qwen3_tts.py`, read in the image on 2026-10-18):
//!
//! - `POST /v1/audio/speech` with JSON `{"model", "input", "task_type": "VoiceDesign",
//!   "instructions", "language", "response_format": "wav", "max_new_tokens"}`. `instructions`
//!   is the description (the adapter maps it to the model's `instruct`; VoiceDesign refuses a
//!   request without one, and a description over the server's length cap); no `voice`, and
//!   no `x_vector_only_mode` or `ref_text` (both "only valid for Base task"). The server
//!   renders the whole line before it answers (`non_streaming_mode` is its default for
//!   VoiceDesign) and sends one WAV with its lengths filled in, at 24 kHz.
//! - `GET /health`: 200 once the model is loaded.
//! - Errors: OpenAI's `{"error": {"message"}}`, as for the Base server.

use futures_util::future::BoxFuture;

use crate::http::{blocking_client, status_error, unreachable};
use crate::provider::TtsError;
use crate::qwen3::{CODES, qwen3_language, token_cap};

/// The checkpoint the design service serves, sent as the request's `model`.
pub const QWEN3_DESIGN_MODEL: &str = "Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign";
const SPEECH: &str = "/v1/audio/speech";
const HEALTH: &str = "/health";
/// The most of a designed WAV read: 30 s of 32-bit float at 48 kHz is under 6 MB, and the Base
/// server takes clips up to 10 MB.
const WAV_CAP: u64 = 10 * 1024 * 1024;

/// What to design: the line the sample says, the voice described in words, and the language
/// it is said in (`None`: the model guesses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignRequest {
    pub text: String,
    pub description: String,
    pub language: Option<String>,
}

/// The designed sample: the WAV as the server sent it, and how long it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Designed {
    pub wav: Vec<u8>,
    pub sample_rate: u32,
    pub secs: f64,
}

/// A client for the VoiceDesign server on `[speak].design_url`.
#[derive(Debug, Clone)]
pub struct Qwen3Designer {
    base_url: String,
    model: String,
}

impl Qwen3Designer {
    /// `base_url` like `http://localhost:4125`; a trailing slash is tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_owned(),
            model: QWEN3_DESIGN_MODEL.to_owned(),
        }
    }

    /// The request body. Public so tests can assert its shape. `Err` for an empty line or
    /// description, or a language the model does not speak.
    pub fn build_body(&self, req: &DesignRequest) -> Result<serde_json::Value, TtsError> {
        let (text, description) = (req.text.trim(), req.description.trim());
        if text.is_empty() || description.is_empty() {
            return Err(TtsError::Request(
                "a designed voice needs a line to say and a description of the voice".into(),
            ));
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "input": text,
            "task_type": "VoiceDesign",
            "instructions": description,
            "response_format": "wav",
            "max_new_tokens": token_cap(text),
        });
        if let Some(code) = &req.language {
            let name = qwen3_language(code).ok_or_else(|| {
                TtsError::Request(format!(
                    "Qwen3-TTS does not speak {code:?}; it speaks {}",
                    CODES.join(", ")
                ))
            })?;
            body["language"] = name.into();
        }
        Ok(body)
    }

    /// Render the sample. Resolves when the whole WAV has arrived and parsed.
    pub fn design<'a>(&'a self, req: &DesignRequest) -> BoxFuture<'a, Result<Designed, TtsError>> {
        let body = self.build_body(req);
        let url = format!("{}{SPEECH}", self.base_url);
        Box::pin(async move {
            let body = body?;
            tokio::task::spawn_blocking(move || design_job(&url, &body))
                .await
                .map_err(|e| TtsError::Worker(format!("design task join: {e}")))?
        })
    }

    /// Whether the server answers `/health` with a 2xx now (the model is loaded).
    pub fn healthy<'a>(&'a self) -> BoxFuture<'a, bool> {
        let url = format!("{}{HEALTH}", self.base_url);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                blocking_client(&url)
                    .ok()
                    .and_then(|c| c.get(&url).send().ok())
                    .is_some_and(|r| r.status().is_success())
            })
            .await
            .unwrap_or(false)
        })
    }
}

/// The blocking half of [`Qwen3Designer::design`].
fn design_job(url: &str, body: &serde_json::Value) -> Result<Designed, TtsError> {
    use std::io::Read;
    let client = blocking_client(url)?;
    let resp = client
        .post(url)
        .json(body)
        .send()
        .map_err(|e| unreachable(url, &e))?;
    if !resp.status().is_success() {
        return Err(status_error(resp));
    }
    let mut wav = Vec::new();
    resp.take(WAV_CAP)
        .read_to_end(&mut wav)
        .map_err(|e| TtsError::BodyRead {
            detail: e.to_string(),
        })?;
    let reader =
        hound::WavReader::new(std::io::Cursor::new(&wav)).map_err(|e| TtsError::Decode {
            detail: format!("the designed sample is not a WAV: {e}"),
        })?;
    let spec = reader.spec();
    if spec.sample_rate == 0 || spec.channels == 0 {
        return Err(TtsError::Decode {
            detail: format!(
                "the designed sample declares {} channel(s) at {} Hz",
                spec.channels, spec.sample_rate
            ),
        });
    }
    let secs = f64::from(reader.duration()) / f64::from(spec.sample_rate);
    tracing::info!(secs, rate = spec.sample_rate, "voice designed");
    Ok(Designed {
        wav,
        sample_rate: spec.sample_rate,
        secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{MockServer, Reply, closed_port_url, serve};

    fn pirate() -> DesignRequest {
        DesignRequest {
            text: "Ahoy, the tide waits for no one.".into(),
            description: "grumpy old pirate, gravelly, slow".into(),
            language: Some("en".into()),
        }
    }

    #[test]
    fn the_design_body_carries_the_description_as_instructions_and_no_voice() {
        let d = Qwen3Designer::new("http://localhost:4125/");
        assert_eq!(d.base_url, "http://localhost:4125");
        let body = d.build_body(&pirate()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "model": "Qwen/Qwen3-TTS-12Hz-1.7B-VoiceDesign",
                "input": "Ahoy, the tide waits for no one.",
                "task_type": "VoiceDesign",
                "instructions": "grumpy old pirate, gravelly, slow",
                "response_format": "wav",
                "max_new_tokens": token_cap("Ahoy, the tide waits for no one."),
                "language": "English",
            })
        );
        let auto = d
            .build_body(&DesignRequest {
                language: None,
                ..pirate()
            })
            .unwrap();
        assert!(auto.get("language").is_none(), "{auto}");
        for bad in [
            DesignRequest {
                language: Some("tr".into()),
                ..pirate()
            },
            DesignRequest {
                description: "  ".into(),
                ..pirate()
            },
            DesignRequest {
                text: String::new(),
                ..pirate()
            },
        ] {
            let err = d.build_body(&bad).expect_err("refused");
            assert!(matches!(err, TtsError::Request(_)), "{err}");
        }
    }

    #[tokio::test]
    async fn design_posts_once_and_returns_the_whole_wav_with_its_length() {
        let wav = crate::decode::sine_wav(24_000, &[0.5], 180.0, 7.5);
        let server = serve(vec![Reply::wav(wav.clone()).in_pieces_of(1000)]).unwrap();
        let d = Qwen3Designer::new(&server.url);
        let designed = d.design(&pirate()).await.unwrap();
        assert_eq!(designed.wav, wav);
        assert_eq!(designed.sample_rate, 24_000);
        assert!((designed.secs - 7.5).abs() < 0.01, "{}", designed.secs);
        assert_eq!(server.request_lines(), ["POST /v1/audio/speech"]);
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[0]);
        let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(sent["task_type"], "VoiceDesign");
        assert_eq!(sent["instructions"], "grumpy old pirate, gravelly, slow");
    }

    #[tokio::test]
    async fn a_design_refusal_brings_the_servers_words_and_a_non_wav_is_a_decode_error() {
        let server = serve(vec![
            Reply::new(
                "400 Bad Request",
                "application/json",
                r#"{"error":{"message":"Instructions too long (max 1000 characters)","type":"BadRequestError","code":400}}"#,
            ),
            Reply::json(r#"{"not":"audio"}"#),
        ])
        .unwrap();
        let d = Qwen3Designer::new(&server.url);
        let err = d.design(&pirate()).await.expect_err("refused");
        assert!(err.to_string().contains("Instructions too long"), "{err}");
        let err = d.design(&pirate()).await.expect_err("not a wav");
        assert!(matches!(err, TtsError::Decode { .. }), "{err}");
    }

    #[tokio::test]
    async fn design_health_is_true_only_on_a_2xx() {
        let server = serve(vec![
            Reply::json("{}"),
            Reply::new("503 Service Unavailable", "text/plain", "loading"),
        ])
        .unwrap();
        let d = Qwen3Designer::new(&server.url);
        assert!(d.healthy().await);
        assert!(!d.healthy().await);
        assert_eq!(server.request_lines(), ["GET /health", "GET /health"]);
        let gone = Qwen3Designer::new(closed_port_url().unwrap());
        assert!(!gone.healthy().await);
        let err = gone.design(&pirate()).await.expect_err("nothing there");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
    }
}
