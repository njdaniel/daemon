//! The fast phase (#12) for `dmn replay` and `dmn run`: every detector candidate goes through
//! `dmn-llm`'s fast phase on a thread of its own. `dmn replay` also prints the question +
//! approach JSON when it is ready ([`Output::Print`]); under `dmn run`'s TUI nothing is printed
//! ([`Output::Quiet`]) and the session the TUI mirrors is the only output.
//!
//! The thread owns a current-thread tokio runtime, so the pipeline loop stays synchronous and the
//! audio path never waits on HTTP. Candidates are handled in order, one request at a time.
//! Under `dmn run` ([`Policy::Supersede`], SPEC § Threading rules) a newer unpinned question
//! cancels the request in flight and any older one still queued is never sent; `dmn replay`
//! ([`Policy::AnswerAll`]) answers every candidate so its output covers each one.
//! [`Suggester::stop`] cancels everything: `q` in `dmn run` drops the in-flight request and
//! whatever is still queued.
//!
//! `r` (regenerate) is a job like any other ([`Suggester::regenerate`]): the same candidate id,
//! the question on screen as the current question, temperature 0.7 and the "different angle"
//! note (`dmn-llm`'s `FastRequest::regenerate`). Its fields replace that question in place.
//!
//! Ollama is optional. When it is unreachable (or `[llm].provider` is not one M1 wires), one
//! warning goes to stderr (to the log file under the TUI), the model dot goes red, and the rest of
//! the run transcribes without suggestions. Any other failure (a timeout, malformed JSON) is one
//! warning for that candidate.
//!
//! This module maps between crates: `dmn-llm` knows nothing of `dmn-detect` or `dmn-core`.
//! Every published field and every outcome is also dispatched into the session (#13), so the
//! session log gets the same actions a live call's fast phase would produce.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_core::{config, wire};
use dmn_detect::Candidate;
use dmn_llm::fast::FAST_PHASE_TIMEOUT;
use dmn_llm::{
    CancellationToken, CurrentQuestion, FastEvent, FastOutcome, FastRequest, IgnoreReason,
    LlmError, OllamaProvider, OllamaSettings, Provider, Suggestion, run_fast_phase,
};
use dmn_session::{Action, Dot, Health, SessionHub};

/// Where the fast phase's results go besides the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `dmn replay`: print each result, readable or (with `json`) one NDJSON line; warnings to
    /// stderr.
    Print { json: bool },
    /// `dmn run`: print nothing (the TUI owns the terminal); warnings go to the log file.
    Quiet,
}

impl Output {
    fn json(self) -> bool {
        matches!(self, Output::Print { json: true })
    }

    fn warn(self, msg: &str) {
        match self {
            Output::Print { .. } => eprintln!("replay: {msg}"),
            Output::Quiet => tracing::warn!("{msg}"),
        }
    }
}

/// The error a superseded candidate's `Failed` action carries.
const SUPERSEDED: &str = "superseded by a newer question";

/// One candidate and the last 60 s of transcript at the moment it was found.
struct Job {
    /// The session's candidate id, which tags every action for this candidate.
    id: u64,
    candidate: Candidate,
    transcript: String,
    /// `r`: the question on screen to answer again from a different angle.
    regenerate: Option<CurrentQuestion>,
}

/// How the fast phase went over the whole run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub shown: usize,
    pub clarifications: usize,
    pub ignored: usize,
    pub failed: usize,
    /// Candidates never sent because the LLM was unavailable, or because the run was stopped.
    pub skipped: usize,
    /// How many of the shown replies were regenerations.
    pub regenerated: usize,
    /// Requests cancelled, or never sent, because a newer unpinned question arrived.
    pub superseded: usize,
}

/// Whether a newer question replaces an older one's request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// SPEC § Threading rules, for `dmn run`: a newer unpinned question cancels the request in
    /// flight, and a candidate still queued behind a newer one is never sent. While pinned,
    /// new questions queue on screen, so every one is answered.
    Supersede,
    /// `dmn replay`: answer every candidate in order, so the output covers each one.
    AnswerAll,
}

/// What a newer question needs to reach the request it replaces.
#[derive(Default)]
struct Latest {
    /// The newest candidate submitted (regenerations are not new questions).
    newest: AtomicU64,
    /// The request in flight, if any.
    inflight: Mutex<Option<CancellationToken>>,
}

impl Latest {
    fn inflight(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Suggester {
    tx: mpsc::Sender<Job>,
    cancel: CancellationToken,
    handle: JoinHandle<Tally>,
    hub: Arc<SessionHub>,
    /// `Some` under [`Policy::Supersede`].
    latest: Option<Arc<Latest>>,
}

impl Suggester {
    /// `clock` stamps each outcome in session time (`--speed` × wall clock).
    pub fn start(
        llm: &config::Llm,
        output: Output,
        policy: Policy,
        hub: Arc<SessionHub>,
        clock: wire::SessionClock,
    ) -> Self {
        let llm = llm.clone();
        Self::spawn(policy, hub, move |rx, hub, latest, cancel| {
            worker(&llm, output, rx, hub, clock, cancel, latest)
        })
    }

    /// Serve jobs on `provider` directly, with no warm-up: the tests' way in.
    #[cfg(test)]
    fn start_with<P: Provider + Send + 'static>(
        provider: P,
        policy: Policy,
        hub: Arc<SessionHub>,
    ) -> Self {
        Self::spawn(policy, hub, move |rx, hub, latest, cancel| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let answerer = Answerer {
                rt: &rt,
                provider: &provider,
                hub,
                clock: wire::SessionClock::start(1.0),
                output: Output::Quiet,
                cancel,
                latest,
            };
            let mut tally = Tally::default();
            let mut current = None;
            while let Ok(job) = rx.recv() {
                let _ = answerer.answer(job, &mut current, &mut tally);
            }
            tally
        })
    }

    fn spawn(
        policy: Policy,
        hub: Arc<SessionHub>,
        serve: impl FnOnce(
            &mpsc::Receiver<Job>,
            &SessionHub,
            Option<&Latest>,
            &CancellationToken,
        ) -> Tally
        + Send
        + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        let latest = (policy == Policy::Supersede).then(|| Arc::new(Latest::default()));
        let (token, shared, worker_hub) = (cancel.clone(), latest.clone(), Arc::clone(&hub));
        let handle = std::thread::spawn(move || serve(&rx, &worker_hub, shared.as_deref(), &token));
        Self {
            tx,
            cancel,
            handle,
            hub,
            latest,
        }
    }

    /// Queue candidate `id` for the fast phase. Never blocks the pipeline loop. Under
    /// [`Policy::Supersede`], unless the session is pinned, this cancels the request in flight.
    pub fn submit(&self, id: u64, candidate: &Candidate, transcript: String) {
        if let Some(latest) = &self.latest {
            latest.newest.fetch_max(id, Ordering::AcqRel);
            if !self.hub.snapshot().pinned
                && let Some(inflight) = latest.inflight().as_ref()
            {
                inflight.cancel();
            }
        }
        self.send(Job {
            id,
            candidate: candidate.clone(),
            transcript,
            regenerate: None,
        });
    }

    /// Queue a regeneration of candidate `id`, whose question `current` is on screen.
    pub fn regenerate(
        &self,
        id: u64,
        candidate: &Candidate,
        transcript: String,
        current: CurrentQuestion,
    ) {
        self.send(Job {
            id,
            candidate: candidate.clone(),
            transcript,
            regenerate: Some(current),
        });
    }

    fn send(&self, job: Job) {
        // A send only fails once the worker has exited, which it does only after the sender
        // drops or the run is stopped; nothing to do then.
        let _ = self.tx.send(job);
    }

    /// Wait for every queued candidate to be answered.
    pub fn finish(self) -> Tally {
        drop(self.tx);
        self.handle.join().unwrap_or_default()
    }

    /// Cancel the in-flight request, skip everything still queued, and wait for the thread.
    pub fn stop(self) -> Tally {
        self.cancel.cancel();
        self.finish()
    }
}

fn worker(
    llm: &config::Llm,
    output: Output,
    rx: &mpsc::Receiver<Job>,
    hub: &SessionHub,
    clock: wire::SessionClock,
    cancel: &CancellationToken,
    latest: Option<&Latest>,
) -> Tally {
    let mut tally = Tally::default();
    let model_down = |why: String| {
        hub.dispatch(Action::Status {
            dot: Dot::Model,
            health: Health::Error(why),
        });
    };
    // Every candidate still queued (and every later one) is closed with a Failed action, so
    // the session does not sit on "loading" for a request that will never be made.
    let skip_all = |tally: &mut Tally, why: &str| {
        for job in rx.iter() {
            hub.dispatch(Action::Failed {
                candidate: job.id,
                error: why.into(),
            });
            tally.skipped += 1;
        }
    };

    if llm.provider != config::Provider::Ollama {
        output.warn(&format!(
            "no suggestions: [llm].provider {:?} is not wired until M2; transcribing only",
            llm.provider
        ));
        model_down(format!("{:?} is not wired until M2", llm.provider));
        skip_all(&mut tally, "LLM unavailable");
        return tally;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            output.warn(&format!(
                "no suggestions: cannot start the async runtime: {e}"
            ));
            model_down(e.to_string());
            skip_all(&mut tally, "LLM unavailable");
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
            output.warn(&format!("no suggestions: {e}; transcribing only"));
            model_down(e.to_string());
            skip_all(&mut tally, "LLM unavailable");
            return tally;
        }
    };
    // Loading the model can take tens of seconds the first time; candidates queue meanwhile.
    let warm = rt.block_on(async {
        tokio::select! {
            r = provider.warm_up() => Some(r),
            () = cancel.cancelled() => None,
        }
    });
    match warm {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            output.warn(&format!(
                "no suggestions: {e}; transcribing only (fix: start Ollama and `ollama pull {}`)",
                settings.model
            ));
            model_down(e.to_string());
            skip_all(&mut tally, "LLM unavailable");
            return tally;
        }
        None => {
            skip_all(&mut tally, "stopped");
            return tally;
        }
    }
    hub.dispatch(Action::Status {
        dot: Dot::Model,
        health: Health::Ok,
    });

    let answerer = Answerer {
        rt: &rt,
        provider: &provider,
        hub,
        clock,
        output,
        cancel,
        latest,
    };
    let mut current: Option<CurrentQuestion> = None;
    while let Ok(job) = rx.recv() {
        if let Err(e) = answerer.answer(job, &mut current, &mut tally) {
            output.warn(&format!(
                "no suggestions from here on: {e}; transcribing only"
            ));
            model_down(e.to_string());
            skip_all(&mut tally, "LLM unavailable");
            return tally;
        }
    }
    tally
}

/// Runs one job at a time against a provider (Ollama in `dmn`, a `MockProvider` in tests).
struct Answerer<'a, P: Provider> {
    rt: &'a tokio::runtime::Runtime,
    provider: &'a P,
    hub: &'a SessionHub,
    clock: wire::SessionClock,
    output: Output,
    cancel: &'a CancellationToken,
    /// `Some` under [`Policy::Supersede`].
    latest: Option<&'a Latest>,
}

/// The request for `job`: a new candidate (answered with the question on screen as context, so
/// a clarification can attach to it), or a regeneration of the question on screen.
fn request(job: &Job, current: Option<&CurrentQuestion>) -> FastRequest {
    FastRequest {
        transcript: job.transcript.clone(),
        current: job.regenerate.clone().or_else(|| current.cloned()),
        regenerate: job.regenerate.is_some(),
        ..FastRequest::new(job.candidate.text.clone(), job.candidate.score)
    }
}

impl<P: Provider> Answerer<'_, P> {
    /// Answer one job and dispatch everything it produced. `Err` only for an unreachable
    /// provider, after which nothing more is sent.
    fn answer(
        &self,
        job: Job,
        current: &mut Option<CurrentQuestion>,
        tally: &mut Tally,
    ) -> Result<(), LlmError> {
        let hub = self.hub;
        if self.cancel.is_cancelled() {
            hub.dispatch(Action::Failed {
                candidate: job.id,
                error: "stopped".into(),
            });
            tally.skipped += 1;
            return Ok(());
        }
        let regenerating = job.regenerate.is_some();
        // Publish this request's token before looking at `newest`: `submit` bumps `newest`
        // before it looks at the token, so a newer question either sees this token and cancels
        // it, or this check sees the newer question.
        let token = self.cancel.child_token();
        if let Some(latest) = self.latest {
            *latest.inflight() = Some(token.clone());
        }
        // A question still queued behind a newer one is never sent, unless pinned (then both
        // are on screen). A regeneration is the user asking about the question on screen, so
        // it is always sent.
        if let Some(latest) = self.latest
            && !regenerating
            && latest.newest.load(Ordering::Acquire) > job.id
            && !hub.snapshot().pinned
        {
            *latest.inflight() = None;
            hub.dispatch(Action::Failed {
                candidate: job.id,
                error: SUPERSEDED.into(),
            });
            tally.superseded += 1;
            return Ok(());
        }
        let req = request(&job, current.as_ref());
        let started = Instant::now();
        let mut question_at = None;
        let mut first_line_at = None;
        let result = self
            .rt
            .block_on(run_fast_phase(self.provider, &req, token.clone(), |e| {
                match e {
                    FastEvent::Question { .. } | FastEvent::Clarification { .. } => {
                        question_at.get_or_insert_with(|| started.elapsed());
                    }
                    FastEvent::ApproachLine { .. } => {
                        first_line_at.get_or_insert_with(|| started.elapsed());
                    }
                    FastEvent::Focus { .. } | FastEvent::Star(_) => {}
                }
                hub.dispatch(wire::fast_event(job.id, &e));
            }));
        if let Some(latest) = self.latest {
            *latest.inflight() = None;
        }
        // Cancelled by a newer question, not by `q`: say so in the session and the tally.
        let superseded = matches!(result, Err(LlmError::Cancelled))
            && token.is_cancelled()
            && !self.cancel.is_cancelled();
        if superseded {
            hub.dispatch(Action::Failed {
                candidate: job.id,
                error: SUPERSEDED.into(),
            });
            tally.superseded += 1;
            return Ok(());
        }
        hub.dispatch(wire::outcome(job.id, &result, self.clock.now()));
        // SPEC § Transcript & status: a failed request turns the model dot red with its reason
        // (the dots are the only place errors show); the next good reply turns it green again.
        let health = match &result {
            Ok(_) => Some(Health::Ok),
            Err(LlmError::Cancelled) if self.cancel.is_cancelled() => None,
            Err(e) => Some(Health::Error(e.to_string())),
        };
        if let Some(health) = health
            && hub.snapshot().status.model != health
        {
            hub.dispatch(Action::Status {
                dot: Dot::Model,
                health,
            });
        }
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
                    self.output,
                );
                if clarification {
                    tally.clarifications += 1;
                } else {
                    tally.shown += 1;
                    if regenerating {
                        tally.regenerated += 1;
                    }
                    if let Some(kind) = suggestion.kind {
                        *current = Some(CurrentQuestion {
                            question: suggestion.question.clone(),
                            kind,
                        });
                    }
                }
            }
            Ok(FastOutcome::Ignored { reason, score }) => {
                print_ignored(&job.candidate, reason, score, timing, self.output);
                tally.ignored += 1;
            }
            Err(e @ LlmError::Unreachable { .. }) => {
                tally.failed += 1;
                return Err(e);
            }
            Err(e) => {
                if !self.cancel.is_cancelled() {
                    self.output.warn(&format!(
                        "fast phase failed for {:?} after {}: {e}",
                        job.candidate.text,
                        secs(timing.total)
                    ));
                }
                if self.output.json() {
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
        Ok(())
    }
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
    output: Output,
) {
    let Output::Print { json } = output else {
        return;
    };
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

fn print_ignored(c: &Candidate, reason: IgnoreReason, score: i32, t: Timing, output: Output) {
    let Output::Print { json } = output else {
        return;
    };
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
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start(
            &llm,
            Output::Print { json: true },
            Policy::AnswerAll,
            Arc::clone(&hub),
            wire::SessionClock::start(1.0),
        );
        for id in [1, 2] {
            hub.dispatch(wire::candidate(id, &candidate()));
            s.submit(id, &candidate(), String::new());
        }
        let tally = s.finish();
        assert_eq!(tally.shown + tally.ignored + tally.clarifications, 0);
        assert_eq!(tally.skipped + tally.failed, 2, "{tally:?}");
        let session = hub.snapshot();
        assert!(
            matches!(session.status.model, Health::Error(_)),
            "the model dot goes red"
        );
        assert_eq!(session.pending, None, "no candidate is left loading");
        assert_eq!(session.counts.failed, 2);
    }

    #[test]
    fn a_remote_provider_is_not_called_in_m1() {
        let llm = config::Llm {
            provider: config::Provider::Anthropic,
            ..config::Llm::default()
        };
        let s = Suggester::start(
            &llm,
            Output::Quiet,
            Policy::AnswerAll,
            Arc::new(SessionHub::default()),
            wire::SessionClock::start(1.0),
        );
        s.submit(1, &candidate(), String::new());
        assert_eq!(
            s.finish(),
            Tally {
                skipped: 1,
                ..Tally::default()
            }
        );
    }

    #[test]
    fn stop_skips_what_is_queued_without_waiting_for_the_model() {
        // An address that never answers: without the cancel, warm-up would sit on the
        // connect timeout. Stop must return promptly and close every queued candidate.
        let llm = config::Llm {
            base_url: "http://10.255.255.1:11434".into(),
            ..config::Llm::default()
        };
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start(
            &llm,
            Output::Quiet,
            Policy::Supersede,
            Arc::clone(&hub),
            wire::SessionClock::start(1.0),
        );
        for id in [1, 2, 3] {
            hub.dispatch(wire::candidate(id, &candidate()));
            s.submit(id, &candidate(), String::new());
        }
        let t = Instant::now();
        let tally = s.stop();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert_eq!(tally.skipped + tally.failed, 3, "{tally:?}");
        assert_eq!(hub.snapshot().pending, None, "nothing left loading");
    }

    #[test]
    fn regenerate_asks_at_temperature_0_7_for_another_angle_and_replaces_in_place() {
        let provider = dmn_llm::MockProvider::json(&serde_json::json!({
            "is_question": true,
            "kind": "system_design",
            "question": "How would you design a rate limiter, another way?",
            "focus": "tradeoffs",
            "approach": ["Start from the failure modes.", "Then pick the algorithm."]
        }));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hub = SessionHub::default();
        let cancel = CancellationToken::new();
        let answerer = Answerer {
            rt: &rt,
            provider: &provider,
            hub: &hub,
            clock: wire::SessionClock::start(1.0),
            output: Output::Quiet,
            cancel: &cancel,
            latest: None,
        };
        // Candidate 1 is on screen, answered once already.
        hub.dispatch(wire::candidate(1, &candidate()));
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "How would you design a rate limiter?".into(),
            kind: "system_design".into(),
        });
        hub.dispatch(Action::ApproachLine {
            candidate: 1,
            index: 0,
            text: "Token bucket per key.".into(),
        });
        hub.dispatch(Action::Regenerate { candidate: 1 });
        assert!(hub.snapshot().live.unwrap().regenerating);

        let job = Job {
            id: 1,
            candidate: candidate(),
            transcript: "Interviewer: How would you design a rate limiter?".into(),
            regenerate: Some(CurrentQuestion {
                question: "How would you design a rate limiter?".into(),
                kind: dmn_llm::Kind::SystemDesign,
            }),
        };
        let mut tally = Tally::default();
        answerer.answer(job, &mut None, &mut tally).unwrap();

        let sent = provider.requests();
        assert_eq!(sent.len(), 1);
        assert!(
            (sent[0].temperature - 0.7).abs() < f32::EPSILON,
            "{}",
            sent[0].temperature
        );
        let user = &sent[0].messages.last().unwrap().content;
        assert!(user.contains("different angle"), "{user}");

        let live = hub.snapshot().live.unwrap();
        assert_eq!(live.candidate, 1, "same question, replaced in place");
        assert_eq!(
            live.text,
            "How would you design a rate limiter, another way?"
        );
        assert_eq!(live.approach.len(), 2);
        assert!(!live.regenerating);
        assert_eq!(tally.regenerated, 1);
    }

    #[test]
    fn a_failed_request_turns_the_model_dot_red_and_a_good_one_green_again() {
        let hub = Arc::new(SessionHub::default());
        let broken =
            dmn_llm::MockProvider::new(vec![dmn_llm::mock::Step::Error("model crashed".into())]);
        let s = Suggester::start_with(broken, Policy::AnswerAll, Arc::clone(&hub));
        submit_with_candidate(&s, &hub, 1);
        assert_eq!(s.finish().failed, 1);
        let dot = hub.snapshot().status.model;
        assert!(
            matches!(&dot, Health::Error(why) if why.contains("model crashed")),
            "{dot:?}"
        );

        let s = Suggester::start_with(slow_reply("Q"), Policy::AnswerAll, Arc::clone(&hub));
        submit_with_candidate(&s, &hub, 2);
        assert_eq!(s.finish().shown, 1);
        assert_eq!(hub.snapshot().status.model, Health::Ok);
    }

    /// A reply that streams for about 2 s (50 ms per 7-character chunk).
    fn slow_reply(question: &str) -> dmn_llm::MockProvider {
        dmn_llm::MockProvider::json(&serde_json::json!({
            "is_question": true,
            "kind": "system_design",
            "question": question,
            "focus": "tradeoffs",
            "approach": [
                "Clarify the requirements and the scale first.",
                "Pick the core data model and the write path.",
                "Name the failure modes and how each degrades."
            ]
        }))
        .with_delay(Duration::from_millis(50))
    }

    fn submit_with_candidate(s: &Suggester, hub: &SessionHub, id: u64) {
        hub.dispatch(wire::candidate(id, &candidate()));
        s.submit(id, &candidate(), String::new());
    }

    #[test]
    fn a_newer_unpinned_question_cancels_the_one_in_flight_and_skips_the_queue() {
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with(slow_reply("Q"), Policy::Supersede, Arc::clone(&hub));
        submit_with_candidate(&s, &hub, 1);
        // Candidate 1 is streaming; 2 queues behind it, then 3 arrives.
        std::thread::sleep(Duration::from_millis(300));
        submit_with_candidate(&s, &hub, 2);
        submit_with_candidate(&s, &hub, 3);
        let tally = s.finish();
        assert_eq!(
            tally.superseded, 2,
            "1 cancelled in flight, 2 never sent: {tally:?}"
        );
        assert_eq!(tally.shown, 1, "{tally:?}");
        let session = hub.snapshot();
        assert_eq!(session.live.map(|q| q.candidate), Some(3));
        assert_eq!(session.pending, None, "nothing left loading");
    }

    #[test]
    fn while_pinned_every_question_is_answered() {
        let hub = Arc::new(SessionHub::default());
        hub.dispatch(Action::Question {
            candidate: 0,
            text: "Pinned".into(),
            kind: "system_design".into(),
        });
        hub.dispatch(Action::Pin);
        let s = Suggester::start_with(slow_reply("Q"), Policy::Supersede, Arc::clone(&hub));
        submit_with_candidate(&s, &hub, 1);
        std::thread::sleep(Duration::from_millis(300));
        submit_with_candidate(&s, &hub, 2);
        let tally = s.finish();
        assert_eq!((tally.shown, tally.superseded), (2, 0), "{tally:?}");
        let session = hub.snapshot();
        assert_eq!(session.live.map(|q| q.candidate), Some(0), "still pinned");
        assert_eq!(session.queued.len(), 2);
    }

    #[test]
    fn replay_answers_every_candidate() {
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with(slow_reply("Q"), Policy::AnswerAll, Arc::clone(&hub));
        submit_with_candidate(&s, &hub, 1);
        std::thread::sleep(Duration::from_millis(300));
        submit_with_candidate(&s, &hub, 2);
        let tally = s.finish();
        assert_eq!((tally.shown, tally.superseded), (2, 0), "{tally:?}");
    }

    #[test]
    fn a_new_candidate_is_asked_at_temperature_0() {
        let job = Job {
            id: 2,
            candidate: candidate(),
            transcript: String::new(),
            regenerate: None,
        };
        let req = request(&job, None);
        assert!(!req.regenerate);
        let chat = dmn_llm::fast::fast_chat_request(&req);
        assert_eq!(chat.temperature, 0.0);
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
