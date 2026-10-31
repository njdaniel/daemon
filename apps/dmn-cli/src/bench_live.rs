//! `dmn bench --live` (#16): question end → Approach latency and CPU use, read back from a stored
//! session (the latest, or `--session <id>`).
//!
//! Every time comes from the session log (schema v2): a question's end is its segment's
//! `end_time`, and `emitted_at` / `question_at` / `approach_at` are the audio times the pipeline
//! stamped when the detector emitted it and the fast phase produced its question line and first
//! Approach line. All of them are on the clock segment times use (dmn-core's `AudioClock`), so
//! the difference is the latency a person beside the call sees, minus the redraw.
//!
//! With a script (`--questions <questions.json>`, or the `questions.json` next to a replay's
//! Them file, found from the session label) each scripted question is paired with the
//! candidate that showed it first, exactly as `dmn run --replay --headless` does
//! ([`latency::pair`]), so the two reports agree. Without one (a live call) every candidate is
//! its own row, timed from the end of its segment; candidates the fast phase did not show (not a
//! question, small talk, superseded) are listed as never shown and left out of p50 / p95.
//!
//! CPU is the dmn process's user + system time between samples (Ollama runs in its own
//! process and is not counted). "Idle listening" is every sample window in which nobody was
//! speaking and no question was being answered: whisper ran no inference in it (it transcribes
//! only while someone speaks, a partial every second and a final at the end), nor in the window
//! after it, since an inference is counted when it finishes, up to a couple of seconds after it
//! starts; and no question was between its emission and its first Approach line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dmn_core::Paths;
use dmn_core::recorder::open_store;
use dmn_store::{CpuSampleRow, QuestionRow, SegmentRow, Store};

use crate::latency::{self, Row, Scripted, Seen};
use crate::run::APPROACH_BUDGET_S;

/// SPEC § Latency budget: p95 question end → Approach.
pub const APPROACH_P95_BUDGET_S: f64 = 3.5;
/// The #16 idle-listening CPU budget, in cores.
pub const IDLE_CPU_BUDGET_CORES: f64 = 1.0;
/// Whisper inference below this, in a window, counts as none (the counter is in milliseconds).
const NO_INFERENCE_S: f64 = 0.001;

pub struct Args {
    /// Session id; the newest session when `None`.
    pub session: Option<i64>,
    /// A fixture's `questions.json` to pair candidates with.
    pub questions: Option<PathBuf>,
}

/// Everything the report needs from one stored session.
#[derive(Debug, Clone)]
pub struct Stored {
    pub id: i64,
    pub label: String,
    pub overruns: i64,
    /// Mic finals the echo guard dropped as copies of the loopback (schema v7, #60).
    pub echo_dropped: i64,
    pub segments: Vec<SegmentRow>,
    pub questions: Vec<QuestionRow>,
    pub cpu: Vec<CpuSampleRow>,
}

pub fn load(store: &Store, session: Option<i64>) -> Result<Stored, String> {
    let err = |e: dmn_store::StoreError| e.to_string();
    let id = match session {
        Some(id) => id,
        None => store
            .latest_session_id()
            .map_err(err)?
            .ok_or("no sessions stored yet: run `dmn run` (or `dmn run --replay …`) first")?,
    };
    let row = store
        .session(id)
        .map_err(err)?
        .ok_or_else(|| format!("no session {id}; `dmn sessions list` shows the stored ones"))?;
    Ok(Stored {
        id,
        label: row.label,
        overruns: row.overruns,
        echo_dropped: row.echo_dropped,
        segments: store.segments(id).map_err(err)?,
        questions: store.questions(id).map_err(err)?,
        cpu: store.cpu_samples(id).map_err(err)?,
    })
}

/// The script a replay session was made from: `questions.json` beside its Them file,
/// read from the label `replay <them> + <you>` the pipeline writes.
pub fn script_for(label: &str) -> Option<PathBuf> {
    let files = label.strip_prefix("replay ")?;
    let (them, _) = files.split_once(" + ")?;
    let path = Path::new(them).parent()?.join("questions.json");
    path.is_file().then_some(path)
}

/// One [`Seen`] per timed candidate, keyed by question row id. Rows written before schema v2
/// (no `emitted_at`) are left out: there is nothing to time them by.
pub fn seen(s: &Stored) -> BTreeMap<u64, Seen> {
    let segments: BTreeMap<i64, &SegmentRow> = s.segments.iter().map(|g| (g.id, g)).collect();
    s.questions
        .iter()
        .filter_map(|q| {
            let seg = segments.get(&q.segment_id)?;
            let emitted = q.emitted_at?;
            Some((
                u64::try_from(q.id).ok()?,
                Seen {
                    start: seg.start_time,
                    end: seg.end_time,
                    // The detector scores a final the moment it arrives (well under 1 ms), so
                    // emission stands in for the final's arrival.
                    final_at: emitted,
                    emitted_at: emitted,
                    question_at: q.question_at,
                    approach_at: q.approach_at,
                    clarification: q.kind.as_deref() == Some("clarification"),
                },
            ))
        })
        .collect()
}

/// Without a script: each timed candidate is its own row, its "question" the candidate's segment.
pub fn unscripted_rows(s: &Stored, seen: &BTreeMap<u64, Seen>) -> Vec<Row> {
    let by_id: BTreeMap<i64, &QuestionRow> = s.questions.iter().map(|q| (q.id, q)).collect();
    seen.iter()
        .map(|(id, c)| {
            let q = i64::try_from(*id).ok().and_then(|i| by_id.get(&i));
            let segment = || {
                s.segments
                    .iter()
                    .find(|g| g.start_time == c.start && g.end_time == c.end)
                    .map(|g| g.text.clone())
                    .unwrap_or_default()
            };
            Row {
                q: Scripted {
                    t: c.start,
                    end: c.end,
                    text: q.and_then(|q| q.text.clone()).unwrap_or_else(segment),
                    kind: q.and_then(|q| q.kind.clone()).unwrap_or_else(|| "-".into()),
                },
                seen: Some(c.clone()),
                latency: c.shown_at().map(|at| at - c.end),
            }
        })
        .collect()
}

/// CPU over a session, in cores (CPU seconds per wall second).
#[derive(Debug, Clone, PartialEq)]
pub struct Cpu {
    pub overall: Option<f64>,
    /// Per idle window, in time order.
    pub idle: Vec<f64>,
}

pub fn cpu(s: &Stored) -> Cpu {
    let samples = &s.cpu;
    let overall = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) if b.wall > a.wall => Some((b.cpu - a.cpu) / (b.wall - a.wall)),
        _ => None,
    };
    // A question is in flight from its emission to its first Approach line (or its question
    // line, or a second after emission when it was never shown).
    let answering: Vec<(f64, f64)> = s
        .questions
        .iter()
        .filter_map(|q| {
            let from = q.emitted_at?;
            Some((from, q.approach_at.or(q.question_at).unwrap_or(from + 1.0)))
        })
        .collect();
    let transcribed = |i: usize| {
        samples
            .get(i + 1)
            .is_some_and(|b| b.whisper - samples[i].whisper >= NO_INFERENCE_S)
    };
    let idle = (0..samples.len().saturating_sub(1))
        .filter(|&i| samples[i + 1].wall > samples[i].wall)
        // This window and the next saw no inference: an inference is counted when it ends.
        .filter(|&i| !transcribed(i) && !transcribed(i + 1))
        .filter(|&i| {
            let (from, to) = (samples[i].at, samples[i + 1].at);
            !answering.iter().any(|(a, b)| *a < to && *b > from)
        })
        .map(|i| {
            let (a, b) = (&samples[i], &samples[i + 1]);
            (b.cpu - a.cpu) / (b.wall - a.wall)
        })
        .collect();
    Cpu { overall, idle }
}

fn cores(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), |c| format!("{c:.2} cores"))
}

/// The whole report for one stored session.
pub fn report(s: &Stored, script: Option<&Path>) -> Result<String, String> {
    let mut out = format!("session {}: {}\n", s.id, s.label);
    let seen = seen(s);
    if s.questions.is_empty() {
        out.push_str("no timed questions: the detector found no question in this session\n");
    } else if seen.is_empty() {
        out.push_str(&format!(
            "no timed questions: {} question row(s), none with timing (sessions stored before \
             #16 have none)\n",
            s.questions.len()
        ));
    } else {
        let rows = match script {
            Some(path) => {
                out.push_str(&format!("paired with {}\n", path.display()));
                latency::pair(&latency::load_scripted(path)?, &seen)
            }
            None => {
                out.push_str("no script: every candidate is timed from the end of its segment\n");
                unscripted_rows(s, &seen)
            }
        };
        out.push_str(&latency::render(&rows, APPROACH_BUDGET_S));
        let sum = latency::summarise(&rows);
        out.push_str(&format!(
            "p95 budget ≤ {APPROACH_P95_BUDGET_S:.1} s: {}\n",
            match sum.p95 {
                Some(p) if p <= APPROACH_P95_BUDGET_S => "met",
                Some(_) => "NOT met",
                None => "no data",
            }
        ));
    }

    let c = cpu(s);
    let idle_median = latency::percentile(&c.idle, 50.0);
    let idle_max = c.idle.iter().copied().reduce(f64::max);
    out.push_str(&format!(
        "\ncpu (dmn process; Ollama is its own process): overall {}; idle listening median {}, \
         max {} over {} window(s) (budget ≤ {IDLE_CPU_BUDGET_CORES:.0} core: {})\n",
        cores(c.overall),
        cores(idle_median),
        cores(idle_max),
        c.idle.len(),
        match idle_max {
            Some(m) if m <= IDLE_CPU_BUDGET_CORES => "met",
            Some(_) => "NOT met",
            None => "no idle window",
        }
    ));
    out.push_str(&format!(
        "overruns: {} ring-buffer frame(s) dropped{}\n",
        s.overruns,
        if s.overruns == 0 {
            ""
        } else {
            " (NOT met: the target is none)"
        }
    ));
    out.push_str(&format!(
        "echo guard: {} mic line(s) dropped as copies of the loopback\n",
        s.echo_dropped
    ));
    Ok(out)
}

pub fn run(paths: &Paths, args: &Args) -> Result<String, String> {
    if !paths.db_file.is_file() {
        return Err(format!(
            "no session log at {}: run `dmn run` (or `dmn run --replay …`) first",
            paths.db_file.display()
        ));
    }
    let store = open_store(paths).map_err(|e| e.to_string())?;
    let stored = load(&store, args.session)?;
    let script = args.questions.clone().or_else(|| script_for(&stored.label));
    report(&stored, script.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_store::{Mark, NewCpuSample, NewQuestion, NewSegment, NewSession, Source};

    /// A stored session: two Them questions (one answered at 1.5 s, one at 2.5 s after
    /// its end), one candidate never shown, CPU sampled every 5 s over 60 s with the process
    /// busy (2 cores) while anyone speaks and idle (0.1 core) otherwise.
    fn session(store: &Store) -> i64 {
        let id = store
            .insert_session(&NewSession::new(1_700_000_000, "live mic + app Firefox"))
            .unwrap();
        let ask = |start: f64, end: f64, text: &str, shown: Option<f64>| {
            let seg = store
                .insert_segment(&NewSegment {
                    session_id: id,
                    source: Source::Them,
                    start_time: start,
                    end_time: end,
                    text: text.into(),
                    is_final: true,
                    avg_logprob: -0.2,
                    language: None,
                })
                .unwrap();
            let q = store
                .insert_question(&NewQuestion {
                    session_id: id,
                    segment_id: seg,
                    score: 5,
                    created_at: 1_700_000_000 + start as i64,
                })
                .unwrap();
            store.mark_question(q, Mark::Emitted, end + 1.0).unwrap();
            if let Some(after) = shown {
                store
                    .set_question_text(q, text, Some("system_design"), None)
                    .unwrap();
                store
                    .mark_question(q, Mark::Question, end + after - 0.3)
                    .unwrap();
                store.mark_question(q, Mark::Approach, end + after).unwrap();
            }
        };
        ask(10.0, 14.0, "How would you shard it?", Some(1.5));
        ask(30.0, 33.0, "And if a region fails?", Some(2.5));
        ask(40.0, 41.0, "Right.", None);
        // Whisper finishes inferences in the windows starting at these times (the speech above),
        // and the process works hard in them; it idles in the rest.
        let busy = [10, 15, 30, 35, 40];
        let (mut cpu, mut whisper) = (0.0, 0.0);
        for t in (0..=60).step_by(5) {
            store
                .insert_cpu_sample(&NewCpuSample {
                    session_id: id,
                    at: f64::from(t),
                    wall: f64::from(t),
                    cpu,
                    whisper,
                })
                .unwrap();
            let speaking = busy.contains(&t);
            cpu += if speaking { 10.0 } else { 0.5 };
            whisper += if speaking { 1.0 } else { 0.0 };
        }
        store.set_session_overruns(id, 0).unwrap();
        id
    }

    #[test]
    fn a_live_session_reports_latency_cpu_and_overruns() {
        let store = Store::open_in_memory().unwrap();
        let id = session(&store);
        let stored = load(&store, None).unwrap();
        assert_eq!(stored.id, id, "the latest session by default");
        let text = report(&stored, None).unwrap();
        assert!(text.contains("p50 1.50 s  p95 2.50 s"), "{text}");
        assert!(text.contains("1 never shown"), "{text}");
        assert!(text.contains("p95 budget ≤ 3.5 s: met"), "{text}");
        assert!(text.contains("idle listening median 0.10 cores"), "{text}");
        assert!(text.contains("overruns: 0"), "{text}");

        let c = cpu(&stored);
        // Idle: 0–5, 20–25 and 45–60. 5–10 and 25–30 are quiet too, but an inference that
        // started in them would only be counted in the next window, so they are not trusted.
        assert!(c.idle.iter().all(|&x| (x - 0.1).abs() < 1e-9), "{c:?}");
        assert_eq!(c.idle.len(), 5, "{c:?}");
        assert!(c.overall.unwrap() > 0.1);
    }

    #[test]
    fn a_script_pairs_questions_like_the_headless_run() {
        let store = Store::open_in_memory().unwrap();
        session(&store);
        let stored = load(&store, None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("questions.json");
        std::fs::write(
            &script,
            r#"[{"t": 10.0, "end": 14.2, "text": "How would you shard it?", "kind": "system_design"},
                {"t": 50.0, "end": 55.0, "text": "Never asked", "kind": "behavioral"}]"#,
        )
        .unwrap();
        let text = report(&stored, Some(&script)).unwrap();
        // 14.0 + 1.5 on screen, scripted end 14.2: 1.30 s.
        assert!(text.contains("p50 1.30 s"), "{text}");
        assert!(text.contains("missed"), "{text}");
    }

    #[test]
    fn a_replay_label_finds_its_script() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sysdesign");
        let label = format!(
            "replay {} + {}",
            fixture.join("interviewer.wav").display(),
            fixture.join("candidate.wav").display()
        );
        assert_eq!(
            script_for(&label),
            Some(fixture.join("questions.json")),
            "{label}"
        );
        assert_eq!(script_for("live mic + app Firefox"), None);
    }

    #[test]
    fn sessions_without_timing_say_so_and_a_missing_session_is_an_error() {
        let store = Store::open_in_memory().unwrap();
        assert!(load(&store, None).unwrap_err().contains("no sessions"));
        let id = store
            .insert_session(&NewSession::new(1_700_000_000, "old"))
            .unwrap();
        let text = report(&load(&store, Some(id)).unwrap(), None).unwrap();
        assert!(text.contains("no timed questions"), "{text}");
        assert!(text.contains("no idle window"), "{text}");
        assert!(
            load(&store, Some(99))
                .unwrap_err()
                .contains("no session 99")
        );
    }
}
