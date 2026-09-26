//! Recall and precision on the scripted fixture text.
//!
//! This is the detector alone: it reads what the interviewer was scripted to say, so a miss here
//! is the rule scorer's fault and not whisper's. `eval/run.py` runs the same comparison over real
//! transcripts from `dmn replay`, where STT errors also count; the two numbers together say
//! whether a miss came from the rules or from the audio.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use dmn_audio::Source;
use dmn_detect::{Candidate, DetectConfig, Detector, Following};
use dmn_transcript::Segment;

const FIXTURES: [&str; 4] = ["sysdesign", "behavioral", "mixed", "continuous"];

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

struct Turn {
    speaker: String,
    start: Duration,
    end: Duration,
    text: String,
    is_question: bool,
}

fn load(fixture: &str) -> Vec<Turn> {
    let path = testdata().join(fixture).join("turns.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let turns: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
    turns
        .iter()
        .map(|t| Turn {
            speaker: t["speaker"].as_str().unwrap().to_owned(),
            start: secs(t["t"].as_f64().unwrap()),
            end: secs(t["end"].as_f64().unwrap()),
            text: t["text"].as_str().unwrap().to_owned(),
            is_question: t["is_question"].as_bool().unwrap_or(false),
        })
        .collect()
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// Score every interviewer turn, giving the detector the truth about what followed it.
fn candidates(fixture: &str, cfg: DetectConfig) -> (Vec<Turn>, Vec<Option<Candidate>>) {
    let turns = load(fixture);
    let detector = Detector::new(cfg);
    let mut out = Vec::with_capacity(turns.len());
    for (i, turn) in turns.iter().enumerate() {
        if turn.speaker != "interviewer" {
            out.push(None);
            continue;
        }
        let following = match turns.get(i + 1) {
            Some(next) => Following {
                silence: Some(next.start.saturating_sub(turn.end)),
                next_speaker: Some(if next.speaker == "interviewer" {
                    Source::Interviewer
                } else {
                    Source::You
                }),
            },
            None => Following::UNKNOWN,
        };
        let segment = Segment {
            source: Source::Interviewer,
            start: turn.start,
            end: turn.end,
            text: turn.text.clone(),
            is_final: true,
            avg_logprob: -0.2,
        };
        out.push(detector.observe(&segment, following));
    }
    (turns, out)
}

struct Score {
    detected_questions: usize,
    questions: usize,
    false_positives: Vec<String>,
    misses: Vec<String>,
}

impl Score {
    fn recall(&self) -> f64 {
        self.detected_questions as f64 / self.questions.max(1) as f64
    }

    fn precision(&self) -> f64 {
        let found = self.detected_questions + self.false_positives.len();
        self.detected_questions as f64 / found.max(1) as f64
    }
}

fn score(fixture: &str, cfg: DetectConfig) -> Score {
    let (turns, found) = candidates(fixture, cfg);
    let mut s = Score {
        detected_questions: 0,
        questions: 0,
        false_positives: Vec::new(),
        misses: Vec::new(),
    };
    for (turn, candidate) in turns.iter().zip(&found) {
        if turn.speaker != "interviewer" {
            continue;
        }
        match (turn.is_question, candidate) {
            (true, Some(_)) => {
                s.questions += 1;
                s.detected_questions += 1;
            }
            (true, None) => {
                s.questions += 1;
                s.misses.push(short(&turn.text));
            }
            (false, Some(c)) => {
                s.false_positives
                    .push(format!("{} :: {}", short(&turn.text), c.render()))
            }
            (false, None) => {}
        }
    }
    s
}

fn short(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 90 {
        format!("{}…", one_line.chars().take(89).collect::<String>())
    } else {
        one_line
    }
}

#[test]
fn recall_and_precision_clear_the_m2_targets_on_scripted_text() {
    let mut overall = (0usize, 0usize, 0usize);
    for fixture in FIXTURES {
        let s = score(fixture, DetectConfig::default());
        println!(
            "{fixture:<11} recall {:.2} ({}/{}) precision {:.2} ({} false positives)",
            s.recall(),
            s.detected_questions,
            s.questions,
            s.precision(),
            s.false_positives.len()
        );
        for fp in &s.false_positives {
            println!("  false positive: {fp}");
        }
        for miss in &s.misses {
            println!("  miss:           {miss}");
        }
        overall.0 += s.detected_questions;
        overall.1 += s.questions;
        overall.2 += s.false_positives.len();
    }
    let recall = overall.0 as f64 / overall.1 as f64;
    let precision = overall.0 as f64 / (overall.0 + overall.2) as f64;
    println!(
        "overall     recall {recall:.3} ({}/{}) precision {precision:.3} ({} false positives)",
        overall.0, overall.1, overall.2
    );
    // SPEC § Milestones, M2 (#63): recall and precision >= 0.9.
    assert!(recall >= 0.9, "recall {recall:.3} is under the 0.9 target");
    assert!(
        precision >= 0.9,
        "precision {precision:.3} is under the 0.9 target"
    );
}

#[test]
fn the_spec_table_alone_is_reported_for_comparison() {
    let mut overall = (0usize, 0usize, 0usize);
    for fixture in FIXTURES {
        let s = score(fixture, DetectConfig::spec_literal());
        println!(
            "{fixture:<11} SPEC-literal recall {:.2} ({}/{}) precision {:.2} ({} false positives)",
            s.recall(),
            s.detected_questions,
            s.questions,
            s.precision(),
            s.false_positives.len()
        );
        overall.0 += s.detected_questions;
        overall.1 += s.questions;
        overall.2 += s.false_positives.len();
    }
    let recall = overall.0 as f64 / overall.1 as f64;
    let precision = overall.0 as f64 / (overall.0 + overall.2) as f64;
    println!("overall     SPEC-literal recall {recall:.3} precision {precision:.3}");
    // No assertion: this documents what the unamended table does, which is the evidence for the
    // amendment. The default config is what the targets are asserted against.
    assert!(recall > 0.0);
}

#[test]
fn scoring_a_line_is_far_inside_the_100_ms_budget() {
    let turns = load("sysdesign");
    let detector = Detector::default();
    let segments: Vec<Segment> = turns
        .iter()
        .filter(|t| t.speaker == "interviewer")
        .map(|t| Segment {
            source: Source::Interviewer,
            start: t.start,
            end: t.end,
            text: t.text.clone(),
            is_final: true,
            avg_logprob: -0.2,
        })
        .collect();
    let started = std::time::Instant::now();
    let rounds = 100;
    for _ in 0..rounds {
        for s in &segments {
            std::hint::black_box(detector.observe(s, Following::speaker(Source::You)));
        }
    }
    let per_line = started.elapsed() / (rounds * segments.len() as u32);
    println!("detector: {per_line:?} per line");
    assert!(
        per_line < Duration::from_millis(100),
        "{per_line:?} per line exceeds the 100 ms budget"
    );
}
