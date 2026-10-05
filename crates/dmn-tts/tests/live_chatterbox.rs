//! #107 against a real chatterbox-tts-api, by hand (results in the PR). Needs the server on
//! `DMN_TTS_URL` (default `http://localhost:4123`); it says so and stops when none answers:
//!
//! `cargo test -p dmn-tts --test live_chatterbox -- --ignored --nocapture`
//!
//! It enrolls a voice named `dmn-live-test` from a generated 8 s clip, enrolls it again (the
//! 409 → DELETE → POST path), synthesizes one Spanish sentence through `/v1/audio/speech` and
//! through `/v1/audio/speech/stream`, checks a name the library lacks is refused, and deletes
//! the voice at the end (`DELETE /v1/voices/dmn-live-test`), so the server's library is left as
//! it was. A sine is not speech: the clone sounds like nobody; this checks the API, not the voice.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use dmn_tts::{ChatterboxProvider, SpeakRequest, TtsError, TtsProvider};

const VOICE: &str = "dmn-live-test";
const TEXT: &str = "Buenas tardes, gracias por la oportunidad.";

fn url() -> String {
    std::env::var("DMN_TTS_URL").unwrap_or_else(|_| "http://localhost:4123".into())
}

/// 8 s of a gliding tone with a syllable-like envelope, mono 16 kHz.
fn clip(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..16_000 * 8 {
        let t = i as f32 / 16_000.0;
        let env = (std::f32::consts::PI * 4.0 * t).sin().abs();
        let f = 140.0 + 40.0 * (t * 1.3).sin();
        let v = 0.3 * env * (std::f32::consts::TAU * f * t).sin();
        w.write_sample((v * f32::from(i16::MAX)) as i16).unwrap();
    }
    w.finalize().unwrap();
}

fn delete_voice(base: &str) {
    let resp = reqwest::blocking::Client::new()
        .delete(format!("{base}/v1/voices/{VOICE}"))
        .send();
    println!(
        "cleanup: DELETE /v1/voices/{VOICE} -> {:?}",
        resp.map(|r| r.status().as_u16())
    );
}

async fn speak(p: &ChatterboxProvider, label: &str) {
    let started = Instant::now();
    let stream = p
        .synthesize(SpeakRequest::new(TEXT, VOICE).with_language("es"))
        .await
        .unwrap();
    let header_at = started.elapsed();
    let rate = stream.sample_rate();
    let mut first_chunk = None;
    let mut frames = 0usize;
    let mut sum_sq = 0f64;
    for chunk in stream {
        let chunk = chunk.unwrap();
        first_chunk.get_or_insert(started.elapsed());
        frames += chunk.samples.len();
        sum_sq += chunk
            .samples
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>();
    }
    let total = started.elapsed();
    let secs = frames as f64 / f64::from(rate);
    let rms = (sum_sq / frames.max(1) as f64).sqrt();
    println!(
        "{label}: {secs:.2} s of audio at {rate} Hz, rms {rms:.3}; header after {:.1} s, first chunk after {:.1} s, done after {:.1} s ({:.1}x real time)",
        header_at.as_secs_f64(),
        first_chunk.unwrap_or_default().as_secs_f64(),
        total.as_secs_f64(),
        total.as_secs_f64() / secs.max(0.001)
    );
    assert!((1.0..15.0).contains(&secs), "{secs} s for one sentence");
    assert!(rms > 0.005, "audible: rms {rms}");
}

#[tokio::test]
#[ignore = "needs chatterbox-tts-api on DMN_TTS_URL (default localhost:4123)"]
async fn chatterbox_live_enroll_speak_and_stream() {
    let base = url();
    if reqwest::get(format!("{base}/health")).await.is_err() {
        println!("no TTS server answers at {base}; nothing checked");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("clip.wav");
    clip(&wav);
    let p = ChatterboxProvider::new(&base);

    let result = async {
        let t = Instant::now();
        let first = p.enroll(VOICE, &wav, Some("es")).await.unwrap();
        println!("enroll: {first:?} in {:.2} s", t.elapsed().as_secs_f64());
        let again = p.enroll(VOICE, &wav, Some("es")).await.unwrap();
        println!("re-enroll: {again:?}");
        assert!(again.replaced, "the second enroll replaced the first");

        speak(&p, "/v1/audio/speech").await;
        speak(&p.clone().streaming(), "/v1/audio/speech/stream").await;

        let err = p
            .synthesize(SpeakRequest::new(TEXT, "dmn-no-such-voice"))
            .await
            .expect_err("an unknown voice is refused");
        assert!(matches!(err, TtsError::UnknownVoice { .. }), "{err}");
        println!("unknown voice: {err}");
    };
    // Clean up even when an assertion fails.
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    tokio::task::spawn_blocking(move || delete_voice(&base))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
