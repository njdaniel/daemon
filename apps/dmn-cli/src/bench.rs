//! `dmn bench`: whisper real-time factor and word error rate on fixture interviewer turns.
//!
//! Loads `testdata/<fixture>/interviewer.wav` through `dmn-audio`'s `WavSource` (the same path
//! replay uses), cuts interviewer turns using `turns.json`, and transcribes them in order with
//! the previous turn's text as prompt, the way the live worker will.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dmn_audio::{AudioSource, FRAME_LEN, SAMPLE_RATE, Source, Speed, WavSource, frame_channel};
use dmn_stt::{Backend, Model, SttConfig, Transcriber, word_error_rate};

pub struct BenchArgs {
    pub model: String,
    pub fixture: String,
    pub threads: usize,
    pub turns: usize,
    /// false = whisper's fixed 30 s encoder window (`--full-window`), for comparison.
    pub trim_audio_ctx: bool,
    pub testdata: PathBuf,
    pub models_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Interviewer turns from `turns.json`: turns of 3–15 s (a typical question) in script order,
/// up to `n`; if fewer than `n` qualify, the first `n` interviewer turns of any length.
pub fn pick_turns(turns_json: &str, n: usize) -> Result<Vec<Turn>, String> {
    let v: serde_json::Value =
        serde_json::from_str(turns_json).map_err(|e| format!("turns.json: {e}"))?;
    let all: Vec<Turn> = v
        .as_array()
        .ok_or("turns.json is not an array")?
        .iter()
        .filter(|t| t["speaker"] == "interviewer")
        .filter_map(|t| {
            Some(Turn {
                start: t["t"].as_f64()?,
                end: t["end"].as_f64()?,
                text: t["text"].as_str()?.to_owned(),
            })
        })
        .collect();
    let typical: Vec<Turn> = all
        .iter()
        .filter(|t| (3.0..=15.0).contains(&(t.end - t.start)))
        .take(n)
        .cloned()
        .collect();
    Ok(if typical.len() >= n.min(all.len()) {
        typical
    } else {
        all.into_iter().take(n).collect()
    })
}

/// Whole WAV as 16 kHz mono f32, through `WavSource` at full speed into a ring big enough that
/// nothing is dropped.
pub fn load_wav(path: &Path) -> Result<Vec<f32>, String> {
    let mut src = WavSource::open(path, Source::Interviewer, Speed::Fast)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let frames_needed = (src.duration().as_secs_f64() * 1000.0 / 20.0).ceil() as usize + 16;
    let (sink, mut frames) = frame_channel(Source::Interviewer, frames_needed);
    src.start(sink).map_err(|e| e.to_string())?;
    src.wait().map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(frames_needed * FRAME_LEN);
    let mut buf = [0f32; FRAME_LEN];
    while frames.pop_frame(&mut buf) {
        out.extend_from_slice(&buf);
    }
    if frames.overruns() > 0 {
        return Err(format!(
            "{} frames dropped loading {}",
            frames.overruns(),
            path.display()
        ));
    }
    Ok(out)
}

pub struct Row {
    pub dur: Duration,
    pub wall: Duration,
    pub rtf: f64,
    pub wer: f64,
    pub low_confidence: bool,
    pub text: String,
    pub reference: String,
}

pub fn run(a: &BenchArgs) -> Result<String, Box<dyn std::error::Error>> {
    let model = Model::from_name(&a.model)?;
    let file = dmn_stt::model_path(&a.models_dir, model);
    let dir = a.testdata.join(&a.fixture);
    let turns = pick_turns(
        &std::fs::read_to_string(dir.join("turns.json"))
            .map_err(|e| format!("{}: {e}; run `just fixtures-fetch`", dir.display()))?,
        a.turns,
    )?;
    let audio = load_wav(&dir.join("interviewer.wav"))?;

    let cfg = SttConfig {
        threads: a.threads,
        trim_audio_ctx: a.trim_audio_ctx,
        ..SttConfig::default()
    };
    let t_load = Instant::now();
    let mut stt = Transcriber::load(&file, cfg)?;
    let load = t_load.elapsed();

    let sr = SAMPLE_RATE as f64;
    let mut rows = Vec::new();
    let mut prompt: Option<String> = None;
    for t in &turns {
        let a0 = ((t.start * sr) as usize).min(audio.len());
        let a1 = ((t.end * sr) as usize).min(audio.len());
        let clip = &audio[a0..a1];
        let started = Instant::now();
        let tr = stt.transcribe(clip, prompt.as_deref())?;
        let wall = started.elapsed();
        let dur = Duration::from_secs_f64(clip.len() as f64 / sr);
        rows.push(Row {
            dur,
            wall,
            rtf: wall.as_secs_f64() / dur.as_secs_f64().max(1e-9),
            wer: word_error_rate(&t.text, &tr.text),
            low_confidence: tr.low_confidence,
            text: tr.text.clone(),
            reference: t.text.clone(),
        });
        prompt = Some(tr.text);
    }
    let window = if a.trim_audio_ctx {
        "encoder window trimmed to each utterance"
    } else {
        "full 30 s encoder window"
    };
    Ok(format!(
        "{}({window})\n",
        render(&a.model, &a.fixture, a.threads, load, &rows)
    ))
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

pub fn render(model: &str, fixture: &str, threads: usize, load: Duration, rows: &[Row]) -> String {
    let mut s = format!(
        "whisper {model} on {fixture} interviewer turns, {} × {threads} threads, model load {:.2} s\n",
        Backend::compiled().name(),
        load.as_secs_f64()
    );
    s.push_str("  #    dur    wall    RTF    WER  text\n");
    for (i, r) in rows.iter().enumerate() {
        let preview: String = r.text.chars().take(60).collect();
        let more = if r.text.chars().count() > 60 {
            "…"
        } else {
            ""
        };
        s.push_str(&format!(
            "{:>3} {:>5.1}s {:>6.2}s {:>6.3} {:>5.1}%{} {preview}{more}\n",
            i + 1,
            r.dur.as_secs_f64(),
            r.wall.as_secs_f64(),
            r.rtf,
            r.wer * 100.0,
            if r.low_confidence { "!" } else { " " },
        ));
    }
    let wers: Vec<f64> = rows.iter().map(|r| r.wer).collect();
    let mean_wer = if wers.is_empty() {
        0.0
    } else {
        wers.iter().sum::<f64>() / wers.len() as f64
    };
    s.push_str(&format!(
        "summary: median RTF {:.3}, median wall {:.2} s, max wall {:.2} s, mean WER {:.1} %, backend {} × {threads} threads\n",
        median(rows.iter().map(|r| r.rtf).collect()),
        median(rows.iter().map(|r| r.wall.as_secs_f64()).collect()),
        rows.iter().map(|r| r.wall.as_secs_f64()).fold(0.0, f64::max),
        mean_wer * 100.0,
        Backend::compiled().name(),
    ));
    let wrong: Vec<(usize, &Row)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.wer > 0.0)
        .collect();
    if !wrong.is_empty() {
        s.push_str("differences:\n");
        for (i, r) in wrong {
            s.push_str(&format!(
                "{:>3} ref: {}\n    hyp: {}\n",
                i + 1,
                r.reference,
                r.text
            ));
        }
    }
    if rows.iter().any(|r| r.low_confidence) {
        s.push_str("(! = avg_logprob < -1.0, would be flagged low-confidence)\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURNS: &str = r#"[
      {"speaker":"interviewer","t":1.0,"end":2.0,"text":"Hi."},
      {"speaker":"candidate","t":2.5,"end":9.0,"text":"Hello there."},
      {"speaker":"interviewer","t":9.5,"end":16.0,"text":"How would you design a rate limiter?"},
      {"speaker":"interviewer","t":17.0,"end":40.0,"text":"A very long turn."},
      {"speaker":"interviewer","t":41.0,"end":45.0,"text":"And if Redis goes down?"}
    ]"#;

    #[test]
    fn picks_typical_interviewer_turns_in_order() {
        let t = pick_turns(TURNS, 5).unwrap();
        // 3–15 s turns: the 6.5 s and 4 s ones; fewer than 5 qualify and fewer than all 4
        // interviewer turns, so fall back to the first interviewer turns in order.
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].text, "Hi.");
        let t = pick_turns(TURNS, 2).unwrap();
        assert_eq!(
            t.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            [
                "How would you design a rate limiter?",
                "And if Redis goes down?"
            ]
        );
    }

    /// Needs `just fixtures-fetch` and `dmn doctor --fetch small.en`; run with `just probe`.
    #[test]
    #[ignore = "needs the small.en model and the fixture WAVs"]
    fn live_small_en_transcribes_a_fixture_question() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
        let models = dmn_core::Paths::from_xdg().unwrap().models_dir;
        let dir = root.join("sysdesign");
        let turns =
            pick_turns(&std::fs::read_to_string(dir.join("turns.json")).unwrap(), 2).unwrap();
        // The second typical turn is the rate-limiter question.
        let q = &turns[1];
        assert!(q.text.contains("rate limiter"), "{}", q.text);
        let audio = load_wav(&dir.join("interviewer.wav")).unwrap();
        let clip = &audio[(q.start * 16_000.0) as usize..(q.end * 16_000.0) as usize];
        let mut stt = Transcriber::load(
            &dmn_stt::model_path(&models, Model::SmallEn),
            SttConfig {
                threads: 8,
                ..SttConfig::default()
            },
        )
        .unwrap();
        let tr = stt.transcribe(clip, None).unwrap();
        let wer = word_error_rate(&q.text, &tr.text);
        assert!(wer < 0.25, "WER {wer:.2}: {:?}", tr.text);
        assert!(!tr.low_confidence, "avg_logprob {}", tr.avg_logprob);
    }

    #[test]
    fn median_and_render() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), 2.5);
        let rows = [Row {
            dur: Duration::from_secs(5),
            wall: Duration::from_millis(400),
            rtf: 0.08,
            wer: 0.0,
            low_confidence: false,
            text: "how would you design a rate limiter".into(),
            reference: "How would you design a rate limiter?".into(),
        }];
        let out = render(
            "small.en",
            "sysdesign",
            4,
            Duration::from_millis(300),
            &rows,
        );
        assert!(out.contains("median RTF 0.080"), "{out}");
        assert!(out.contains("CPU × 4 threads"), "{out}");
    }
}
