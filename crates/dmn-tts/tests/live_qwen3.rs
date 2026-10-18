//! #122 against a real Qwen3-TTS on vLLM-Omni (`sidecars/qwen3-tts/`), by hand (results in the
//! PR). Needs the server on `DMN_TTS_URL` (default `http://localhost:4124`); it says so and
//! stops when none answers:
//!
//! `cargo test -p dmn-tts --test live_qwen3 -- --ignored --nocapture`
//!
//! It enrolls a voice named `dmn-live-test` without a language, enrolls it again (the
//! list → DELETE → POST path), speaks one short and one three-sentence line in each of zh, ru and
//! de with that one voice, timing the first audio, checks a name the library lacks and a
//! language the model lacks are refused, and deletes the voice at the end, so the server's
//! library is left as it was. The clip is `DMN_TTS_CLIP` when set (a real voice, so the timings
//! and the audio mean something; `DMN_TTS_OUT` then names a directory each line is written to
//! as a WAV), else a generated tone (this checks the API, not the voice).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Instant;

use dmn_tts::{Qwen3Provider, SpeakRequest, TtsError, TtsProvider};

const VOICE: &str = "dmn-live-test";

/// (code, short line, three sentences): the step 1 measurement's kind of line.
const LINES: [(&str, &str, &str); 3] = [
    (
        "zh",
        "我觉得这个方案可行。",
        "我会先用一个消息队列来缓冲请求。然后每个服务独立扩展。最后我们用缓存降低数据库的压力。",
    ),
    (
        "ru",
        "Я думаю, это хорошее решение.",
        "Сначала я бы поставил очередь сообщений перед сервисом. Потом каждый сервис масштабируется отдельно. В конце кэш снимает нагрузку с базы данных.",
    ),
    (
        "de",
        "Ich denke, das ist eine gute Lösung.",
        "Zuerst würde ich eine Nachrichtenwarteschlange vor den Dienst stellen. Dann skaliert jeder Dienst für sich. Am Ende entlastet ein Cache die Datenbank.",
    ),
];

fn url() -> String {
    std::env::var("DMN_TTS_URL").unwrap_or_else(|_| "http://localhost:4124".into())
}

/// 8 s of a gliding tone with a syllable-like envelope, mono 16 kHz.
fn tone(path: &std::path::Path) {
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
        .delete(format!("{base}/v1/audio/voices/{VOICE}"))
        .send();
    println!(
        "cleanup: DELETE /v1/audio/voices/{VOICE} -> {:?}",
        resp.map(|r| r.status().as_u16())
    );
}

/// Speak `text`, print the timings, and (unless it is the warm-up) check it is audible speech.
async fn speak(p: &Qwen3Provider, code: &str, text: &str, out: Option<PathBuf>, check: bool) {
    let started = Instant::now();
    let stream = p
        .synthesize(SpeakRequest::new(text, VOICE).with_language(code))
        .await
        .unwrap();
    let rate = stream.sample_rate();
    let mut first_chunk = None;
    let mut samples = Vec::new();
    for chunk in stream {
        let chunk = chunk.unwrap();
        first_chunk.get_or_insert(started.elapsed());
        samples.extend(chunk.samples);
    }
    let total = started.elapsed();
    let secs = samples.len() as f64 / f64::from(rate);
    let rms = (samples
        .iter()
        .map(|&v| f64::from(v) * f64::from(v))
        .sum::<f64>()
        / samples.len().max(1) as f64)
        .sqrt();
    println!(
        "{code} {:>3} chars: first audio {:.3} s, done {:.2} s, {secs:.2} s of audio (RTF {:.2}), rms {rms:.3}",
        text.chars().count(),
        first_chunk.unwrap_or_default().as_secs_f64(),
        total.as_secs_f64(),
        total.as_secs_f64() / secs.max(0.001)
    );
    if check {
        assert!((0.5..40.0).contains(&secs), "{secs} s for {text:?}");
        assert!(rms > 0.005, "audible: rms {rms}");
    }
    if let Some(path) = out {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for v in samples {
            w.write_sample((v.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
    }
}

#[tokio::test]
#[ignore = "needs Qwen3-TTS on vLLM-Omni on DMN_TTS_URL (default localhost:4124)"]
async fn qwen3_live_one_voice_speaks_zh_ru_de() {
    let base = url();
    if reqwest::get(format!("{base}/health")).await.is_err() {
        println!("no TTS server answers at {base}; nothing checked");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let wav = match std::env::var("DMN_TTS_CLIP") {
        Ok(clip) => PathBuf::from(clip),
        Err(_) => {
            let wav = dir.path().join("clip.wav");
            tone(&wav);
            wav
        }
    };
    let out_dir = std::env::var("DMN_TTS_OUT").ok().map(PathBuf::from);
    let p = Qwen3Provider::new(&base);

    let result = async {
        let t = Instant::now();
        let first = p.enroll(VOICE, &wav, None).await.unwrap();
        println!("enroll: {first:?} in {:.2} s", t.elapsed().as_secs_f64());
        let again = p.enroll(VOICE, &wav, None).await.unwrap();
        println!("re-enroll: {again:?}");
        assert!(again.replaced, "the second enroll replaced the first");

        // The first request pays for the speaker embedding and warms the graphs.
        // dmn's own warm-up line, whose audio it throws away.
        speak(&p, "en", "OK.", None, false).await;
        for (code, short, long) in LINES {
            for (kind, text) in [("short", short), ("long", long)] {
                let out = out_dir
                    .as_ref()
                    .map(|d| d.join(format!("{code}-{kind}-qwen3.wav")));
                speak(&p, code, text, out, true).await;
            }
        }

        let err = p
            .synthesize(SpeakRequest::new("Hallo.", "dmn-no-such-voice").with_language("de"))
            .await
            .expect_err("an unknown voice is refused");
        assert!(matches!(err, TtsError::UnknownVoice { .. }), "{err}");
        println!("unknown voice: {err}");
        let err = p
            .synthesize(SpeakRequest::new("Merhaba.", VOICE).with_language("tr"))
            .await
            .expect_err("Turkish is not one of its languages");
        println!("unknown language: {err}");
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
