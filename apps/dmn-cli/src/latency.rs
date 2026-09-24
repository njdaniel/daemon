//! Question-end → on-screen latency for `dmn run --replay … --headless`: #15's "shows the
//! question and Approach within 2.0 s p50 of the scripted question end", measured on the same
//! pipeline and session the TUI draws, without a terminal.
//!
//! Times are audio seconds: 0 is the instant both files started, and at 1× the wall clock and
//! the audio advance together. For each candidate the pipeline records when its final segment
//! arrived and when the detector emitted it; the session watch records when the question first
//! appeared and when its first Approach line (or STAR card) did. Each scripted question in the
//! fixture's `questions.json` is then paired with the candidate whose segment overlaps it and
//! reached the screen first; its latency is that moment minus the scripted question's `end`.
//!
//! A question counts as on screen when its text and at least one Approach line (or a STAR card)
//! are showing. A clarification that has no Approach lines of its own counts once its `↳` line
//! shows, since that line is all a clarification puts on screen.

use std::collections::BTreeMap;
use std::path::Path;

use dmn_session::Session;

/// A candidate's segment may start this long before the scripted question (a VAD frame or a
/// forced split) and end this long after it, and still be that question.
const BEFORE_S: f64 = 1.0;
const AFTER_S: f64 = 2.0;

/// One question from a fixture's `questions.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Scripted {
    pub t: f64,
    pub end: f64,
    pub text: String,
    pub kind: String,
}

/// Read `questions.json`: an array of `{t, end, text, kind}`.
pub fn load_scripted(path: &Path) -> Result<Vec<Scripted>, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let items = v
        .as_array()
        .ok_or_else(|| format!("{}: not a JSON array", path.display()))?;
    items
        .iter()
        .map(|q| {
            let num = |k: &str| {
                q.get(k)
                    .and_then(serde_json::Value::as_f64)
                    .ok_or_else(|| format!("{}: a question has no numeric {k:?}", path.display()))
            };
            let text = |k: &str| {
                q.get(k)
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Ok(Scripted {
                t: num("t")?,
                end: num("end")?,
                text: text("text"),
                kind: text("kind"),
            })
        })
        .collect()
}

/// What the pipeline and the screen did for one candidate, in audio seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    /// The candidate's segment.
    pub start: f64,
    pub end: f64,
    /// When the final segment it came from reached the pipeline loop.
    pub final_at: f64,
    /// When the detector emitted it.
    pub emitted_at: f64,
    /// When the question (or the `↳` clarification line) first showed.
    pub question_at: Option<f64>,
    /// When its first Approach line or STAR card first showed.
    pub approach_at: Option<f64>,
    pub clarification: bool,
}

impl Seen {
    /// When the candidate counted as on screen, if it ever did.
    pub fn shown_at(&self) -> Option<f64> {
        self.approach_at
            .or(self.question_at.filter(|_| self.clarification))
    }
}

/// Screen times per candidate id, recorded from the session `watch` channel.
#[derive(Debug, Default)]
pub struct Screen {
    seen: BTreeMap<u64, (Option<f64>, Option<f64>, bool)>,
}

impl Screen {
    /// Note what `s` shows at audio time `now`. Only first sightings are kept.
    pub fn observe(&mut self, s: &Session, now: f64) {
        // Only the live question is on screen; one queued behind a pin is a badge, not a
        // question anyone can read yet.
        if let Some(q) = &s.live {
            // A regenerated question is the same candidate; its first showing is what counts.
            let e = self.seen.entry(q.candidate).or_default();
            e.0.get_or_insert(now);
            if !q.approach.is_empty() || q.star.is_some() {
                e.1.get_or_insert(now);
            }
            for c in &q.clarifications {
                let e = self.seen.entry(c.candidate).or_default();
                e.0.get_or_insert(now);
                e.2 = true;
                if !c.approach.is_empty() {
                    e.1.get_or_insert(now);
                }
            }
        }
    }

    /// Fill each candidate's screen times from what was observed.
    pub fn apply(&self, emitted: &mut BTreeMap<u64, Seen>) {
        for (id, seen) in emitted.iter_mut() {
            if let Some((question, approach, clarification)) = self.seen.get(id) {
                seen.question_at = *question;
                seen.approach_at = *approach;
                seen.clarification = *clarification;
            }
        }
    }
}

/// One scripted question and what happened to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub q: Scripted,
    /// The candidate that put it on screen first, if any overlapped it.
    pub seen: Option<Seen>,
    /// Scripted question end → on screen, when it got there.
    pub latency: Option<f64>,
}

/// Pair each scripted question with the overlapping candidate that reached the screen first.
/// Questions are taken in order and a candidate is used at most once.
pub fn pair(scripted: &[Scripted], candidates: &BTreeMap<u64, Seen>) -> Vec<Row> {
    let mut used = Vec::new();
    let mut rows = Vec::new();
    let mut order: Vec<&Scripted> = scripted.iter().collect();
    order.sort_by(|a, b| a.t.total_cmp(&b.t));
    for q in order {
        let overlapping = candidates.iter().filter(|(id, c)| {
            !used.contains(*id) && c.start < q.end + AFTER_S && c.end > q.t - BEFORE_S
        });
        // Earliest on screen wins; a candidate that never showed is kept only if nothing did.
        let best = overlapping.min_by(|(_, a), (_, b)| {
            let key = |c: &Seen| c.shown_at().unwrap_or(f64::INFINITY);
            key(a).total_cmp(&key(b)).then(a.start.total_cmp(&b.start))
        });
        let (seen, latency) = match best {
            Some((id, c)) => {
                used.push(*id);
                (Some(c.clone()), c.shown_at().map(|at| at - q.end))
            }
            None => (None, None),
        };
        rows.push(Row {
            q: q.clone(),
            seen,
            latency,
        });
    }
    rows
}

/// Nearest-rank percentile of `values` (0 < `p` ≤ 100).
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    v.get(rank.clamp(1, v.len()) - 1).copied()
}

fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let v: Vec<f64> = values.collect();
    percentile(&v, 50.0)
}

/// The summary numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub shown: usize,
    pub missed: usize,
}

pub fn summarise(rows: &[Row]) -> Summary {
    let lat: Vec<f64> = rows.iter().filter_map(|r| r.latency).collect();
    Summary {
        p50: percentile(&lat, 50.0),
        p95: percentile(&lat, 95.0),
        shown: lat.len(),
        missed: rows.len() - lat.len(),
    }
}

fn secs(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), |s| format!("{s:+.2}"))
}

fn short(text: &str, cells: usize) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= cells {
        return one;
    }
    let mut out: String = one.chars().take(cells.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The report `dmn run --headless` prints. Every time is relative to the scripted question end.
pub fn render(rows: &[Row], budget_s: f64) -> String {
    let mut out = String::new();
    out.push_str(
        "scripted question end → on screen (s); final = its last segment reached the pipeline, \
         question/approach = first shown\n",
    );
    out.push_str(&format!(
        "{:>3} {:>7} {:>7} {:>8} {:>8} {:>8}  {:<14} {}\n",
        "#", "end", "final", "question", "approach", "latency", "kind", "question"
    ));
    for (i, r) in rows.iter().enumerate() {
        let rel = |v: Option<f64>| secs(v.map(|t| t - r.q.end));
        let (final_at, question, approach) = match &r.seen {
            Some(s) => (
                rel(Some(s.final_at)),
                rel(s.question_at),
                rel(s.approach_at),
            ),
            None => ("-".into(), "-".into(), "-".into()),
        };
        let latency = match r.latency {
            Some(l) => format!("{l:.2}{}", if l > budget_s { "*" } else { "" }),
            None => "missed".into(),
        };
        out.push_str(&format!(
            "{:>3} {:>7.1} {:>7} {:>8} {:>8} {:>8}  {:<14} {}\n",
            i + 1,
            r.q.end,
            final_at,
            question,
            approach,
            latency,
            r.q.kind,
            short(&r.q.text, 60)
        ));
    }
    let s = summarise(rows);
    let fmt = |v: Option<f64>| v.map_or_else(|| "-".into(), |x| format!("{x:.2} s"));
    out.push_str(&format!(
        "\np50 {}  p95 {}  over {} question(s) on screen; {} never shown (budget p50 ≤ {budget_s:.1} s: {})\n",
        fmt(s.p50),
        fmt(s.p95),
        s.shown,
        s.missed,
        match s.p50 {
            Some(p) if p <= budget_s => "met",
            Some(_) => "NOT met",
            None => "no data",
        }
    ));
    // Where the time goes, as medians over the questions that reached the screen: end of speech
    // to the final transcript (VAD's silence close + the final whisper pass), final to
    // candidate (the detector), candidate to question and to the first Approach line (the LLM).
    let shown: Vec<(&Scripted, &Seen)> = rows
        .iter()
        .filter(|r| r.latency.is_some())
        .filter_map(|r| r.seen.as_ref().map(|s| (&r.q, s)))
        .collect();
    if !shown.is_empty() {
        let m = |f: &dyn Fn(&Scripted, &Seen) -> Option<f64>| {
            fmt(median(shown.iter().filter_map(|(q, s)| f(q, s))))
        };
        out.push_str(&format!(
            "stage medians: speech end → final {}  (of which segment end → final, i.e. whisper, {}); \
             final → candidate {}; candidate → question {}; candidate → first Approach line {}\n",
            m(&|q, s| Some(s.final_at - q.end)),
            m(&|_, s| Some(s.final_at - s.end)),
            m(&|_, s| Some(s.emitted_at - s.final_at)),
            m(&|_, s| s.question_at.map(|t| t - s.emitted_at)),
            m(&|_, s| s.approach_at.map(|t| t - s.emitted_at)),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_session::{Action, SessionHub};

    fn q(t: f64, end: f64) -> Scripted {
        Scripted {
            t,
            end,
            text: format!("q at {t}"),
            kind: "system_design".into(),
        }
    }

    fn seen(start: f64, end: f64, shown: Option<f64>) -> Seen {
        Seen {
            start,
            end,
            final_at: end + 1.5,
            emitted_at: end + 1.5,
            question_at: shown.map(|s| s - 0.4),
            approach_at: shown,
            clarification: false,
        }
    }

    #[test]
    fn each_question_takes_the_overlapping_candidate_that_showed_first() {
        let scripted = [q(20.0, 30.0), q(95.0, 100.0), q(200.0, 205.0)];
        let mut c = BTreeMap::new();
        // Two candidates overlap the first question (a forced 12 s split); the later one showed
        // first.
        c.insert(1, seen(19.9, 25.0, Some(34.0)));
        c.insert(2, seen(25.0, 30.5, Some(32.1)));
        c.insert(3, seen(94.8, 100.6, Some(102.0)));
        // Nothing overlaps the third.
        c.insert(4, seen(150.0, 152.0, Some(154.0)));
        let rows = pair(&scripted, &c);
        assert!((rows[0].latency.unwrap() - 2.1).abs() < 1e-9);
        assert!((rows[1].latency.unwrap() - 2.0).abs() < 1e-9);
        assert_eq!(rows[2].latency, None);
        assert_eq!(rows[2].seen, None);
        let s = summarise(&rows);
        assert_eq!((s.shown, s.missed), (2, 1));
    }

    #[test]
    fn a_candidate_that_never_showed_is_a_miss_not_a_latency() {
        let mut c = BTreeMap::new();
        c.insert(1, seen(20.0, 30.0, None));
        let rows = pair(&[q(20.0, 30.0)], &c);
        assert!(rows[0].seen.is_some());
        assert_eq!(rows[0].latency, None);
    }

    #[test]
    fn a_clarification_counts_once_its_line_shows() {
        let c = Seen {
            question_at: Some(3.0),
            clarification: true,
            ..Seen::default()
        };
        assert_eq!(c.shown_at(), Some(3.0));
        let plain = Seen {
            question_at: Some(3.0),
            ..Seen::default()
        };
        assert_eq!(plain.shown_at(), None, "a question needs its Approach");
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v = [3.0, 1.0, 2.0, 4.0];
        assert_eq!(percentile(&v, 50.0), Some(2.0));
        assert_eq!(percentile(&v, 95.0), Some(4.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 95.0), Some(7.0));
    }

    #[test]
    fn the_screen_records_first_sightings_from_the_session() {
        let hub = SessionHub::default();
        let mut screen = Screen::default();
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "Q".into(),
            kind: "system_design".into(),
        });
        screen.observe(&hub.snapshot(), 10.0);
        hub.dispatch(Action::ApproachLine {
            candidate: 1,
            index: 0,
            text: "a".into(),
        });
        screen.observe(&hub.snapshot(), 10.5);
        screen.observe(&hub.snapshot(), 11.0);
        hub.dispatch(Action::Clarification {
            candidate: 2,
            text: "and if Redis is down?".into(),
        });
        screen.observe(&hub.snapshot(), 20.0);

        let mut emitted = BTreeMap::new();
        emitted.insert(1, Seen::default());
        emitted.insert(2, Seen::default());
        screen.apply(&mut emitted);
        assert_eq!(emitted[&1].question_at, Some(10.0));
        assert_eq!(emitted[&1].approach_at, Some(10.5));
        assert_eq!(emitted[&2].question_at, Some(20.0));
        assert!(emitted[&2].clarification);
        assert_eq!(emitted[&2].shown_at(), Some(20.0));
    }

    #[test]
    fn scripted_questions_load_from_the_fixture() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sysdesign/questions.json");
        let qs = load_scripted(&path).unwrap();
        assert_eq!(qs.len(), 14);
        assert!(qs.iter().all(|q| q.end > q.t));
    }

    #[test]
    fn the_report_flags_over_budget_and_missed_questions() {
        let rows = vec![
            Row {
                q: q(20.0, 30.0),
                seen: Some(seen(20.0, 30.0, Some(33.0))),
                latency: Some(3.0),
            },
            Row {
                q: q(40.0, 50.0),
                seen: None,
                latency: None,
            },
        ];
        let text = render(&rows, 2.0);
        assert!(text.contains("3.00*"), "{text}");
        assert!(text.contains("missed"), "{text}");
        assert!(text.contains("NOT met"), "{text}");
        assert!(text.contains("stage medians"), "{text}");
    }
}
