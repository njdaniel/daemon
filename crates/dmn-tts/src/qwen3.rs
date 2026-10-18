//! Client for Qwen3-TTS (Alibaba, Apache-2.0) served by vLLM-Omni (#122): the sidecar in
//! `sidecars/qwen3-tts/`, `vllm-omni serve Qwen/Qwen3-TTS-12Hz-1.7B-Base --omni`, on
//! `[speak].tts_url` with `[speak].engine = "qwen3"`. Measured on the 4090 (2026-10-14): first
//! audio 0.04–0.08 s after the request, against 1–2 s for Chatterbox.
//!
//! The requests, as vLLM-Omni 0.30.0 defines them (`entrypoints/openai/api_server.py`,
//! `serving_speech.py`, `protocol/audio.py`, `tts_adapters/qwen3_tts.py`):
//!
//! - Synthesis: `POST /v1/audio/speech` with JSON `{"model", "input", "voice", "task_type":
//!   "Base", "x_vector_only_mode": true, "language", "response_format": "wav", "stream_format":
//!   "audio", "initial_codec_chunk_frames", "max_new_tokens"}`. The answer streams as it
//!   renders: one WAV header whose lengths are `0xFFFFFFFF`, then 16-bit mono PCM at 24 kHz.
//!   `language` is the model's name for it ("Chinese", "Russian"); left out, the model guesses
//!   ("Auto").
//! - First chunk: the server sends the first [`FIRST_CHUNK_FRAMES`] codec frames (80 ms of audio
//!   each) on their own, then blocks of 25 (2 s). Its default first chunk is one frame, at
//!   ~0.04 s, but the next block only comes ~0.3 s after the request, so playback started on
//!   the first frame stalled for up to 0.17 s after it, in 12 of 12 measured lines. Three to
//!   six frames cover that wait, but the model opens most sentences with 0.4–0.8 s of
//!   near-silence (the sidecar's `silence_ban_frames` shortens it, not always), which the
//!   leading trim (`tail.rs`) skips; when the speech starts past the first chunk, the first
//!   sound waits for the next block. Measured 2026-10-18 (24 lines each, the owner's clip), the
//!   first audible sample arrived after 0.105 s median but 0.38 s at p90 with six frames, and
//!   0.147 s median, 0.164 s at most with ten: ten it is (12 frames 0.17 s, 16 frames 0.21 s).
//! - x-vector mode: the voice is cloned from the clip's speaker embedding only. The other mode
//!   (in-context, with the clip's transcript) continued from the English transcript and garbled
//!   Chinese in step 1's test, so dmn never uploads a transcript and always asks for this mode.
//!   One clip speaks every language: the engine is cross-lingual.
//! - Voice check: `GET /v1/audio/voices` answers `{"voices": [built-in names], "uploaded_voices":
//!   [{"name", ...}]}`; only uploaded voices are the user's, and a name missing there is refused
//!   before any speech is asked for. The server matches names case-insensitively.
//! - Enrollment: `POST /v1/audio/voices`, `multipart/form-data` with `name`, `consent` (an id
//!   the server stores with the clip) and `audio_sample` (1–30 s, at most 10 MB). A voice of
//!   that name is deleted first (`DELETE /v1/audio/voices/{name}`), so a re-enrollment replaces
//!   it whatever the server's registration policy.
//! - Errors: OpenAI's `{"error": {"message"}}`.
//!
//! Each sentence is its own request, as for Chatterbox: the measurement asked one sentence at
//! a time, the trailing-silence cut stays a guard against anything after the speech, and
//! `max_new_tokens` is sized to the sentence ([`token_cap`]) so a generation that runs away
//! (the model looped on Chinese once in testing, through another runtime) ends within seconds
//! instead of the server's 2048 tokens (~160 s).

use std::path::Path;

use futures_util::future::BoxFuture;

use crate::http::{
    Job, blocking_client, check_voice_name_chars, file_stem, list_voices, multipart_form,
    path_segment, read_body, status_error, unreachable,
};
use crate::provider::{Enrolled, PcmStream, SpeakRequest, TtsError, TtsProvider, Voice};
use crate::sentences::sentences;

const NAME: &str = "qwen3";
/// The model the sidecar serves, sent as the request's `model`.
pub const QWEN3_MODEL: &str = "Qwen/Qwen3-TTS-12Hz-1.7B-Base";
const SPEECH: &str = "/v1/audio/speech";
const VOICES: &str = "/v1/audio/voices";
/// The consent id the server stores with an uploaded clip: the user enrolls their own voice.
const CONSENT: &str = "dmn-self";
/// Codec frames in the first streamed chunk (module doc: 0.8 s of audio, the first sound at
/// ~0.15 s).
pub(crate) const FIRST_CHUNK_FRAMES: u32 = 10;

/// The languages Qwen3-TTS speaks (vLLM-Omni's `DEFAULT_TTS_LANGUAGES`), as the code dmn's
/// config uses and the name the model takes.
pub const QWEN3_LANGUAGES: [(&str, &str); 10] = [
    ("zh", "Chinese"),
    ("en", "English"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("de", "German"),
    ("fr", "French"),
    ("ru", "Russian"),
    ("pt", "Portuguese"),
    ("es", "Spanish"),
    ("it", "Italian"),
];
const CODES: [&str; 10] = ["zh", "en", "ja", "ko", "de", "fr", "ru", "pt", "es", "it"];

/// The model's name for `code`; `None` for a language it does not speak.
pub fn qwen3_language(code: &str) -> Option<&'static str> {
    QWEN3_LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, name)| *name)
}

/// The codec makes 12.5 tokens per second of audio ("12Hz" in the model's name).
const TOKENS_PER_SEC: f64 = 12.5;

/// `max_new_tokens` for one sentence: 3 s plus 0.5 s per character of audio. Chinese is read at
/// about 4–5 characters a second (0.2–0.25 s each) and Russian or German at 12–15 (under 0.1 s),
/// so this is two to five times what the sentence needs, and still a bound.
pub(crate) fn token_cap(sentence: &str) -> u64 {
    let chars = sentence.chars().count() as f64;
    ((3.0 + 0.5 * chars) * TOKENS_PER_SEC).ceil() as u64
}

/// The [`crate::http::Fit`]: the sentence, and its token cap.
fn fit(body: &mut serde_json::Value, sentence: &str) {
    body["input"] = sentence.into();
    body["max_new_tokens"] = token_cap(sentence).into();
}

/// The uploaded voices in a `GET /v1/audio/voices` answer, each answering to its lowercase name
/// too (the server's matching); `None` when it is not that shape. The built-in `voices` are
/// not the user's and are left out.
fn voice_list(body: &[u8]) -> Option<Vec<Voice>> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let entries = v.get("uploaded_voices")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|e| e.get("name")?.as_str())
            .map(|name| {
                let lower = name.to_lowercase();
                Voice {
                    name: name.to_owned(),
                    aliases: if lower == name {
                        Vec::new()
                    } else {
                        vec![lower]
                    },
                    language: None,
                }
            })
            .collect(),
    )
}

#[derive(Debug, Clone)]
pub struct Qwen3Provider {
    base_url: String,
    model: String,
}

impl Qwen3Provider {
    /// `base_url` like `http://localhost:4124`; a trailing slash is tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim().trim_end_matches('/').to_owned(),
            model: QWEN3_MODEL.to_owned(),
        }
    }

    /// The `model` field of the speech request; default [`QWEN3_MODEL`]. vLLM answers 404 for
    /// a model it does not serve.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// The speech request body, without the per-sentence `max_new_tokens`. Public so tests can
    /// assert its shape. `Err` for a language the model does not speak. The request's tuning
    /// (Chatterbox's knobs) is not sent: the model's own sampling defaults stand.
    pub fn build_speech_body(&self, req: &SpeakRequest) -> Result<serde_json::Value, TtsError> {
        let mut body = serde_json::json!({
            "model": self.model,
            "input": req.text,
            "voice": req.voice,
            "task_type": "Base",
            "x_vector_only_mode": true,
            "response_format": "wav",
            "stream_format": "audio",
            "initial_codec_chunk_frames": FIRST_CHUNK_FRAMES,
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
}

/// The blocking half of [`Qwen3Provider::enroll`].
fn enroll_job(base: &str, name: &str, clip: Vec<u8>) -> Result<Enrolled, TtsError> {
    let url = format!("{base}{VOICES}");
    let client = blocking_client(&url)?;
    let lower = name.to_lowercase();
    let replaced = list_voices(&client, &url, voice_list)?
        .unwrap_or_default()
        .iter()
        .any(|v| v.answers_to(name) || v.answers_to(&lower));
    if replaced {
        let del_url = format!("{url}/{}", path_segment(name));
        let del = client
            .delete(&del_url)
            .send()
            .map_err(|e| unreachable(&del_url, &e))?;
        if !del.status().is_success() && del.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(status_error(del));
        }
    }
    let file_name = format!("{}.wav", file_stem(name));
    let (content_type, form) = multipart_form(
        &[("name", name), ("consent", CONSENT)],
        ("audio_sample", &file_name),
        &clip,
    );
    let resp = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(form)
        .send()
        .map_err(|e| unreachable(&url, &e))?;
    if !resp.status().is_success() {
        return Err(status_error(resp));
    }
    let body = read_body(resp)?;
    let registered = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.pointer("/voice/name")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| name.to_owned());
    tracing::info!(voice = %registered, replaced, "voice enrolled with Qwen3-TTS");
    Ok(Enrolled {
        name: registered,
        replaced,
    })
}

impl TtsProvider for Qwen3Provider {
    fn name(&self) -> &'static str {
        NAME
    }

    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>> {
        Box::pin(async move {
            let body = self.build_speech_body(&req)?;
            let mut parts = sentences(&req.text);
            if parts.is_empty() {
                // Blank text: the server says what it thinks of it.
                parts.push(req.text.clone());
            }
            Job {
                speech_url: format!("{}{SPEECH}", self.base_url),
                voices_url: format!("{}{VOICES}", self.base_url),
                parse_voices: voice_list,
                voice: req.voice,
                body,
                fit,
                sentences: parts,
            }
            .start()
            .await
        })
    }

    /// `language` is not sent: one voice speaks every language (x-vector mode clones the
    /// speaker, not the language).
    fn enroll<'a>(
        &'a self,
        name: &'a str,
        wav: &'a Path,
        language: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Enrolled, TtsError>> {
        Box::pin(async move {
            check_voice_name_chars(name)?;
            if let Some(code) = language {
                tracing::debug!(voice = %name, language = %code, "Qwen3-TTS voices speak every language; the enrollment language is not sent");
            }
            let clip = std::fs::read(wav).map_err(|e| TtsError::VoiceRead {
                path: wav.display().to_string(),
                detail: e.to_string(),
            })?;
            let (base, name) = (self.base_url.clone(), name.to_owned());
            tokio::task::spawn_blocking(move || enroll_job(&base, &name, clip))
                .await
                .map_err(|e| TtsError::Worker(format!("enroll task join: {e}")))?
        })
    }

    fn voices<'a>(&'a self) -> BoxFuture<'a, Result<Option<Vec<Voice>>, TtsError>> {
        Box::pin(async move {
            let url = format!("{}{VOICES}", self.base_url);
            tokio::task::spawn_blocking(move || {
                list_voices(&blocking_client(&url)?, &url, voice_list)
            })
            .await
            .map_err(|e| TtsError::Worker(format!("voices task join: {e}")))?
        })
    }

    fn cross_lingual(&self) -> bool {
        true
    }

    fn languages(&self) -> Option<&'static [&'static str]> {
        Some(&CODES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{MockServer, Reply, closed_port_url, serve};

    /// What vLLM-Omni streams: one header with both lengths `0xFFFFFFFF`, 16-bit mono 24 kHz.
    fn streamed_wav(secs: f32) -> Vec<u8> {
        let mut wav = crate::decode::sine_wav(24_000, &[0.5], 220.0, secs);
        wav[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        wav[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        wav
    }

    fn uploaded(names: &[&str]) -> Reply {
        let entries: Vec<String> = names
            .iter()
            .map(|n| {
                format!(
                    r#"{{"name":"{n}","consent":"dmn-self","created_at":1,"file_size":1,"mime_type":"audio/wav","embedding_source":"audio","embedding_dim":null}}"#
                )
            })
            .collect();
        Reply::json(&format!(
            r#"{{"voices":["default"],"uploaded_voices":[{}]}}"#,
            entries.join(",")
        ))
    }

    fn body_json(server: &MockServer, i: usize) -> serde_json::Value {
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[i]);
        serde_json::from_slice(body).unwrap()
    }

    fn drain(stream: PcmStream) -> Vec<f32> {
        stream.flat_map(|c| c.unwrap().samples).collect()
    }

    #[test]
    fn speech_body_asks_for_the_cloned_voice_streamed_in_x_vector_mode() {
        let p = Qwen3Provider::new("http://localhost:4124/");
        assert_eq!(p.base_url, "http://localhost:4124");
        let req = SpeakRequest::new("你好。", "voice")
            .with_language("zh")
            .with_tuning(crate::Tuning {
                temperature: Some(0.6),
                cfg_weight: Some(0.3),
                exaggeration: None,
            });
        assert_eq!(
            p.build_speech_body(&req).unwrap(),
            serde_json::json!({
                "model": "Qwen/Qwen3-TTS-12Hz-1.7B-Base",
                "input": "你好。",
                "voice": "voice",
                "task_type": "Base",
                "x_vector_only_mode": true,
                "response_format": "wav",
                "stream_format": "audio",
                "initial_codec_chunk_frames": 10,
                "language": "Chinese",
            })
        );
        for (code, name) in [("ru", "Russian"), ("de", "German")] {
            let req = SpeakRequest::new("x", "voice").with_language(code);
            assert_eq!(p.build_speech_body(&req).unwrap()["language"], name);
        }
        let auto = p.build_speech_body(&SpeakRequest::new("x", "v")).unwrap();
        assert!(auto.get("language").is_none(), "{auto}");
    }

    #[tokio::test]
    async fn a_language_the_model_does_not_speak_is_refused_before_any_request() {
        let url = closed_port_url().unwrap();
        let p = Qwen3Provider::new(&url);
        let err = p
            .synthesize(SpeakRequest::new("Merhaba.", "voice").with_language("tr"))
            .await
            .expect_err("Turkish is not one of its languages");
        assert!(matches!(err, TtsError::Request(_)), "{err}");
        assert!(err.to_string().contains("\"tr\""), "{err}");
        assert!(err.to_string().contains("zh, en"), "{err}");
    }

    #[test]
    fn the_token_cap_grows_with_the_sentence_and_bounds_a_runaway() {
        assert_eq!(token_cap(""), 38);
        // 20 Chinese characters, ~4-5 s of speech: capped at 13 s.
        let zh = "我们明天上午十点在会议室见面讨论项目进度吧";
        assert_eq!(token_cap(zh), ((3.0 + 0.5 * 21.0) * 12.5_f64).ceil() as u64);
        assert!(token_cap(zh) < 2048 / 8);
        assert!(token_cap(&"a".repeat(200)) > token_cap(&"a".repeat(100)));
    }

    /// One request per sentence, each with its own token cap, the WAV of unknown length decoded
    /// as it streams in odd-sized pieces.
    #[tokio::test]
    async fn synthesis_checks_the_voice_then_streams_each_sentence() {
        let server = serve(vec![
            uploaded(&["voice"]),
            Reply::wav(streamed_wav(0.25)).in_pieces_of(13).chunked(),
            Reply::wav(streamed_wav(0.25)).in_pieces_of(13).chunked(),
        ])
        .unwrap();
        let p = Qwen3Provider::new(&server.url);
        let text = "Привет, как у тебя дела сегодня? У меня всё хорошо, спасибо.";
        let stream = p
            .synthesize(SpeakRequest::new(text, "voice").with_language("ru"))
            .await
            .unwrap();
        assert_eq!(stream.sample_rate(), 24_000);
        assert_eq!(drain(stream).len(), 12_000);
        assert_eq!(
            server.request_lines(),
            [
                "GET /v1/audio/voices",
                "POST /v1/audio/speech",
                "POST /v1/audio/speech"
            ]
        );
        let (first, second) = (body_json(&server, 1), body_json(&server, 2));
        assert_eq!(first["input"], "Привет, как у тебя дела сегодня?");
        assert_eq!(second["input"], "У меня всё хорошо, спасибо.");
        assert_eq!(first["language"], "Russian");
        assert_eq!(first["x_vector_only_mode"], true);
        assert_eq!(
            first["max_new_tokens"],
            token_cap("Привет, как у тебя дела сегодня?")
        );
        assert_eq!(
            second["max_new_tokens"],
            token_cap("У меня всё хорошо, спасибо.")
        );
    }

    #[tokio::test]
    async fn a_voice_that_was_never_uploaded_is_refused_and_built_ins_do_not_count() {
        let server = serve(vec![uploaded(&["alice"])]).unwrap();
        let p = Qwen3Provider::new(&server.url);
        let err = p
            .synthesize(SpeakRequest::new("Hallo.", "default").with_language("de"))
            .await
            .expect_err("a built-in speaker is someone else's voice");
        assert!(matches!(err, TtsError::UnknownVoice { .. }), "{err}");
        assert!(err.to_string().contains("alice"), "{err}");
        assert_eq!(server.request_lines(), ["GET /v1/audio/voices"]);
    }

    #[tokio::test]
    async fn the_servers_error_text_comes_back() {
        let server = serve(vec![
            uploaded(&["voice"]),
            Reply::new(
                "400 Bad Request",
                "application/json",
                r#"{"error":{"message":"Invalid language 'Klingon'.","type":"BadRequestError","code":400}}"#,
            ),
        ])
        .unwrap();
        let p = Qwen3Provider::new(&server.url);
        let err = p
            .synthesize(SpeakRequest::new("x", "voice"))
            .await
            .expect_err("must fail");
        match err {
            TtsError::HttpStatus { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, "Invalid language 'Klingon'.");
            }
            other => panic!("{other}"),
        }
    }

    #[tokio::test]
    async fn voices_lists_the_uploaded_ones_with_no_language_and_their_lowercase_names() {
        let server = serve(vec![uploaded(&["voice", "Voice-RU"])]).unwrap();
        let p = Qwen3Provider::new(&server.url);
        let voices = p.voices().await.unwrap().unwrap();
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].name, "voice");
        assert!(voices.iter().all(|v| v.language.is_none()));
        assert!(voices[1].answers_to("voice-ru"));
        assert!(p.cross_lingual());
        let langs = p.languages().unwrap();
        assert!(["zh", "ru", "de"].iter().all(|c| langs.contains(c)));
    }

    fn clip() -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-voice.wav");
        let bytes = crate::decode::sine_wav(24_000, &[0.5], 220.0, 1.5);
        std::fs::write(&path, &bytes).unwrap();
        (dir, path, bytes)
    }

    #[tokio::test]
    async fn enroll_uploads_the_clip_with_a_name_and_consent_and_no_language() {
        let server = serve(vec![
            uploaded(&[]),
            Reply::json(
                r#"{"success":true,"voice":{"name":"voice","consent":"dmn-self","created_at":1,"mime_type":"audio/wav","file_size":72044}}"#,
            ),
        ])
        .unwrap();
        let (_dir, path, bytes) = clip();
        let p = Qwen3Provider::new(&server.url);
        let enrolled = p.enroll("voice", &path, Some("zh")).await.unwrap();
        assert_eq!(
            enrolled,
            Enrolled {
                name: "voice".into(),
                replaced: false
            }
        );
        assert_eq!(
            server.request_lines(),
            ["GET /v1/audio/voices", "POST /v1/audio/voices"]
        );
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[1]);
        let text = String::from_utf8_lossy(body);
        assert!(text.contains("name=\"name\"\r\n\r\nvoice\r\n"), "{text}");
        assert!(
            text.contains("name=\"consent\"\r\n\r\ndmn-self\r\n"),
            "{text}"
        );
        assert!(
            text.contains("name=\"audio_sample\"; filename=\"voice.wav\""),
            "{text}"
        );
        assert!(!text.contains("language"), "{text}");
        assert!(!text.contains("ref_text"), "x-vector mode: no transcript");
        assert!(body.windows(bytes.len()).any(|w| w == bytes));
    }

    #[tokio::test]
    async fn re_enrolling_deletes_the_old_voice_first() {
        let server = serve(vec![
            uploaded(&["Voice"]),
            Reply::json(r#"{"success":true,"message":"Voice 'voice' deleted successfully"}"#),
            Reply::json(r#"{"success":true,"voice":{"name":"voice"}}"#),
        ])
        .unwrap();
        let (_dir, path, _) = clip();
        let p = Qwen3Provider::new(&server.url);
        let enrolled = p.enroll("voice", &path, None).await.unwrap();
        assert!(enrolled.replaced);
        assert_eq!(
            server.request_lines(),
            [
                "GET /v1/audio/voices",
                "DELETE /v1/audio/voices/voice",
                "POST /v1/audio/voices"
            ]
        );
    }

    #[tokio::test]
    async fn enroll_maps_the_servers_refusal_and_an_unreachable_server() {
        let server = serve(vec![
            uploaded(&[]),
            Reply::new(
                "400 Bad Request",
                "application/json",
                r#"{"error":{"message":"Reference audio too long (45.0s). Maximum 30s supported — use a shorter clip.","type":"BadRequestError","code":400}}"#,
            ),
        ])
        .unwrap();
        let (_dir, path, _) = clip();
        let err = Qwen3Provider::new(&server.url)
            .enroll("voice", &path, None)
            .await
            .expect_err("too long");
        assert!(err.to_string().contains("Maximum 30s"), "{err}");

        let url = closed_port_url().unwrap();
        let err = Qwen3Provider::new(&url)
            .enroll("voice", &path, None)
            .await
            .expect_err("nothing there");
        assert!(matches!(err, TtsError::Unreachable { .. }), "{err}");
    }
}
