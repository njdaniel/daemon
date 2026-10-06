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
    Language, Model, SttConfig, SttWorker, SttWorkerConfig, Transcriber, model_path,
    word_error_rate,
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
    transcribe_lane_in(wav_path, source, model, Language::English)
}

/// [`transcribe_lane`] under a language mode (#77).
fn transcribe_lane_in(
    wav_path: &Path,
    source: Source,
    model: &Path,
    language: Language,
) -> Vec<Segment> {
    let mut wav = WavSource::open(wav_path, source, Speed::Fast).expect("open fixture");
    // Unpaced: the ring must hold the whole file or the producer outruns the VAD.
    let ring = (wav.duration().as_millis() / u128::from(FRAME_MS)) as usize + 100;
    let (sink, frames) = frame_channel(source, ring);
    let (tx, rx) = event_channel(512);

    let transcriber = Transcriber::load(
        model,
        SttConfig {
            threads: 8,
            language,
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
#[ignore = "needs the es fixture WAVs (just fixtures-fetch) and the multilingual model (dmn doctor --fetch small)"]
fn the_spanish_interviewer_lane_arrives_in_english_with_es_detected() {
    // #77: `language = "auto"` with the multilingual `small`: every final names the language
    // whisper heard and its text is English, matched against the script's English reference.
    let fixture = testdata().join("es");
    let wav = fixture.join("interviewer.wav");
    let model = model_path(&models_dir(), Model::Small);
    assert!(
        wav.is_file(),
        "missing {}: run just fixtures-fetch",
        wav.display()
    );
    assert!(
        model.is_file(),
        "missing {}: run dmn doctor --fetch small",
        model.display()
    );
    let questions: Vec<serde_json::Value> =
        serde_json::from_reader(std::fs::File::open(fixture.join("questions.json")).expect("open"))
            .expect("parse questions.json");
    let segments = transcribe_lane_in(&wav, Source::Interviewer, &model, Language::Auto);
    let finals: Vec<&Segment> = segments.iter().filter(|s| !s.text.is_empty()).collect();
    assert!(!finals.is_empty(), "the lane produced no transcript");
    let es = finals
        .iter()
        .filter(|s| s.language.as_deref() == Some("es"))
        .count();
    assert!(
        es * 10 >= finals.len() * 9,
        "only {es} of {} finals were detected as Spanish: {:?}",
        finals.len(),
        finals
            .iter()
            .map(|s| (s.language.clone(), s.text.clone()))
            .collect::<Vec<_>>()
    );
    // The English reference of each question should surface in some transcript line: the
    // bar is lower than the English fixtures' because the text is whisper's translation.
    let mut matched = 0usize;
    for q in &questions {
        let text = q["text"].as_str().expect("question text");
        if finals.iter().any(|s| overlap(text, &s.text) >= 0.4) {
            matched += 1;
        } else {
            println!("no line matched: {text}");
        }
    }
    println!(
        "matched {matched}/{} questions; {es}/{} finals detected as es",
        questions.len(),
        finals.len()
    );
    assert!(
        matched * 10 >= questions.len() * 8,
        "only {matched} of {} scripted questions were found in translation",
        questions.len()
    );
    assert!(
        finals
            .iter()
            .all(|s| s.render().contains("(es→en)") || s.language.as_deref() != Some("es")),
        "a Spanish final is tagged in its transcript line"
    );
}

/// #109: `Language::Detect`, the You lane's mode while `[speak]` is on, detects the language as
/// `auto` does but keeps the text in it: the Spanish lane arrives in Spanish, not translated,
/// where the test above gets English from the same audio under `auto`.
#[test]
#[ignore = "needs the es fixture WAVs (just fixtures-fetch) and the multilingual model (dmn doctor --fetch small)"]
fn under_detect_the_spanish_lane_keeps_its_spanish() {
    let fixture = testdata().join("es");
    let wav = fixture.join("interviewer.wav");
    let model = model_path(&models_dir(), Model::Small);
    assert!(wav.is_file() && model.is_file(), "fixture or model missing");
    let segments = transcribe_lane_in(&wav, Source::Interviewer, &model, Language::Detect);
    let finals: Vec<&Segment> = segments.iter().filter(|s| !s.text.is_empty()).collect();
    assert!(!finals.is_empty(), "the lane produced no transcript");
    // Words that run through Spanish and not English: their share of every final's words.
    const SPANISH: [&str; 12] = [
        "de", "que", "el", "la", "en", "los", "las", "una", "por", "cómo", "qué", "es",
    ];
    let words: Vec<String> = finals
        .iter()
        .flat_map(|s| s.text.split_whitespace())
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .collect();
    let spanish = words
        .iter()
        .filter(|w| SPANISH.contains(&w.as_str()))
        .count();
    let english = words
        .iter()
        .filter(|w| ["the", "you", "how", "would", "what", "and"].contains(&w.as_str()))
        .count();
    for s in finals.iter().take(5) {
        println!("{:?}: {}", s.language, s.text);
    }
    println!(
        "{} finals, {spanish} Spanish and {english} English marker words of {}",
        finals.len(),
        words.len()
    );
    assert!(
        spanish * 100 >= words.len() * 15 && english * 100 < words.len() * 2,
        "the text is not Spanish: {spanish} Spanish and {english} English markers in {} words",
        words.len()
    );
    let es = finals
        .iter()
        .filter(|s| s.language.as_deref() == Some("es"))
        .count();
    assert!(
        es * 10 >= finals.len() * 9,
        "detected as es: {es} of {}",
        finals.len()
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

#[test]
#[ignore = "needs the es fixture WAVs (just fixtures-fetch) and the large-v3-turbo model (dmn doctor --fetch large-v3-turbo)"]
fn large_v3_turbo_under_the_translate_task_keeps_the_spanish() {
    // #85: PR #78 refuses large-v3-turbo for `[stt].language` other than `en` because, given
    // whisper's translate task on the Spanish fixture, it returned Spanish (2026-09-28). That
    // run used a model file later found corrupt (#84), so this re-runs it on a verified file,
    // straight through the transcriber (the guard lives in its callers). Each final is scored
    // against the scripted interviewer turns in English and in the Spanish that was spoken; the
    // refusal stands while most finals read as Spanish.
    let fixture = testdata().join("es");
    let wav = fixture.join("interviewer.wav");
    let model = model_path(&models_dir(), Model::LargeV3Turbo);
    assert!(
        wav.is_file(),
        "missing {}: run just fixtures-fetch",
        wav.display()
    );
    assert!(
        model.is_file(),
        "missing {}: run dmn doctor --fetch large-v3-turbo",
        model.display()
    );
    let turns: Vec<serde_json::Value> =
        serde_json::from_reader(std::fs::File::open(fixture.join("turns.json")).expect("open"))
            .expect("parse turns.json");
    let interviewer: Vec<(&str, &str)> = turns
        .iter()
        .filter(|t| t["speaker"] == "interviewer")
        .filter_map(|t| Some((t["text"].as_str()?, t["spoken"].as_str()?)))
        .collect();
    // `Transcriber::load` refuses a file named `*turbo*` under the translate task (the backstop
    // for the same refusal); a neutral name for the same verified bytes gets past it, which is
    // the point of this measurement.
    let dir = tempfile::tempdir().expect("tempdir");
    let neutral = dir.path().join("ggml-measured.bin");
    std::os::unix::fs::symlink(&model, &neutral).expect("symlink");
    let segments = transcribe_lane_in(&wav, Source::Interviewer, &neutral, Language::Auto);
    // `transcribe_lane_in` runs with partials off, so these are all finals already; the filter
    // says so rather than relying on it.
    let finals: Vec<&Segment> = segments
        .iter()
        .filter(|s| s.is_final && !s.text.is_empty())
        .collect();
    assert!(!finals.is_empty(), "the lane produced no transcript");
    // The best overlap of `text` with any interviewer turn, as scripted in English or as
    // spoken in Spanish.
    let best = |text: &str, spanish: bool| {
        interviewer
            .iter()
            .map(|&(en, es)| overlap(if spanish { es } else { en }, text))
            .fold(0.0_f64, f64::max)
    };
    let mut spanish = 0usize;
    for (i, s) in finals.iter().enumerate() {
        let en = best(&s.text, false);
        let es = best(&s.text, true);
        if es > en {
            spanish += 1;
        }
        if i < 3 {
            println!("{:?} en {en:.2} es {es:.2}: {}", s.language, s.text);
        }
    }
    println!("{spanish} of {} finals read as Spanish", finals.len());
    assert!(
        spanish * 2 > finals.len(),
        "large-v3-turbo translated {} of {} finals to English: the refusal in Model::translates \
         (#78) no longer holds on a verified file",
        finals.len() - spanish,
        finals.len()
    );
}
