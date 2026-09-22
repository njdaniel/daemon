//! Issue #7 acceptance on the TTS fixtures (`just fixtures-fetch`): utterance boundaries within
//! 200 ms of the reference for at least 90 % of turns, and a 1× drain with no overruns.
//!
//! Two references are scored and both are printed:
//! - **scripted**: a turn's `t` / `end` in `turns.json`, which are the edges of the synthesised
//!   clip. Piper voices put 20–60 ms of silence before the first word and, for the Norman voice
//!   (interviewer), 150–250 ms after the last one, so a scripted `end` is not where speech ends.
//! - **voiced**: the first and last sample inside the turn's clip window whose magnitude reaches
//!   -40 dBFS, 20 dB above the tracks' -60 dBFS noise floor. On these clean synthetic tracks that
//!   is exactly where sound is, independent of the frame-RMS VAD under test.
//!
//! The assertion uses the voiced reference; see the PR for why.
//!
//! The WAVs are not in git. Without them these tests print why and pass, so a fresh clone stays
//! green; CI fetches them before running the tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use dmn_audio::{AudioSource, DEFAULT_CAPACITY_FRAMES, Source, Speed, WavSource, frame_channel};
use dmn_vad::{CloseReason, Utterance, VadConfig, VadEvent, VadWorker, event_channel};

const TOLERANCE_S: f64 = 0.200;
const REQUIRED: f64 = 0.90;
/// Sample magnitude that counts as sound for the voiced reference: -40 dBFS.
const VOICED_PEAK: f64 = 0.01;

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata")
}

fn wav(fixture: &str, speaker: &str) -> Option<PathBuf> {
    let p = testdata().join(fixture).join(format!("{speaker}.wav"));
    if p.exists() {
        Some(p)
    } else {
        eprintln!(
            "skipping: {} missing; run `just fixtures-fetch`",
            p.display()
        );
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    t: f64,
    end: f64,
}

fn turns(fixture: &str, speaker: &str) -> Vec<Bounds> {
    let text = std::fs::read_to_string(testdata().join(fixture).join("turns.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .filter(|x| x["speaker"] == speaker)
        .map(|x| Bounds {
            t: x["t"].as_f64().unwrap(),
            end: x["end"].as_f64().unwrap(),
        })
        .collect()
}

/// 16 kHz mono 16-bit PCM samples of a fixture WAV, as f64 in [-1, 1).
fn pcm16(path: &Path) -> Vec<f64> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return bytes[i + 8..(i + 8 + len).min(bytes.len())]
                .chunks_exact(2)
                .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0)
                .collect();
        }
        i += 8 + len + (len & 1);
    }
    panic!("{}: no data chunk", path.display());
}

/// Where sound actually is inside each turn's clip window.
fn voiced(turns: &[Bounds], samples: &[f64]) -> Vec<Bounds> {
    let rate = 16_000.0;
    turns
        .iter()
        .map(|b| {
            let lo = ((b.t * rate) as usize).min(samples.len());
            let hi = ((b.end * rate) as usize).min(samples.len());
            let win = &samples[lo..hi];
            let first = win.iter().position(|x| x.abs() >= VOICED_PEAK).unwrap_or(0);
            let last = win
                .iter()
                .rposition(|x| x.abs() >= VOICED_PEAK)
                .unwrap_or(win.len().saturating_sub(1));
            Bounds {
                t: (lo + first) as f64 / rate,
                end: (lo + last + 1) as f64 / rate,
            }
        })
        .collect()
}

/// Run one speaker WAV through WavSource (fast) → VadWorker and return its utterances.
fn segment(path: &Path, source: Source) -> Vec<Utterance> {
    let mut wav = WavSource::open(path, source, Speed::Fast).unwrap();
    // Fast mode decodes far quicker than the worker drains, so the ring holds the whole file:
    // the point here is boundary accuracy, and an overrun would shift every later timestamp.
    let cap = (wav.duration().as_millis() / 20) as usize + 100;
    let (sink, frames) = frame_channel(source, cap);
    let (tx, rx) = event_channel(100_000);
    let worker = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
    wav.start(sink).unwrap();
    wav.wait().unwrap();
    let stats = worker.finish().unwrap();
    assert_eq!(stats.overruns, 0, "{}: ring overran", path.display());
    assert_eq!(stats.dropped_events, 0);
    rx.try_iter()
        .filter_map(|e| match e {
            VadEvent::Utterance(u) => Some(u),
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct Score {
    turns: usize,
    ok: usize,
    missed: usize,
    extra: usize,
    split: usize,
    errors_ms: Vec<f64>,
    misses: Vec<String>,
}

fn score(refs: &[Bounds], utts: &[Utterance]) -> Score {
    let secs = |d: Duration| d.as_secs_f64();
    let mut s = Score {
        turns: refs.len(),
        ..Score::default()
    };
    let mut claimed = vec![false; utts.len()];
    for r in refs {
        let idx: Vec<usize> = (0..utts.len())
            .filter(|&i| {
                secs(utts[i].end) > r.t - TOLERANCE_S && secs(utts[i].start) < r.end + TOLERANCE_S
            })
            .collect();
        let (Some(&first), Some(&last)) = (idx.first(), idx.last()) else {
            s.missed += 1;
            s.misses
                .push(format!("turn {:.2}–{:.2} s: no utterance", r.t, r.end));
            continue;
        };
        for &i in &idx {
            claimed[i] = true;
        }
        // Pieces other than 12 s forced splits mean the turn was cut by a >700 ms gap.
        let gaps: Vec<String> = idx
            .windows(2)
            .filter(|w| utts[w[0]].closed_by != CloseReason::MaxLength)
            .map(|w| {
                format!(
                    "{:.0} ms gap at {:.2} s",
                    (secs(utts[w[1]].start) - secs(utts[w[0]].end)) * 1000.0,
                    secs(utts[w[0]].end)
                )
            })
            .collect();
        if !gaps.is_empty() {
            s.split += 1;
            s.misses.push(format!(
                "turn {:.2}–{:.2} s split by {}",
                r.t,
                r.end,
                gaps.join(", ")
            ));
        }
        let d_start = secs(utts[first].start) - r.t;
        let d_end = secs(utts[last].end) - r.end;
        s.errors_ms.push(d_start.abs() * 1000.0);
        s.errors_ms.push(d_end.abs() * 1000.0);
        if d_start.abs() <= TOLERANCE_S && d_end.abs() <= TOLERANCE_S {
            s.ok += 1;
        } else {
            s.misses.push(format!(
                "turn {:.2}–{:.2} s: start {:+.0} ms, end {:+.0} ms",
                r.t,
                r.end,
                d_start * 1000.0,
                d_end * 1000.0
            ));
        }
    }
    s.extra = claimed.iter().filter(|c| !**c).count();
    s
}

fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn add(total: &mut Score, s: Score) {
    total.turns += s.turns;
    total.ok += s.ok;
    total.missed += s.missed;
    total.extra += s.extra;
    total.split += s.split;
    total.errors_ms.extend(s.errors_ms);
    total.misses.extend(s.misses);
}

fn line(fixture: &str, who: &str, reference: &str, s: &Score) {
    eprintln!(
        "{fixture:<10} {who:<11} {reference:<8} turns {:>2}  within 200 ms {:>2} ({:>5.1} %)  median {:>3.0} ms  p95 {:>3.0} ms  missed {}  split {}  extra {}",
        s.turns,
        s.ok,
        100.0 * s.ok as f64 / s.turns.max(1) as f64,
        percentile(&s.errors_ms, 0.5),
        percentile(&s.errors_ms, 0.95),
        s.missed,
        s.split,
        s.extra,
    );
}

fn check_fixture(fixture: &str) {
    let mut scripted_total = Score::default();
    let mut voiced_total = Score::default();
    for (speaker, source) in [
        ("interviewer", Source::Interviewer),
        ("candidate", Source::You),
    ] {
        let Some(path) = wav(fixture, speaker) else {
            return;
        };
        let utts = segment(&path, source);
        let scripted_refs = turns(fixture, speaker);
        let voiced_refs = voiced(&scripted_refs, &pcm16(&path));
        let scripted = score(&scripted_refs, &utts);
        let voiced = score(&voiced_refs, &utts);
        line(fixture, speaker, "scripted", &scripted);
        line(fixture, speaker, "voiced", &voiced);
        add(&mut scripted_total, scripted);
        add(&mut voiced_total, voiced);
    }
    line(fixture, "both", "scripted", &scripted_total);
    line(fixture, "both", "voiced", &voiced_total);
    for m in &voiced_total.misses {
        eprintln!("  voiced: {m}");
    }
    let pct = voiced_total.ok as f64 / voiced_total.turns as f64;
    assert!(
        pct >= REQUIRED,
        "{fixture}: {:.1} % of turns within 200 ms of the voiced bounds, need {:.0} %",
        100.0 * pct,
        100.0 * REQUIRED
    );
    assert_eq!(voiced_total.missed, 0, "{fixture}: turns with no utterance");
    // Splits are reported, not failed: a voice that pauses longer than 700 ms between sentences
    // inside one scripted turn is correctly two utterances under SPEC's close rule.
    assert_eq!(
        voiced_total.extra, 0,
        "{fixture}: utterances outside every turn"
    );
}

#[test]
fn sysdesign_boundaries_within_200_ms() {
    check_fixture("sysdesign");
}

#[test]
fn behavioral_boundaries_within_200_ms() {
    check_fixture("behavioral");
}

#[test]
fn mixed_boundaries_within_200_ms() {
    check_fixture("mixed");
}

/// Acceptance: the per-source VAD thread keeps up at 1× with the default 2 s ring. About 10 s of
/// wall clock, so it runs in the normal test suite (in parallel with the rest).
#[test]
fn drains_at_1x_with_the_default_ring_and_no_overruns() {
    let Some(path) = wav("sysdesign", "interviewer") else {
        return;
    };
    let mut wav = WavSource::open(&path, Source::Interviewer, Speed::RealTime).unwrap();
    let (sink, frames) = frame_channel(Source::Interviewer, DEFAULT_CAPACITY_FRAMES);
    let (tx, rx) = event_channel(64);
    let worker = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
    wav.start(sink).unwrap();
    std::thread::sleep(Duration::from_secs(10));
    wav.stop().unwrap();
    let stats = worker.finish().unwrap();
    eprintln!("1x drain: {stats:?}");
    assert_eq!(stats.overruns, 0);
    assert!(
        (480..=520).contains(&stats.frames),
        "about 500 frames in 10 s: {}",
        stats.frames
    );
    let first = rx.try_iter().find_map(|e| match e {
        VadEvent::Utterance(u) => Some(u),
        _ => None,
    });
    assert!(first.is_some(), "the first 10 s holds speech");
}
