//! [`MockTtsProvider`]: synthesizes a generated sine WAV and records every request, mirroring
//! `dmn-llm`'s `MockProvider`, so this crate's consumers need no live server.

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use crate::decode::decode_reader;
use crate::provider::{Enrolled, PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider};

/// One enrollment the mock received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockEnrollment {
    pub name: String,
    pub language: Option<String>,
    /// The clip's size in bytes.
    pub bytes: usize,
}

#[derive(Debug)]
pub struct MockTtsProvider {
    /// The WAV synthesized on every request; a sine by default.
    wav: Vec<u8>,
    requests: Arc<Mutex<Vec<SpeakRequest>>>,
    enrollments: Arc<Mutex<Vec<MockEnrollment>>>,
}

impl Default for MockTtsProvider {
    /// A 220 Hz sine at 24 kHz (Chatterbox's rate), half a second: the shape of a real
    /// server's answer without sounding like speech to anyone listening to a test.
    fn default() -> Self {
        Self::with_wav(crate::decode::sine_wav(24_000, &[0.5], 220.0, 0.5))
    }
}

impl MockTtsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synthesize `wav` on every request instead of the default sine.
    pub fn with_wav(wav: Vec<u8>) -> Self {
        Self {
            wav,
            requests: Arc::new(Mutex::new(Vec::new())),
            enrollments: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every synthesis request received so far, oldest first.
    pub fn requests(&self) -> Vec<SpeakRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Every enrollment received so far, oldest first.
    pub fn enrollments(&self) -> Vec<MockEnrollment> {
        self.enrollments
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }
}

impl TtsProvider for MockTtsProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>> {
        Box::pin(async move {
            if let Ok(mut r) = self.requests.lock() {
                r.push(req);
            }
            let mut rate = None;
            let mut chunks: Vec<Result<PcmChunk, TtsError>> = Vec::new();
            decode_reader(
                std::io::Cursor::new(self.wav.as_slice()),
                |r| rate = Some(r),
                |chunk| {
                    chunks.push(chunk);
                    true
                },
            );
            let sample_rate = match rate {
                Some(Ok(rate)) => rate,
                Some(Err(e)) => return Err(e),
                None => {
                    return Err(TtsError::Worker(
                        "mock decode produced no header verdict".into(),
                    ));
                }
            };
            // Sized to the whole answer, so it is queued without a feeder thread.
            let (tx, rx) = crossbeam_channel::bounded(chunks.len().max(1));
            for chunk in chunks {
                let _ = tx.send(chunk);
            }
            Ok(PcmStream::new(rx, sample_rate))
        })
    }

    fn enroll<'a>(
        &'a self,
        name: &'a str,
        wav: &'a Path,
        language: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Enrolled, TtsError>> {
        Box::pin(async move {
            let bytes = std::fs::read(wav).map_err(|e| TtsError::VoiceRead {
                path: wav.display().to_string(),
                detail: e.to_string(),
            })?;
            let mut replaced = false;
            if let Ok(mut e) = self.enrollments.lock() {
                replaced = e.iter().any(|m| m.name == name);
                e.push(MockEnrollment {
                    name: name.to_owned(),
                    language: language.map(str::to_owned),
                    bytes: bytes.len(),
                });
            }
            Ok(Enrolled {
                name: name.to_owned(),
                replaced,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthesize_replays_the_sine_and_records_requests() {
        let mock = MockTtsProvider::new();
        let req = SpeakRequest::new("hola", "voice").with_language("es");
        let stream = mock.synthesize(req.clone()).await.unwrap();
        assert_eq!(stream.sample_rate(), 24_000);
        let samples: usize = stream.map(|c| c.unwrap().samples.len()).sum();
        assert_eq!(samples, 12_000, "0.5 s at 24 kHz");
        assert_eq!(mock.requests(), vec![req]);
    }

    #[tokio::test]
    async fn a_mock_with_a_broken_wav_fails_like_a_server_would() {
        let mock = MockTtsProvider::with_wav(b"nope".to_vec());
        let err = mock
            .synthesize(SpeakRequest::new("x", "voice"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, TtsError::Decode { .. }), "{err}");
    }

    #[tokio::test]
    async fn enroll_records_name_language_and_clip_size() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        let bytes = crate::decode::sine_wav(16_000, &[0.5], 300.0, 0.25);
        std::fs::write(&clip, &bytes).unwrap();
        let mock = MockTtsProvider::new();
        let first = mock.enroll("narrator", &clip, Some("es")).await.unwrap();
        assert!(!first.replaced);
        assert!(mock.enroll("narrator", &clip, None).await.unwrap().replaced);
        assert_eq!(
            mock.enrollments()[0],
            MockEnrollment {
                name: "narrator".into(),
                language: Some("es".into()),
                bytes: bytes.len()
            }
        );
        let err = mock
            .enroll("x", &dir.path().join("gone.wav"), None)
            .await
            .expect_err("must fail");
        assert!(matches!(err, TtsError::VoiceRead { .. }), "{err}");
    }
}
