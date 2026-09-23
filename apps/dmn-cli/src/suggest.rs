//! Replay's fast phase (#12): every detector candidate goes through `dmn-llm`'s fast phase on a
//! thread of its own, and the question + approach JSON is printed when it is ready.
//!
//! The thread owns a current-thread tokio runtime, so the replay loop stays synchronous and the
//! audio path never waits on HTTP. Candidates are handled in order, one request at a time. The
//! live pipeline cancels an in-flight request when a newer unpinned question arrives; replay
//! instead answers every candidate so the output covers each one.
//!
//! Ollama is optional here. When it is unreachable (or `[llm].provider` is not one M1 wires),
//! one warning goes to stderr and the rest of the run transcribes without suggestions. Any other
//! failure (a timeout, malformed JSON) is one warning line for that candidate.
//!
//! This module maps between crates: `dmn-llm` knows nothing of `dmn-detect` or `dmn-core`.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_core::config;
use dmn_detect::Candidate;
use dmn_llm::fast::FAST_PHASE_TIMEOUT;
use dmn_llm::{
    CancellationToken, CurrentQuestion, FastEvent, FastOutcome, FastRequest, IgnoreReason,
    LlmError, OllamaProvider, OllamaSettings, Provider, Suggestion, run_fast_phase,
};

/// One candidate and the last 60 s of transcript at the moment it was found.
struct Job {
    candidate: Candidate,
    transcript: String,
}

/// How the fast phase went over the whole replay.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub shown: usize,
    pub clarifications: usize,
    pub ignored: usize,
    pub failed: usize,
    /// Candidates never sent because the LLM was unavailable.
    pub skipped: usize,
}

pub struct Suggester {
    tx: mpsc::Sender<Job>,
    handle: JoinHandle<Tally>,
}

impl Suggester {
    pub fn start(llm: &config::Llm, json: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let llm = llm.clone();
        let handle = std::thread::spawn(move || worker(&llm, json, &rx));
        Self { tx, handle }
    }

    /// Queue `candidate` for the fast phase. Never blocks the replay loop.
    pub fn submit(&self, candidate: &Candidate, transcript: String) {
        // A send only fails once the worker has exited, which it does only after the sender
        // drops; nothing to do then.
        let _ = self.tx.send(Job {
            candidate: candidate.clone(),
            transcript,
        });
    }

    /// Wait for every queued candidate to be answered.
    pub fn finish(self) -> Tally {
        drop(self.tx);
        self.handle.join().unwrap_or_default()
    }
}

fn warn(msg: &str) {
    eprintln!("replay: {msg}");
}

fn worker(llm: &config::Llm, json: bool, rx: &mpsc::Receiver<Job>) -> Tally {
    let mut tally = Tally::default();
    let skip_all = |tally: &mut Tally| tally.skipped += rx.iter().count();

    if llm.provider != config::Provider::Ollama {
        warn(&format!(
            "no suggestions: [llm].provider {:?} is not wired until M2; transcribing only",
            llm.provider
        ));
        skip_all(&mut tally);
        return tally;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn(&format!(
                "no suggestions: cannot start the async runtime: {e}"
            ));
            skip_all(&mut tally);
            return tally;
        }
    };
    let settings = OllamaSettings {
        base_url: llm.base_url.clone(),
        model: llm.model.clone(),
        ..OllamaSettings::default()
    };
    let provider = match OllamaProvider::from_settings(&settings) {
        Ok(p) => p,
        Err(e) => {
            warn(&format!("no suggestions: {e}; transcribing only"));
            skip_all(&mut tally);
            return tally;
        }
    };
    // Loading the model can take tens of seconds the first time; candidates queue meanwhile.
    if let Err(e) = rt.block_on(provider.warm_up()) {
        warn(&format!(
            "no suggestions: {e}; transcribing only (fix: start Ollama and `ollama pull {}`)",
            settings.model
        ));
        skip_all(&mut tally);
        return tally;
    }

    let mut current: Option<CurrentQuestion> = None;
    while let Ok(job) = rx.recv() {
        let req = FastRequest {
            transcript: job.transcript,
            current: current.clone(),
            ..FastRequest::new(job.candidate.text.clone(), job.candidate.score)
        };
        let started = Instant::now();
        let mut question_at = None;
        let mut first_line_at = None;
        let result = rt.block_on(run_fast_phase(
            &provider,
            &req,
            CancellationToken::new(),
            |e| match e {
                FastEvent::Question { .. } | FastEvent::Clarification { .. } => {
                    question_at.get_or_insert_with(|| started.elapsed());
                }
                FastEvent::ApproachLine { .. } => {
                    first_line_at.get_or_insert_with(|| started.elapsed());
                }
                FastEvent::Focus { .. } | FastEvent::Star(_) => {}
            },
        ));
        let timing = Timing {
            question: question_at,
            first_line: first_line_at,
            total: started.elapsed(),
        };
        match result {
            Ok(FastOutcome::Shown {
                suggestion,
                clarification,
                flags,
            }) => {
                let flags: Vec<String> = flags.into_iter().map(|f| f.field).collect();
                print_shown(
                    &job.candidate,
                    &suggestion,
                    clarification,
                    &flags,
                    timing,
                    json,
                );
                if clarification {
                    tally.clarifications += 1;
                } else {
                    tally.shown += 1;
                    if let Some(kind) = suggestion.kind {
                        current = Some(CurrentQuestion {
                            question: suggestion.question.clone(),
                            kind,
                        });
                    }
                }
            }
            Ok(FastOutcome::Ignored { reason, score }) => {
                print_ignored(&job.candidate, reason, score, timing, json);
                tally.ignored += 1;
            }
            Err(e @ LlmError::Unreachable { .. }) => {
                warn(&format!(
                    "no suggestions from here on: {e}; transcribing only"
                ));
                tally.failed += 1;
                skip_all(&mut tally);
                return tally;
            }
            Err(e) => {
                warn(&format!(
                    "fast phase failed for {:?} after {}: {e}",
                    job.candidate.text,
                    secs(timing.total)
                ));
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "suggestion",
                            "outcome": "error",
                            "start": job.candidate.start.as_secs_f64(),
                            "text": job.candidate.text,
                            "error": e.to_string(),
                            "total_ms": ms(timing.total),
                        })
                    );
                }
                tally.failed += 1;
            }
        }
    }
    tally
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    question: Option<Duration>,
    first_line: Option<Duration>,
    total: Duration,
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

/// The fast-phase fields of `s`, the only ones this phase fills.
fn fast_json(s: &Suggestion) -> serde_json::Value {
    let mut v = serde_json::json!({
        "question": s.question,
        "kind": s.kind,
        "focus": s.focus,
        "approach": s.approach,
    });
    if let (Some(star), Some(obj)) = (&s.star, v.as_object_mut()) {
        obj.insert("star".into(), serde_json::json!(star));
    }
    v
}

fn print_shown(
    c: &Candidate,
    s: &Suggestion,
    clarification: bool,
    flags: &[String],
    t: Timing,
    json: bool,
) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "suggestion",
                "outcome": if clarification { "clarification" } else { "shown" },
                "start": c.start.as_secs_f64(),
                "text": c.text,
                "suggestion": fast_json(s),
                "flags": flags,
                "question_ms": t.question.map(ms),
                "first_line_ms": t.first_line.map(ms),
                "total_ms": ms(t.total),
            })
        );
        return;
    }
    let what = if clarification {
        "clarification"
    } else {
        "question"
    };
    let over = if t.total > FAST_PHASE_TIMEOUT {
        " (over budget)"
    } else {
        ""
    };
    let cut = if flags.is_empty() {
        String::new()
    } else {
        format!(", cut: {}", flags.join(" "))
    };
    println!(
        "  => {what} at {}, first line at {}, done at {}{over}{cut}\n     {}",
        t.question.map(secs).unwrap_or_else(|| "-".into()),
        t.first_line.map(secs).unwrap_or_else(|| "-".into()),
        secs(t.total),
        fast_json(s)
    );
}

fn print_ignored(c: &Candidate, reason: IgnoreReason, score: i32, t: Timing, json: bool) {
    let reason = match reason {
        IgnoreReason::NotAQuestion => "not_a_question",
        IgnoreReason::SmallTalk => "small_talk",
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "suggestion",
                "outcome": "ignored",
                "start": c.start.as_secs_f64(),
                "text": c.text,
                "reason": reason,
                "score": score,
                "total_ms": ms(t.total),
            })
        );
    } else {
        println!(
            "  => not shown ({reason}, score {score}) after {}",
            secs(t.total)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Candidate {
        Candidate {
            source: dmn_audio::Source::Interviewer,
            start: Duration::from_secs(1),
            end: Duration::from_secs(3),
            text: "How would you design a rate limiter?".into(),
            line: "So. How would you design a rate limiter?".into(),
            score: 5,
            cues: Vec::new(),
        }
    }

    #[test]
    fn unreachable_ollama_skips_every_candidate_without_failing() {
        let llm = config::Llm {
            // Port 9 (discard) on localhost: refused at once, no waiting on a timeout.
            base_url: "http://127.0.0.1:9".into(),
            ..config::Llm::default()
        };
        let s = Suggester::start(&llm, true);
        s.submit(&candidate(), String::new());
        s.submit(&candidate(), String::new());
        let tally = s.finish();
        assert_eq!(tally.shown + tally.ignored + tally.clarifications, 0);
        assert_eq!(tally.skipped + tally.failed, 2, "{tally:?}");
    }

    #[test]
    fn a_remote_provider_is_not_called_in_m1() {
        let llm = config::Llm {
            provider: config::Provider::Anthropic,
            ..config::Llm::default()
        };
        let s = Suggester::start(&llm, false);
        s.submit(&candidate(), String::new());
        assert_eq!(
            s.finish(),
            Tally {
                skipped: 1,
                ..Tally::default()
            }
        );
    }

    #[test]
    fn fast_json_holds_only_the_fast_fields() {
        let s = Suggestion {
            question: "q".into(),
            kind: Some(dmn_llm::Kind::Coding),
            focus: "f".into(),
            approach: vec!["a".into()],
            followups: vec!["never printed".into()],
            ..Suggestion::default()
        };
        let v = fast_json(&s);
        assert_eq!(
            v,
            serde_json::json!({"question": "q", "kind": "coding", "focus": "f", "approach": ["a"]})
        );
    }
}
