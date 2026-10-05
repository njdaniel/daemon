//! [`MockTtsProvider`]: synthesizes a generated sine WAV and records every request, mirroring
//! `dmn-llm`'s `MockProvider`, so this crate's consumers and `dmn speak` need no live server.

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;

use crate::decode::decode_reader;
use crate::provider::{PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider};

#[derive(Debug)]
pub struct MockTtsProvider {
    /// The WAV synthesized on every request; a sine by default.
    wav: Vec<u8>,
    requests: Arc<Mutex<Vec<SpeakRequest>>>,
    enrollments: Arc<Mutex<Vec<(String, usize)>>>,
}

impl Default for MockTtsProvider {
    /// A 220 Hz sine at 24 kHz, half a second: the shape of a real server's answer without
    /// sounding like speech to anyone listening to a test.
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

    /// Every enrollment: the name and the clip's size in bytes, oldest first.
    pub fn enrollments(&self) -> Vec<(String, usize)> {
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
            let (rate_tx, rate_rx) = crossbeam_channel::bounded(1);
            let mut chunks: Vec<Result<PcmChunk, TtsError>> = Vec::new();
            decode_reader(
                std::io::Cursor::new(self.wav.clone()),
                |rate| {
                    let _ = rate_tx.send(rate);
                },
                |chunk| chunks.push(chunk),
            );
            let sample_rate = match rate_rx.recv() {
                Ok(Ok(rate)) => rate,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(TtsError::Worker(
                        "mock decode produced no header verdict".into(),
                    ));
                }
            };
            // Pre-sized so the whole stream is queued without a feeder thread.
            let (tx, rx) = crossbeam_channel::bounded(chunks.len().max(1));
            for chunk in chunks {
                let _ = tx.send(chunk);
            }
            Ok(PcmStream::new(rx, sample_rate))
        })
    }

    fn enroll<'a>(&'a self, name: &'a str, wav: &'a Path) -> BoxFuture<'a, Result<String, TtsError>> {
        Box::pin(async move {
            let bytes = std::fs::read(wav).map_err(|e| TtsError::VoiceRead {
                path: wav.display().to_string(),
                detail: e.to_string(),
            })?;
            if let Ok(mut e) = self.enrollments.lock() {
                e.push((name.to_owned(), bytes.len()));
            }
            Ok(name.to_owned())
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
        let samples: usize = stream
            .map(|c| c.unwrap().samples.len())
            .sum();
        assert_eq!(samples, 12_000, "0.5 s at 24 kHz");
        assert_eq!(mock.requests(), vec![req]);
    }

    #[tokio::test]
    async fn enroll_records_name_and_clip_size() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("v.wav");
        let bytes = crate::decode::sine_wav(16_000, &[0.5], 300.0, 0.25);
        std::fs::write(&clip, &bytes).unwrap();
        let mock = MockTtsProvider::new();
        let name = mock.enroll("narrator", &clip).await.unwrap();
        assert_eq!(name, "narrator");
        assert_eq!(
            mock.enrollments(),
            vec![("narrator".to_owned(), bytes.len())]
        );
        let err = mock
            .enroll("x", &dir.path().join("gone.wav"))
            .await
            .expect_err("must fail");
        assert!(matches!(err, TtsError::VoiceRead { .. }), "{err}");
    }
}
