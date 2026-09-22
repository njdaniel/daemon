//! End-to-end over a fixture: WAV → VAD → whisper worker → transcript log.
//!
//! Ignored by default: it needs the fixture WAVs (`just fixtures-fetch`) and a whisper model
//! (`dmn doctor --fetch small.en`), neither of which CI has. Run it with
//! `cargo test -p dmn-stt --test pipeline -- --ignored --nocapture`.

// An integration test is its own crate, so the crate-root allowance in lib.rs does not reach it;
// CLAUDE.md permits unwrap/expect in tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dmn_audio::{AudioSource, FRAME_MS, Source, Speed, WavSource, frame_channel};
use dmn_stt::{
    Model, SttConfig, SttWorker, SttWorkerConfig, Transcriber, model_path, word_error_rate,
};
use dmn_transcript::{Segment, TranscriptLog};
use dmn_vad::{VadConfig, VadWorker, event_channel};

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

fn models_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".cache/dmn/models"),
        None => PathBuf::from("/nonexistent"),
    }
}

/// Words in common, as a fraction of the longer side: enough to pair a transcript line with the
/// scripted turn it came from without demanding an exact match.
fn overlap(a: &str, b: &str) -> f64 {
    let norm = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect::<String>()
            })
            .filter(|w| !w.is_empty())
            .collect()
    };
    let (x, y) = (norm(a), norm(b));
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    let mut pool = y.clone();
    let mut hits = 0usize;
    for w in &x {
        if let Some(i) = pool.iter().position(|p| p == w) {
            pool.remove(i);
            hits += 1;
        }
    }
    hits as f64 / x.len().max(y.len()) as f64
}

/// Run one speaker's WAV through the real pipeline and return the finals in order.
fn transcribe_lane(wav_path: &Path, source: Source, model: &Path) -> Vec<Segment> {
    let mut wav = WavSource::open(wav_path, source, Speed::Fast).expect("open fixture");
    // Unpaced: the ring must hold the whole file or the producer outruns the VAD.
    let ring = (wav.duration().as_millis() / u128::from(FRAME_MS)) as usize + 100;
    let (sink, frames) = frame_channel(source, ring);
    let (tx, rx) = event_channel(512);

    let transcriber = Transcriber::load(
        model,
        SttConfig {
            threads: 8,
            ..SttConfig::default()
        },
    )
    .expect("load model");
    let out = Arc::new(Mutex::new(Vec::new()));
    let sink_out = Arc::clone(&out);
    let stt = SttWorker::spawn(
        Box::new(transcriber),
        rx,
        Box::new(move |s: Segment| sink_out.lock().expect("lock").push(s)),
        SttWorkerConfig {
            // Partials are stale before they run when the audio is unpaced.
            partials: false,
            ..SttWorkerConfig::default()
        },
        "test",
    )
    .expect("spawn worker");

    let mut vad_cfg = VadConfig::default();
    vad_cfg.segmenter.partial_every = None;
    let vad = VadWorker::spawn(frames, &vad_cfg, tx).expect("spawn vad");
    wav.start(sink).expect("start");
    wav.wait().expect("deliver");
    let vad_stats = vad.finish().expect("vad");
    let stt_stats = stt.finish();
    assert_eq!(vad_stats.overruns, 0, "the ring held the whole file");
    assert_eq!(vad_stats.dropped_events, 0, "no utterance was dropped");
    assert_eq!(stt_stats.errors, 0, "no inference failed");
    let segments = out.lock().expect("lock").clone();
    println!(
        "{source}: {} utterances, {} finals, {} dropped, whisper {:.1}s",
        vad_stats.utterances,
        stt_stats.finals,
        stt_stats.dropped(),
        stt_stats.inference_ms as f64 / 1000.0,
    );
    segments
}

#[test]
#[ignore = "needs the fixture WAVs (just fixtures-fetch) and a whisper model (dmn doctor --fetch small.en)"]
fn the_interviewer_lane_transcribes_every_scripted_question_in_order() {
    let fixture = testdata().join("sysdesign");
    let wav = fixture.join("interviewer.wav");
    let model = model_path(&models_dir(), Model::SmallEn);
    assert!(
        wav.is_file(),
        "missing {}: run just fixtures-fetch",
        wav.display()
    );
    assert!(
        model.is_file(),
        "missing {}: run dmn doctor --fetch small.en",
        model.display()
    );

    let questions: Vec<serde_json::Value> =
        serde_json::from_reader(std::fs::File::open(fixture.join("questions.json")).expect("open"))
            .expect("parse questions.json");
    let segments = transcribe_lane(&wav, Source::Interviewer, &model);
    assert!(!segments.is_empty(), "the lane produced no transcript");

    let mut log = TranscriptLog::new();
    for s in &segments {
        log.apply(s.clone());
    }
    let lines = log.window(Duration::MAX);

    // Every scripted question should appear, in order, as (part of) a transcript line.
    let mut cursor = 0usize;
    let mut matched = 0usize;
    let mut wers = Vec::new();
    for q in &questions {
        let text = q["text"].as_str().expect("question text");
        let t = Duration::from_secs_f64(q["t"].as_f64().expect("question t"));
        let found = lines[cursor..]
            .iter()
            .enumerate()
            .filter(|(_, l)| overlap(text, &l.text) >= 0.5)
            .max_by(|a, b| {
                overlap(text, &a.1.text)
                    .partial_cmp(&overlap(text, &b.1.text))
                    .expect("comparable")
            });
        if let Some((i, line)) = found {
            matched += 1;
            cursor += i + 1;
            wers.push(word_error_rate(text, &line.text));
            let drift = line.start.abs_diff(t);
            assert!(
                drift < Duration::from_secs(2),
                "question at {t:?} surfaced at {:?}: {}",
                line.start,
                line.text
            );
        } else {
            println!("no line matched: {text}");
        }
    }
    let mean_wer = wers.iter().sum::<f64>() / wers.len().max(1) as f64;
    println!(
        "matched {matched}/{} questions, mean WER {:.1} %",
        questions.len(),
        mean_wer * 100.0
    );
    assert!(
        matched * 10 >= questions.len() * 9,
        "only {matched} of {} scripted questions were found",
        questions.len()
    );
    // Loose: these are clean TTS voices, so anything above this is a real regression, but
    // whisper still splits long turns and drops filler.
    assert!(
        mean_wer < 0.25,
        "mean WER {mean_wer:.3} over matched questions"
    );
}

#[test]
#[ignore = "needs the fixture WAVs and a whisper model"]
fn silence_between_turns_produces_no_hallucinated_segments() {
    // The mixed fixture has 5–8 s silences; whisper's stock phrases must not survive them.
    let fixture = testdata().join("mixed");
    let wav = fixture.join("interviewer.wav");
    let model = model_path(&models_dir(), Model::SmallEn);
    assert!(
        wav.is_file() && model.is_file(),
        "fixtures or model missing"
    );

    let segments = transcribe_lane(&wav, Source::Interviewer, &model);
    for s in &segments {
        let fp: String = s
            .text
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        let fp = fp.trim();
        assert!(
            !dmn_stt::filter::HALLUCINATION_PHRASES.contains(&fp),
            "a stock hallucination reached the transcript: {s:?}"
        );
    }
}
