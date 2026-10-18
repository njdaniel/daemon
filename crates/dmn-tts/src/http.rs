//! What the two server clients share (#122): the blocking HTTP client and its errors, the
//! voice check before a synthesis, the decoder thread that asks for one sentence at a time and
//! streams each WAV body through the trailing-silence cut, and the hand-built multipart form.
//!
//! The blocking client is built inside the decoder thread / the enroll task, never held on a
//! provider: a `reqwest::blocking::Client` embeds its own tokio runtime, and dropping one from
//! an async context panics, while a provider dropped at the end of a request future would do
//! exactly that.

use std::io::Read;
use std::time::Duration;

use crate::provider::{PcmChunk, PcmStream, TtsError, Voice};
use crate::tail::decode_cut;

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
pub(crate) const MESSAGE_CHARS: usize = 300;
/// How much of a voice listing or an enrollment answer is read.
const LISTING_CAP: u64 = 4 * 1024 * 1024;

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

pub(crate) fn blocking_client(url: &str) -> Result<reqwest::blocking::Client, TtsError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| TtsError::Unreachable {
            url: url.to_owned(),
            detail: format!("cannot build the HTTP client: {}", error_chain(&e)),
        })
}

pub(crate) fn unreachable(url: &str, e: &reqwest::Error) -> TtsError {
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

/// FastAPI's `{"detail": {"error": {"message"}}}` and `{"detail": "..."}`, a validation error's
/// `{"detail": [{"msg"}]}`, OpenAI's `{"error": {"message"}}` (vLLM's too), or `{"message"}`.
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
pub(crate) fn status_error(resp: reqwest::blocking::Response) -> TtsError {
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    let _ = resp.take(ERROR_BODY_CAP).read_to_end(&mut bytes);
    TtsError::HttpStatus {
        status,
        message: server_message(&bytes),
    }
}

/// A 2xx body, up to [`LISTING_CAP`].
pub(crate) fn read_body(resp: reqwest::blocking::Response) -> Result<Vec<u8>, TtsError> {
    let mut body = Vec::new();
    resp.take(LISTING_CAP)
        .read_to_end(&mut body)
        .map_err(|e| TtsError::BodyRead {
            detail: error_chain(&e),
        })?;
    Ok(body)
}

/// `name` as one URL path segment.
pub(crate) fn path_segment(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// How a server's voice listing is read: `None` when the body is not that server's shape.
pub(crate) type ParseVoices = fn(&[u8]) -> Option<Vec<Voice>>;

/// `GET url`, the server's voice library. `Ok(None)` when the server has no voice library (an
/// error status: another OpenAI-compatible server) or answers with something that is not a
/// listing.
pub(crate) fn list_voices(
    client: &reqwest::blocking::Client,
    url: &str,
    parse: ParseVoices,
) -> Result<Option<Vec<Voice>>, TtsError> {
    let resp = client.get(url).send().map_err(|e| unreachable(url, &e))?;
    let status = resp.status();
    if !status.is_success() {
        tracing::warn!(status = status.as_u16(), url = %url, "cannot list the TTS server's voices");
        return Ok(None);
    }
    let voices = parse(&read_body(resp)?);
    if voices.is_none() {
        tracing::warn!(url = %url, "the TTS server's voice listing is not the expected shape");
    }
    Ok(voices)
}

/// Refuse a voice the server's library does not have (chatterbox-tts-api would speak in its
/// default voice). A server with no library to list is trusted as is.
pub(crate) fn check_voice(
    client: &reqwest::blocking::Client,
    url: &str,
    parse: ParseVoices,
    voice: &str,
) -> Result<(), TtsError> {
    let Some(voices) = list_voices(client, url, parse)? else {
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

/// Puts one sentence into the speech request body: its `input`, and whatever else depends on it.
pub(crate) type Fit = fn(&mut serde_json::Value, &str);

/// The [`Fit`] that sets `input` and nothing else.
pub(crate) fn set_input(body: &mut serde_json::Value, sentence: &str) {
    body["input"] = sentence.into();
}

/// One synthesis as the decoder thread runs it: the voice check, then one POST of `body` per
/// sentence, fitted to it by `fit`.
pub(crate) struct Job {
    /// The speech route, absolute.
    pub speech_url: String,
    /// The voice listing route, absolute, and how to read it.
    pub voices_url: String,
    pub parse_voices: ParseVoices,
    pub voice: String,
    pub body: serde_json::Value,
    pub fit: Fit,
    pub sentences: Vec<String>,
}

impl Job {
    /// Start the decoder thread and resolve once the first WAV header has been read: every
    /// failure before it (the voice check, a refused connection, an error status, a body that
    /// is not a WAV) is this future's `Err`.
    pub(crate) async fn start(self) -> Result<PcmStream, TtsError> {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (chunk_tx, chunk_rx) = crossbeam_channel::bounded(CHANNEL_CAP);
        std::thread::Builder::new()
            .name("dmn-tts-decode".to_owned())
            .spawn(move || self.run(ready_tx, chunk_tx))
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
    }

    /// The decoder thread: check the voice, then for each sentence (#113) POST the speech
    /// request and decode its WAV body chunk by chunk, through the trailing-silence cut, into
    /// `chunks`. Every failure up to the first WAV header goes out on `ready` (so the start
    /// future fails before returning); a later one is one `Err` chunk, and the end.
    fn run(
        self,
        ready: crossbeam_channel::Sender<Result<u32, TtsError>>,
        chunks: crossbeam_channel::Sender<Result<PcmChunk, TtsError>>,
    ) {
        let ready = std::cell::RefCell::new(Some(ready));
        let failed = std::cell::Cell::new(false);
        // Before the first header the failure is the start future's; after it, the stream's
        // last item.
        let fail = |e: TtsError| {
            failed.set(true);
            match ready.borrow_mut().take() {
                Some(tx) => {
                    let _ = tx.send(Err(e));
                }
                None => {
                    let _ = chunks.send(Err(e));
                }
            }
        };
        let url = self.speech_url;
        let client = match blocking_client(&url) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        if let Err(e) = check_voice(&client, &self.voices_url, self.parse_voices, &self.voice) {
            return fail(e);
        }
        let count = self.sentences.len();
        for (i, sentence) in self.sentences.into_iter().enumerate() {
            let mut body = self.body.clone();
            (self.fit)(&mut body, &sentence);
            let resp = match client.post(&url).json(&body).send() {
                Ok(r) => r,
                Err(e) => return fail(unreachable(&url, &e)),
            };
            if !resp.status().is_success() {
                return fail(status_error(resp));
            }
            tracing::debug!(url = %url, voice = %self.voice, sentence = i + 1, of = count, "tts answer arriving");
            let gone = std::cell::Cell::new(false);
            let cut = decode_cut(
                resp,
                |rate| match rate {
                    Ok(rate) => {
                        if let Some(tx) = ready.borrow_mut().take() {
                            let _ = tx.send(Ok(rate));
                        }
                    }
                    Err(e) => fail(e),
                },
                // A full channel blocks here until the caller drains it: that is the
                // backpressure. A send to a dropped stream fails, and the decoder stops reading.
                |chunk| {
                    let failure = chunk.is_err();
                    let sent = chunks.send(chunk).is_ok();
                    gone.set(gone.get() || !sent);
                    failed.set(failed.get() || failure);
                    sent
                },
            );
            if cut {
                tracing::debug!(sentence = i + 1, "tts: cut at the trailing silence");
            }
            if gone.get() || failed.get() {
                return;
            }
        }
    }
}

/// A `multipart/form-data` body of text `fields` and one WAV `file` (its field name and the
/// file name sent), built by hand rather than through reqwest's `multipart` feature, which
/// would add `mime_guess` and `unicase`. Returns the content type and the body.
pub(crate) fn multipart_form(
    fields: &[(&str, &str)],
    file: (&str, &str),
    clip: &[u8],
) -> (String, Vec<u8>) {
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
    for (key, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let (field, file_name) = file;
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{file_name}\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(clip);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The longest voice name sent (chatterbox-tts-api's own limit, `voices.py`: `max_length=100`).
const MAX_VOICE_NAME: usize = 100;

/// A voice name both servers can store and a URL path can carry: chatterbox-tts-api refuses
/// `/ \ : * ? " < > |` and more than 100 characters, vLLM-Omni path separators and NUL; control
/// characters would break the multipart form.
pub(crate) fn check_voice_name_chars(name: &str) -> Result<(), TtsError> {
    let bad = |c: char| c.is_control() || "/\\:*?\"<>|".contains(c);
    if name.trim().is_empty() || name.chars().count() > MAX_VOICE_NAME || name.contains(bad) {
        return Err(TtsError::Request(format!(
            "voice name {name:?} cannot be stored by the TTS server (1-{MAX_VOICE_NAME} characters, none of / \\ : * ? \" < > |)"
        )));
    }
    Ok(())
}

/// `name` as a file name the server stores: ASCII letters, digits and `-_.`, the rest `_`.
pub(crate) fn file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}
