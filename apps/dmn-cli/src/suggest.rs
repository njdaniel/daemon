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
//!
//! #54: after the warm-up and before the first question, the thread loads the context
//! ([`crate::context`]): the stored sources and the job requirements, summarising a new job
//! description once. Every request then carries the requirements and the chunks retrieved for
//! its question, and `dmn replay --json` records which chunks went with each candidate.
//!
//! #55, the slow phase: once a question is shown, a second request fills its six expandable
//! sections. The fast phase always wins the model. Ollama serves one request at a time
//! (`OLLAMA_NUM_PARALLEL=1`), so a slow request in flight (up to 20 s) would hold the next
//! question's fast phase behind it. Every job handed to the thread therefore cancels the slow
//! request in flight first ([`SlowGate`]), and the thread runs a slow phase only when no job is
//! waiting. An interrupted slow phase goes back in line and starts again later; under
//! [`Policy::Supersede`] only questions still on screen (live, or queued behind a pin) get one,
//! the live question first. `dmn replay` ([`Policy::AnswerAll`]) fills every question's sections,
//! finishing the ones still waiting after the last candidate.
//!
//! #57, session memory: every answered question gets one summary line ([`Action::Summary`]). A
//! question is summarised once the candidate has answered it, so the work rides on the next
//! question's slow phase: the pipeline hands each candidate the question on screen and what the
//! candidate said since it was asked ([`Answered`]), and that slow phase returns the line and any
//! promise ("I'll come back to clock skew") with no extra request. A question whose line never
//! comes that way (the slow phase is off, cut, or the session ended first) gets a fallback line
//! from its own text ([`dmn_context::memory::fallback`]). Every fast and slow request carries the
//! session's last 8 lines.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_core::config::Config;
use dmn_core::{config, wire};
use dmn_detect::Candidate;
use dmn_llm::fast::FAST_PHASE_TIMEOUT;
use dmn_llm::schema::Star;
use dmn_llm::{
    CancellationToken, CurrentQuestion, FastEvent, FastOutcome, FastRequest, IgnoreReason, Kind,
    LlmError, PreviousAnswer, Provider, SlowFields, SlowOutcome, SlowRequest, Suggestion,
    run_fast_phase, run_slow_phase,
};
use dmn_session::{Action, Dot, Health, SessionHub, Stage};

use crate::context::{Context, ContextInfo, ContextSource, Retrieval, StoryPick};

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

    /// Something the user must know but that is not a failure: which provider answers, and
    /// that transcript text leaves the machine (#59). `dmn run` shows it on the status line.
    fn note(self, msg: &str) {
        match self {
            Output::Print { .. } => eprintln!("replay: {msg}"),
            Output::Quiet => tracing::info!("{msg}"),
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
    /// Swap (#58): with `regenerate` set, answer the question on screen from the next story
    /// (the one the session now names), at temperature 0 and without the "different angle"
    /// note.
    swap: bool,
    /// #57: the question on screen when this candidate arrived, and the candidate's answer to it.
    answered: Option<Answered>,
    /// #63: this candidate is the rest of the question on screen, split at a pause. It is asked
    /// as a question of its own, without that half as the current question: the model would
    /// otherwise take the whole question for a clarification of its first half.
    continues: bool,
}

/// A shown question and what the candidate said while it was on screen (#57): what the next
/// question's slow phase summarises. Built by the pipeline, which has the whole transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Answered {
    /// Its candidate id.
    pub id: u64,
    pub question: String,
    pub focus: String,
    /// When it was asked, in audio seconds from the session start.
    pub at: f64,
    /// The candidate's words since then, oldest first.
    pub answer: String,
}

/// A shown question, as far as its summary line needs it (#57).
#[derive(Debug, Clone, PartialEq)]
struct Asked {
    id: u64,
    question: String,
    focus: String,
    at: f64,
}

impl From<&Answered> for Asked {
    fn from(a: &Answered) -> Self {
        Self {
            id: a.id,
            question: a.question.clone(),
            focus: a.focus.clone(),
            at: a.at,
        }
    }
}

/// Which shown questions still owe their summary line (#57).
#[derive(Debug, Default)]
struct Memory {
    shown: Vec<Asked>,
    summarised: HashSet<u64>,
}

impl Memory {
    fn shown(&mut self, job: &SlowJob) {
        if !self.shown.iter().any(|a| a.id == job.id) {
            self.shown.push(Asked {
                id: job.id,
                question: job.question.clone(),
                focus: job.focus.clone(),
                at: job.start,
            });
        }
    }
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
    /// What context was loaded (#54).
    pub context: ContextInfo,
    /// Retrievals run, and their slowest and total time (staleness check, reload, search).
    pub retrievals: usize,
    pub retrieval_slowest: Duration,
    pub retrieval_total: Duration,
    /// #55: slow phases that filled every section, that hit the deadline, that failed.
    pub slow_done: usize,
    pub slow_timed_out: usize,
    pub slow_failed: usize,
    /// Slow phases cut short because a new candidate needed the model, and started again.
    pub slow_restarts: usize,
    /// Shown questions whose slow phase never ran: superseded, or the run was stopped.
    pub slow_dropped: usize,
    /// Longest completed slow phase, request start to close.
    pub slow_slowest: Duration,
    /// #57: summary lines written, and how many of them were fallbacks (no model line).
    pub summaries: usize,
    pub summary_fallbacks: usize,
}

impl Tally {
    fn retrieved(&mut self, took: Duration) {
        self.retrievals += 1;
        self.retrieval_slowest = self.retrieval_slowest.max(took);
        self.retrieval_total += took;
    }

    /// Mean time per retrieval.
    pub fn retrieval_mean(&self) -> Duration {
        u32::try_from(self.retrievals)
            .ok()
            .filter(|&n| n > 0)
            .map_or(Duration::ZERO, |n| self.retrieval_total / n)
    }
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

/// What lets a new job reach a slow phase in flight (#55): how many jobs were handed over but not
/// yet taken, and the slow request's token.
///
/// A sender bumps `waiting` and then cancels `inflight`; the thread publishes a slow request's
/// token and then reads `waiting`. So a job handed over while a slow phase is starting either
/// finds its token and cancels it, or is seen by the thread before the request is sent.
#[derive(Default)]
struct SlowGate {
    waiting: AtomicU64,
    inflight: Mutex<Option<CancellationToken>>,
}

impl SlowGate {
    fn inflight(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A job is on its way: stop the slow phase, if one is running.
    fn arriving(&self) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        if let Some(token) = self.inflight().as_ref() {
            token.cancel();
        }
    }

    /// The thread took a job.
    fn taken(&self) {
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }

    fn job_waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst) > 0
    }
}

/// A shown question whose slow phase is still to run (#55).
struct SlowJob {
    id: u64,
    /// What the Interviewer said (the candidate text) and where it starts in the audio.
    said: String,
    start: f64,
    transcript: String,
    question: String,
    kind: Kind,
    focus: String,
    approach: Vec<String>,
    star: Option<Star>,
    /// When its fast phase finished.
    ready: Instant,
    /// Attempts cut short by a newer job.
    restarts: usize,
    /// #57: the question shown before this one, to summarise along the way.
    answered: Option<Answered>,
}

/// The part of the model not on the GPU, in whole percent; a sliver under 1 % is rounding.
fn cpu_percent(gpu_share: f32) -> u8 {
    let cpu = ((1.0 - gpu_share.clamp(0.0, 1.0)) * 100.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0.0 ..= 100.0
    let cpu = cpu as u8;
    cpu
}

pub struct Suggester {
    tx: mpsc::Sender<Job>,
    cancel: CancellationToken,
    handle: JoinHandle<Tally>,
    hub: Arc<SessionHub>,
    /// `Some` under [`Policy::Supersede`].
    latest: Option<Arc<Latest>>,
    gate: Arc<SlowGate>,
}

impl Suggester {
    /// `clock` stamps each outcome in session time (`--speed` × wall clock); `audio` stamps
    /// when each question and its first Approach line were produced, in audio time (#16).
    /// `context` is where the sources and job requirements are read from (#54). `config_file`,
    /// when given, is watched between requests: a change to `[llm]`'s provider, endpoint, model
    /// or key variable swaps the provider for the next request, without a restart (#59).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        llm: &config::Llm,
        output: Output,
        policy: Policy,
        hub: Arc<SessionHub>,
        clock: wire::SessionClock,
        audio: wire::AudioClock,
        context: ContextSource,
        config_file: Option<PathBuf>,
    ) -> Self {
        let llm = llm.clone();
        Self::spawn(policy, hub, move |ch| {
            let reload =
                config_file.map(|path| Reload::watch(path, &llm, Box::new(wire::provider)));
            worker(&llm, output, clock, audio, &ch, &context, reload)
        })
    }

    /// Serve jobs on `provider` directly, with no warm-up, no context and no slow phase: the
    /// tests' way in.
    #[cfg(test)]
    pub(crate) fn start_with<P: Provider + Send + 'static>(
        provider: P,
        policy: Policy,
        hub: Arc<SessionHub>,
    ) -> Self {
        Self::start_with_context(provider, policy, hub, Context::empty(), None)
    }

    /// Serve jobs like [`Suggester::start_with`], starting on `build(llm)` and watching
    /// `config_file` for `[llm]` changes: the reload path of `dmn run` with mock providers.
    #[cfg(test)]
    fn start_with_reload(
        llm: &config::Llm,
        config_file: PathBuf,
        build: Builder,
        policy: Policy,
        hub: Arc<SessionHub>,
    ) -> Self {
        let llm = llm.clone();
        Self::spawn(policy, hub, move |ch| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let provider = build(&llm).expect("first provider");
            let mut answerer = Answerer {
                rt: &rt,
                provider,
                hub: ch.hub,
                clock: wire::SessionClock::start(1.0),
                audio: wire::AudioClock::new(Instant::now(), 1.0),
                output: Output::Quiet,
                cancel: ch.cancel,
                latest: ch.latest,
                reload: Some(Reload::watch(config_file, &llm, build)),
            };
            ch.hub.dispatch(Action::Provider {
                label: wire::provider_label(&llm),
            });
            let mut context = Context::empty();
            let mut tally = Tally::default();
            let _ = serve(&mut answerer, &ch, None, &mut context, &mut tally);
            tally
        })
    }

    /// [`Suggester::start_with`] with `context` already loaded, and the slow phase on when
    /// `slow` gives its deadline.
    #[cfg(test)]
    fn start_with_context<P: Provider + Send + 'static>(
        provider: P,
        policy: Policy,
        hub: Arc<SessionHub>,
        mut context: Context,
        slow: Option<Duration>,
    ) -> Self {
        Self::spawn(policy, hub, move |ch| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let mut answerer = Answerer {
                rt: &rt,
                provider: Arc::new(provider),
                hub: ch.hub,
                clock: wire::SessionClock::start(1.0),
                audio: wire::AudioClock::new(Instant::now(), 1.0),
                output: Output::Quiet,
                cancel: ch.cancel,
                latest: ch.latest,
                reload: None,
            };
            let mut tally = Tally {
                context: context.info(),
                ..Tally::default()
            };
            let _ = serve(&mut answerer, &ch, slow, &mut context, &mut tally);
            tally
        })
    }

    fn spawn(
        policy: Policy,
        hub: Arc<SessionHub>,
        serve: impl FnOnce(Channels<'_>) -> Tally + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = CancellationToken::new();
        let latest = (policy == Policy::Supersede).then(|| Arc::new(Latest::default()));
        let gate = Arc::new(SlowGate::default());
        let (token, shared, worker_hub, worker_gate) = (
            cancel.clone(),
            latest.clone(),
            Arc::clone(&hub),
            Arc::clone(&gate),
        );
        let handle = std::thread::spawn(move || {
            serve(Channels {
                rx: &rx,
                hub: &worker_hub,
                latest: shared.as_deref(),
                cancel: &token,
                gate: &worker_gate,
            })
        });
        Self {
            tx,
            cancel,
            handle,
            hub,
            latest,
            gate,
        }
    }

    /// [`Suggester::submit_after`] with nothing answered before: the tests' way in.
    #[cfg(test)]
    pub fn submit(&self, id: u64, candidate: &Candidate, transcript: String) {
        self.submit_after(id, candidate, transcript, None, false);
    }

    /// Queue candidate `id` for the fast phase, with the question on screen and the candidate's
    /// answer to it for session memory (#57). Never blocks the pipeline loop. Under
    /// [`Policy::Supersede`], unless the session is pinned, this cancels the request in flight.
    /// `continues`: the candidate is the rest of the question on screen (#63).
    pub fn submit_after(
        &self,
        id: u64,
        candidate: &Candidate,
        transcript: String,
        answered: Option<Answered>,
        continues: bool,
    ) {
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
            swap: false,
            answered,
            continues,
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
            swap: false,
            answered: None,
            continues: false,
        });
    }

    /// Queue a swap (#58): candidate `id`'s STAR block again, from the story the session moved
    /// on to. Like [`Suggester::regenerate`], the reply replaces the question in place, and a
    /// newer question pre-empts it.
    pub fn swap(
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
            swap: true,
            answered: None,
            continues: false,
        });
    }

    fn send(&self, job: Job) {
        // #55: the fast phase always wins the model; a slow phase in flight stops first.
        self.gate.arriving();
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

/// What the serving thread shares with the [`Suggester`] that spawned it.
struct Channels<'a> {
    rx: &'a mpsc::Receiver<Job>,
    hub: &'a SessionHub,
    /// `Some` under [`Policy::Supersede`].
    latest: Option<&'a Latest>,
    cancel: &'a CancellationToken,
    gate: &'a SlowGate,
}

fn worker(
    llm: &config::Llm,
    output: Output,
    clock: wire::SessionClock,
    audio: wire::AudioClock,
    ch: &Channels<'_>,
    context: &ContextSource,
    reload: Option<Reload>,
) -> Tally {
    let (rx, hub, cancel) = (ch.rx, ch.hub, ch.cancel);
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
    // #59: whichever provider `[llm]` names; the API key is read from its env var here.
    let provider = match wire::provider(llm) {
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
            let fix = match llm.provider {
                config::Provider::Ollama => {
                    format!(" (fix: start Ollama and `ollama pull {}`)", llm.model)
                }
                _ => String::new(),
            };
            output.warn(&format!("no suggestions: {e}; transcribing only{fix}"));
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
    // #79: a model loaded after whisper took the GPU (assistance turned on mid-session) may not
    // fit; say how much of it runs from system memory rather than fail or hide it.
    if let Ok(Some(share)) = rt.block_on(provider.gpu_share()) {
        let cpu_percent = cpu_percent(share);
        if cpu_percent > 0 {
            tracing::warn!(cpu_percent, "LLM partly in system memory");
        }
        hub.dispatch(Action::LlmOffload { cpu_percent });
    }
    announce(llm, hub, output);

    // #54: the sources and job requirements, loaded once before the first question. A new job
    // description is summarised here, off every question's path; candidates queue meanwhile.
    let mut context = Context::open(context).unwrap_or_else(|e| {
        output.warn(&format!("no context: {e}; prompts go without it"));
        Context::empty()
    });
    if let Err(e) = context.ensure_requirements(&rt, &*provider, cancel) {
        output.warn(&format!(
            "job description not summarised: {e}; prompts go without its requirements"
        ));
    }
    tally.context = context.info();
    print_context(&context, output);

    let mut answerer = Answerer {
        rt: &rt,
        provider,
        hub,
        clock,
        audio,
        output,
        cancel,
        latest: ch.latest,
        reload,
    };
    // `[llm].slow_timeout_ms = 0` turns the slow phase off.
    let slow = (llm.slow_timeout_ms > 0).then(|| Duration::from_millis(llm.slow_timeout_ms));
    if let Err(e) = serve(&mut answerer, ch, slow, &mut context, &mut tally) {
        output.warn(&format!(
            "no suggestions from here on: {e}; transcribing only"
        ));
        model_down(e.to_string());
        skip_all(&mut tally, "LLM unavailable");
    }
    tally
}

/// Serve jobs until the sender drops (or the run is stopped). A job waiting always goes first;
/// the slow phase (when `slow` gives its deadline) runs only while none is. `Err` only for an
/// unreachable provider, after which nothing more is sent.
fn serve(
    answerer: &mut Answerer<'_>,
    ch: &Channels<'_>,
    slow: Option<Duration>,
    context: &mut Context,
    tally: &mut Tally,
) -> Result<(), LlmError> {
    let mut current: Option<CurrentQuestion> = None;
    let mut pending: VecDeque<SlowJob> = VecDeque::new();
    let mut memory = Memory::default();
    let mut closed = false;
    loop {
        let job = match ch.rx.try_recv() {
            Ok(job) => Some(job),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                closed = true;
                None
            }
        };
        let job = match job {
            Some(job) => job,
            None => {
                if let (Some(timeout), Some(mut next)) =
                    (slow, answerer.next_slow(&mut pending, &mut memory, tally))
                {
                    // #59: between requests, never mid-request.
                    answerer.reload();
                    if answerer.run_slow(&mut next, timeout, ch.gate, context, &mut memory, tally)
                        == SlowRun::Interrupted
                    {
                        pending.push_front(next);
                    }
                    continue;
                }
                if closed {
                    tally.slow_dropped += pending.len();
                    // Every shown question still without its line gets its fallback now.
                    let owed: Vec<Asked> = memory
                        .shown
                        .iter()
                        .filter(|a| !memory.summarised.contains(&a.id))
                        .cloned()
                        .collect();
                    for asked in &owed {
                        answerer.summarise(asked, None, &mut memory, tally);
                    }
                    return Ok(());
                }
                match ch.rx.recv() {
                    Ok(job) => job,
                    Err(_) => {
                        closed = true;
                        continue;
                    }
                }
            }
        };
        ch.gate.taken();
        // #59: a provider change in the config file takes effect here, before this request.
        answerer.reload();
        let carried = job.answered.clone();
        let Some(mut next) = answerer.answer(job, &mut current, context, tally)? else {
            // No slow phase from this job (ignored, a clarification, superseded, failed). While
            // its previous question is still on screen the next job carries it again. If the
            // screen moved on (a question that showed, then failed), nothing will: write that
            // question's fallback line now rather than at the end of the session.
            if let Some(prev) = carried
                && answerer.hub.snapshot().live.map(|q| q.candidate) != Some(prev.id)
            {
                answerer.summarise(&Asked::from(&prev), None, &mut memory, tally);
            }
            continue;
        };
        memory.shown(&next);
        if slow.is_some() {
            // A regeneration replaces its question's earlier slow job, and takes over the
            // summary that job still owed.
            if let Some(at) = pending.iter().position(|p| p.id == next.id)
                && let Some(old) = pending.remove(at)
                && next.answered.is_none()
            {
                next.answered = old.answered;
            }
            pending.push_back(next);
        } else if let Some(prev) = next.answered.take() {
            // No slow phase to write it: the previous question's fallback line, now.
            answerer.summarise(&Asked::from(&prev), None, &mut memory, tally);
        }
    }
}

/// How one slow-phase attempt went, for [`serve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlowRun {
    /// It ended (filled, timed out, failed, or the run was stopped): done with this question.
    Finished,
    /// A new job needed the model: try again later.
    Interrupted,
}

/// Runs one job at a time against a provider (the one `[llm]` names in `dmn`, a `MockProvider`
/// in tests). `provider` is swapped only between requests, by [`Answerer::reload`].
struct Answerer<'a> {
    rt: &'a tokio::runtime::Runtime,
    provider: Arc<dyn Provider>,
    hub: &'a SessionHub,
    clock: wire::SessionClock,
    /// Stamps [`Action::Timing`] for the session log (#16).
    audio: wire::AudioClock,
    output: Output,
    cancel: &'a CancellationToken,
    /// `Some` under [`Policy::Supersede`].
    latest: Option<&'a Latest>,
    /// #59: the config file to watch for a provider change; `None` in `dmn replay` without a
    /// config file and in most tests.
    reload: Option<Reload>,
}

/// Builds the provider `[llm]` names: [`wire::provider`] in `dmn`, mocks in tests.
type Builder = Box<dyn Fn(&config::Llm) -> Result<Arc<dyn Provider>, LlmError> + Send>;

/// The config file `dmn run` watches (#59). Read between requests: unchanged text costs one read
/// and a compare; changed text is parsed, and only a change to the provider, endpoint, model or
/// key variable builds a new provider.
struct Reload {
    path: PathBuf,
    /// The file's text when last read.
    seen: String,
    /// The `[llm]` the current provider was built from.
    llm: config::Llm,
    build: Builder,
}

impl Reload {
    fn watch(path: PathBuf, llm: &config::Llm, build: Builder) -> Self {
        let seen = std::fs::read_to_string(&path).unwrap_or_default();
        Self {
            path,
            seen,
            llm: llm.clone(),
            build,
        }
    }
}

/// Say which provider answers: the model dot's name, and, when transcript text leaves the
/// machine, the notice (SPEC § Providers) on the status line and in `dmn replay`'s output.
fn announce(llm: &config::Llm, hub: &SessionHub, output: Output) {
    hub.dispatch(Action::Provider {
        label: wire::provider_label(llm),
    });
    match llm.privacy_notice() {
        Some(notice) => output.note(&format!(
            "model: {} ({}); {notice}",
            llm.provider.as_str(),
            llm.model
        )),
        None => tracing::info!(provider = llm.provider.as_str(), model = %llm.model, "model"),
    }
}

/// The request for `job`: a new candidate (answered with the question on screen as context, so
/// a clarification can attach to it), or a regeneration of the question on screen.
fn request(job: &Job, current: Option<&CurrentQuestion>) -> FastRequest {
    // #63: the question on screen is this candidate's own first half, not context for it.
    let current = current.filter(|_| !job.continues);
    FastRequest {
        transcript: job.transcript.clone(),
        current: job.regenerate.clone().or_else(|| current.cloned()),
        regenerate: job.regenerate.is_some() && !job.swap,
        swap: job.swap,
        ..FastRequest::new(job.candidate.text.clone(), job.candidate.score)
    }
}

impl Answerer<'_> {
    /// #59: if the watched config file now names a different provider, build it, warm it up and
    /// answer the next request with it. Called between requests only, so a request in flight
    /// always finishes on the provider it started on. A bad file, a provider that cannot be
    /// built, or a failed warm-up keeps the current provider and says why.
    fn reload(&mut self) {
        let Some(r) = self.reload.as_mut() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&r.path) else {
            return;
        };
        if text == r.seen {
            return;
        }
        r.seen.clone_from(&text);
        let keep = r.llm.provider.as_str();
        let cfg = match Config::from_toml(&text, &r.path.display().to_string()) {
            Ok(cfg) => cfg,
            Err(e) => {
                self.output
                    .warn(&format!("config reload: {e}; keeping {keep}"));
                return;
            }
        };
        if wire::same_provider(&cfg.llm, &r.llm) {
            return;
        }
        let next = match (r.build)(&cfg.llm) {
            Ok(p) => p,
            Err(e) => {
                self.output
                    .warn(&format!("config reload: {e}; keeping {keep}"));
                return;
            }
        };
        let cancel = self.cancel;
        let warm = self.rt.block_on(async {
            tokio::select! {
                r = next.warm_up() => Some(r),
                () = cancel.cancelled() => None,
            }
        });
        match warm {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                self.output.warn(&format!(
                    "config reload: {} did not warm up: {e}; keeping {keep}",
                    cfg.llm.provider.as_str()
                ));
                return;
            }
            None => return,
        }
        tracing::info!(
            from = keep,
            to = cfg.llm.provider.as_str(),
            model = %cfg.llm.model,
            "provider switched by config reload"
        );
        self.provider = next;
        r.llm = cfg.llm;
        self.hub.dispatch(Action::Status {
            dot: Dot::Model,
            health: Health::Ok,
        });
        announce(&r.llm, self.hub, self.output);
    }

    /// Answer one job and dispatch everything it produced. Returns the slow phase to run for a
    /// question it put on screen (#55). `Err` only for an unreachable provider, after which
    /// nothing more is sent.
    fn answer(
        &self,
        job: Job,
        current: &mut Option<CurrentQuestion>,
        context: &mut Context,
        tally: &mut Tally,
    ) -> Result<Option<SlowJob>, LlmError> {
        let hub = self.hub;
        if self.cancel.is_cancelled() {
            hub.dispatch(Action::Failed {
                candidate: job.id,
                error: "stopped".into(),
            });
            tally.skipped += 1;
            return Ok(None);
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
            return Ok(None);
        }
        // #54: the requirements every prompt carries, and the chunks for this question (the
        // question on screen when regenerating). Timed from here, so its cost is visible in the
        // run summary against the 50 ms the issue allows it.
        let query = job
            .regenerate
            .as_ref()
            .map_or(job.candidate.text.as_str(), |c| c.question.as_str());
        // #58: a new candidate is matched to the stories; `r` and Swap reuse the story the
        // session names for this question (Swap has already moved it on to the next one).
        let chosen = regenerating
            .then(|| {
                let s = hub.snapshot();
                s.live
                    .iter()
                    .chain(s.queued.iter())
                    .find(|q| q.candidate == job.id)
                    .and_then(|q| q.story_label().map(str::to_owned))
            })
            .flatten();
        // A new candidate is matched on the whole utterance it came from (see `StoryPick`).
        let said = if job.candidate.line.trim().is_empty() {
            job.candidate.text.as_str()
        } else {
            job.candidate.line.as_str()
        };
        let pick = match (&chosen, regenerating) {
            (Some(label), _) => StoryPick::Chosen(label),
            (None, true) => StoryPick::Off,
            (None, false) => StoryPick::Match(said),
        };
        let retrieval = context.retrieve_for(query, pick);
        tally.retrieved(retrieval.took);
        let mut req = request(&job, current.as_ref());
        req.job_requirements = context.requirements().to_vec();
        req.chunks = retrieval.chunks.clone();
        req.story = retrieval.story.clone();
        // #57: the session's last 8 summary lines.
        req.summary = hub.snapshot().summary_for_prompt();
        let started = Instant::now();
        let mut question_at = None;
        let mut first_line_at = None;
        let mut approach_marked = false;
        let audio = &self.audio;
        // #16: when the question line and the first Approach line (or the STAR cards) were
        // produced, in audio time, for the session log. The store keeps the first showing, so a
        // regeneration's marks change nothing there.
        let mark = |stage: Stage| Action::Timing {
            candidate: job.id,
            stage,
            at: audio.now(),
        };
        let result = self
            .rt
            .block_on(run_fast_phase(&*self.provider, &req, token.clone(), |e| {
                let timing = match e {
                    FastEvent::Question { .. } | FastEvent::Clarification { .. } => {
                        let first = question_at.is_none();
                        question_at.get_or_insert_with(|| started.elapsed());
                        first.then(|| mark(Stage::Question))
                    }
                    FastEvent::ApproachLine { .. } | FastEvent::Star(_) => {
                        if matches!(e, FastEvent::ApproachLine { .. }) {
                            first_line_at.get_or_insert_with(|| started.elapsed());
                        }
                        let first = !approach_marked;
                        approach_marked = true;
                        first.then(|| mark(Stage::Approach))
                    }
                    FastEvent::Focus { .. } => None,
                };
                hub.dispatch(wire::fast_event(job.id, &e));
                if let Some(t) = timing {
                    hub.dispatch(t);
                }
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
            return Ok(None);
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
        let mut slow = None;
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
                    &retrieval,
                    self.output,
                );
                if clarification {
                    tally.clarifications += 1;
                } else {
                    tally.shown += 1;
                    if regenerating {
                        tally.regenerated += 1;
                    }
                    // #58: a new behavioral question built from a matched story records the
                    // ranking, so Swap can walk it. A regenerate or swap keeps what it has.
                    if suggestion.kind == Some(Kind::Behavioral)
                        && retrieval.story.is_some()
                        && !retrieval.stories.is_empty()
                    {
                        hub.dispatch(Action::Stories {
                            candidate: job.id,
                            ranked: retrieval.stories.clone(),
                        });
                    }
                    if let Some(kind) = suggestion.kind {
                        *current = Some(CurrentQuestion {
                            question: suggestion.question.clone(),
                            kind,
                        });
                        // #55: a question on screen gets its sections filled next. A
                        // clarification attaches to the question above it and gets none.
                        slow = Some(SlowJob {
                            id: job.id,
                            said: job.candidate.text.clone(),
                            start: job.candidate.start.as_secs_f64(),
                            transcript: job.transcript.clone(),
                            question: suggestion.question.clone(),
                            kind,
                            focus: suggestion.focus.clone(),
                            approach: suggestion.approach.clone(),
                            star: suggestion.star.clone(),
                            ready: Instant::now(),
                            restarts: 0,
                            // A regenerate re-asks the same question: nothing new was answered.
                            answered: job.answered.clone().filter(|a| a.id != job.id),
                        });
                    }
                }
            }
            Ok(FastOutcome::Ignored { reason, score }) => {
                print_ignored(
                    &job.candidate,
                    reason,
                    score,
                    timing,
                    &retrieval,
                    self.output,
                );
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
                            "chunks": retrieval.json(),
                            "retrieval_us": us(retrieval.took),
                        })
                    );
                }
                tally.failed += 1;
            }
        }
        Ok(slow)
    }

    /// The next slow phase to run, or `None` when there is none (or the run was stopped). Under
    /// [`Policy::Supersede`] only questions still on screen count, the live one first; the rest
    /// are dropped. `dmn replay` takes them in order.
    fn next_slow(
        &self,
        pending: &mut VecDeque<SlowJob>,
        memory: &mut Memory,
        tally: &mut Tally,
    ) -> Option<SlowJob> {
        if self.cancel.is_cancelled() {
            tally.slow_dropped += pending.len();
            pending.clear();
            return None;
        }
        if self.latest.is_none() {
            return pending.pop_front();
        }
        let session = self.hub.snapshot();
        let live = session.live.as_ref().map(|q| q.candidate);
        let on_screen =
            |id: u64| live == Some(id) || session.queued.iter().any(|q| q.candidate == id);
        let (keep, dropped): (VecDeque<SlowJob>, VecDeque<SlowJob>) = std::mem::take(pending)
            .into_iter()
            .partition(|p| on_screen(p.id));
        *pending = keep;
        tally.slow_dropped += dropped.len();
        // #57: a dropped slow phase would have summarised the question before it; write that
        // question's fallback line now rather than at the end of the session.
        for prev in dropped.iter().filter_map(|p| p.answered.as_ref()) {
            self.summarise(&Asked::from(prev), None, memory, tally);
        }
        let at = pending
            .iter()
            .position(|p| Some(p.id) == live)
            .or_else(|| pending.len().checked_sub(1))?;
        pending.remove(at)
    }

    /// One slow-phase attempt for `job`, dispatching each section as it arrives. Interrupted
    /// (and nothing ended in the session) when a job needs the model; then `job` runs again
    /// later from the start.
    fn run_slow(
        &self,
        job: &mut SlowJob,
        timeout: Duration,
        gate: &SlowGate,
        context: &mut Context,
        memory: &mut Memory,
        tally: &mut Tally,
    ) -> SlowRun {
        let hub = self.hub;
        // Publish the token, then look for a waiting job: see [`SlowGate`].
        let token = self.cancel.child_token();
        *gate.inflight() = Some(token.clone());
        if gate.job_waiting() {
            *gate.inflight() = None;
            return SlowRun::Interrupted;
        }
        // Retrieve again with the question and its focus: a better query than the candidate
        // text the fast phase had (#54).
        // #58: a STAR block built from a story keeps that story for the sections too, so Deeper
        // Answer never reaches for a different one.
        let story = job
            .star
            .as_ref()
            .and_then(|s| s.story.as_deref())
            .map_or(StoryPick::Off, StoryPick::Chosen);
        let retrieval = context.retrieve_for(&format!("{} {}", job.question, job.focus), story);
        tally.retrieved(retrieval.took);
        let mut req = SlowRequest::new(job.question.clone(), job.kind);
        req.context = FastRequest {
            transcript: job.transcript.clone(),
            job_requirements: context.requirements().to_vec(),
            chunks: retrieval.chunks.clone(),
            story: retrieval.story.clone(),
            ..FastRequest::new(job.said.clone(), 0)
        };
        req.focus = job.focus.clone();
        req.approach = job.approach.clone();
        req.star = job.star.clone();
        req.coding = wire::coding_mode(hub.snapshot().coding_solutions);
        req.chunk_files = retrieval.refs.iter().map(|r| r.file.clone()).collect();
        req.timeout = timeout;
        // #57: the session's summary lines, and the previous question to summarise if it still
        // owes its line.
        req.context.summary = hub.snapshot().summary_for_prompt();
        req.previous = job
            .answered
            .as_ref()
            .filter(|a| !memory.summarised.contains(&a.id))
            .map(|a| PreviousAnswer {
                question: a.question.clone(),
                answer: a.answer.clone(),
            });

        hub.dispatch(Action::SlowStarted { candidate: job.id });
        let waited = job.ready.elapsed();
        let started = Instant::now();
        let mut fields = SlowFields::default();
        let mut arrived: Vec<(&'static str, Duration)> = Vec::new();
        let result = self
            .rt
            .block_on(run_slow_phase(&*self.provider, &req, token.clone(), |f| {
                arrived.push((f.name(), started.elapsed()));
                fields.apply(&f);
                if let Some(action) = wire::slow_field(job.id, &f) {
                    hub.dispatch(action);
                }
            }));
        *gate.inflight() = None;
        let took = started.elapsed();
        let stopped = self.cancel.is_cancelled();
        if matches!(result, Err(LlmError::Cancelled)) && token.is_cancelled() && !stopped {
            // A job needs the model. What arrived stays on screen; the phase starts over.
            job.restarts += 1;
            tally.slow_restarts += 1;
            return SlowRun::Interrupted;
        }
        hub.dispatch(wire::slow_ended(
            job.id,
            &result,
            &fields,
            stopped,
            self.clock.now(),
        ));
        match &result {
            Ok(_) => {
                tally.slow_done += 1;
                tally.slow_slowest = tally.slow_slowest.max(took);
            }
            Err(LlmError::Cancelled) if stopped => tally.slow_dropped += 1,
            Err(LlmError::Timeout(_)) => {
                tally.slow_timed_out += 1;
                self.output.warn(&format!(
                    "slow phase for {:?} hit its {} deadline; {} section(s) arrived",
                    job.question,
                    secs(timeout),
                    arrived.len()
                ));
            }
            Err(e) => {
                tally.slow_failed += 1;
                self.output
                    .warn(&format!("slow phase failed for {:?}: {e}", job.question));
            }
        }
        if !stopped {
            print_slow(
                job,
                &result,
                &fields,
                SlowTiming {
                    waited,
                    took,
                    arrived: &arrived,
                },
                &retrieval,
                self.output,
            );
        }
        // #57: the previous question's line, from what this phase wrote (or its fallback).
        if let Some(prev) = &job.answered {
            self.summarise(&Asked::from(prev), Some(&fields), memory, tally);
        }
        SlowRun::Finished
    }

    /// Write `asked`'s one summary line (#57): the model's summary and promise from `fields`
    /// when it gave one, else the fallback from the question itself. Once per question.
    fn summarise(
        &self,
        asked: &Asked,
        fields: Option<&SlowFields>,
        memory: &mut Memory,
        tally: &mut Tally,
    ) {
        if !memory.summarised.insert(asked.id) {
            return;
        }
        let model = fields
            .and_then(|f| f.summary.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut flags = Vec::new();
        // A promise only counts with the summary it came with.
        let promise = model
            .and(fields.and_then(|f| f.promise.as_deref()))
            .map(|p| {
                dmn_llm::limits::limit_line(
                    "promise",
                    p,
                    dmn_llm::limits::PROMISE_CHARS,
                    &mut flags,
                )
            })
            .filter(|p| !p.is_empty());
        let text = model.map_or_else(
            || dmn_context::memory::fallback(&asked.question, &asked.focus),
            str::to_owned,
        );
        let line = dmn_llm::limits::summary_line(
            &dmn_context::memory::clock(asked.at),
            &text,
            promise.as_deref(),
            &mut flags,
        );
        tally.summaries += 1;
        if model.is_none() {
            tally.summary_fallbacks += 1;
        }
        print_summary(
            asked,
            &line,
            promise.as_deref(),
            model.is_some(),
            self.output,
        );
        self.hub.dispatch(Action::Summary {
            candidate: asked.id,
            line,
            promise,
            at: self.clock.now(),
        });
    }
}

/// A summary line as `dmn replay` prints it (#57). Its JSON type is `memory`: `summary` is the
/// run's closing record, which `eval/run.py` reads.
fn print_summary(asked: &Asked, line: &str, promise: Option<&str>, model: bool, output: Output) {
    let Output::Print { json } = output else {
        return;
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "memory",
                "candidate": asked.id,
                "question": asked.question,
                "line": line,
                "promise": promise,
                "from": if model { "model" } else { "fallback" },
            })
        );
    } else {
        let from = if model { "" } else { " (fallback)" };
        println!("  ~~ summary{from}: {line}");
    }
}

/// When a slow phase's sections arrived, for `dmn replay`.
struct SlowTiming<'a> {
    /// From its fast phase finishing to this (the last) attempt starting.
    waited: Duration,
    /// This attempt, request to close.
    took: Duration,
    /// Each section, from the request.
    arrived: &'a [(&'static str, Duration)],
}

fn print_slow(
    job: &SlowJob,
    result: &Result<SlowOutcome, LlmError>,
    fields: &SlowFields,
    t: SlowTiming<'_>,
    r: &Retrieval,
    output: Output,
) {
    let Output::Print { json } = output else {
        return;
    };
    let (outcome, error) = match result {
        Ok(_) => ("done", None),
        Err(LlmError::Timeout(_)) => ("slow", None),
        Err(e) => ("failed", Some(e.to_string())),
    };
    let flags: Vec<String> = match result {
        Ok(o) => o.flags.iter().map(|f| f.field.clone()).collect(),
        Err(_) => Vec::new(),
    };
    if json {
        let field_ms: serde_json::Map<String, serde_json::Value> = t
            .arrived
            .iter()
            .map(|(name, at)| ((*name).to_owned(), ms(*at).into()))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "type": "slow",
                "outcome": outcome,
                "start": job.start,
                "text": job.said,
                "question": job.question,
                "kind": job.kind,
                "fields": fields.to_json(),
                "flags": flags,
                "error": error,
                "field_ms": field_ms,
                "total_ms": ms(t.took),
                "wait_ms": ms(t.waited),
                "restarts": job.restarts,
                "chunks": r.json(),
            })
        );
        return;
    }
    let restarts = match job.restarts {
        0 => String::new(),
        n => format!(", {n} restart(s)"),
    };
    let cut = if flags.is_empty() {
        String::new()
    } else {
        format!(", cut: {}", flags.join(" "))
    };
    println!(
        "  .. slow phase {outcome} in {} ({} after the fast phase{restarts}){cut}{}\n     {}",
        secs(t.took),
        secs(t.waited),
        context_note(r),
        fields.to_json()
    );
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

fn us(d: Duration) -> u64 {
    d.as_micros() as u64
}

/// ` · context: resume.md#1, system-design.md#0` for the readable replay line; empty without.
fn context_note(r: &Retrieval) -> String {
    if r.refs.is_empty() {
        return String::new();
    }
    let names: Vec<String> = r
        .refs
        .iter()
        .map(|c| format!("{}#{}", c.file, c.idx))
        .collect();
    format!(" · context: {}", names.join(", "))
}

/// The context loaded for this run: one line for `dmn replay`, one NDJSON record with `--json`.
fn print_context(context: &Context, output: Output) {
    let info = context.info();
    match output {
        Output::Print { json: true } => println!(
            "{}",
            serde_json::json!({
                "type": "context",
                "sources": info.sources,
                "passages": info.passages,
                "stories": info.stories,
                "requirements": context.requirements(),
                "requirements_from": info.from.label(),
            })
        ),
        Output::Print { json: false } => {
            println!(
                "context: {} source(s), {} passage(s) searchable, {} stor{}; job requirements: {} ({})",
                info.sources,
                info.passages,
                info.stories,
                if info.stories == 1 { "y" } else { "ies" },
                info.requirements,
                info.from.label()
            );
            for r in context.requirements() {
                println!("  - {r}");
            }
        }
        Output::Quiet => tracing::info!(
            sources = info.sources,
            passages = info.passages,
            stories = info.stories,
            requirements = info.requirements,
            from = info.from.label(),
            "context loaded"
        ),
    }
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

#[allow(clippy::too_many_arguments)]
fn print_shown(
    c: &Candidate,
    s: &Suggestion,
    clarification: bool,
    flags: &[String],
    t: Timing,
    r: &Retrieval,
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
                "chunks": r.json(),
                "retrieval_us": us(r.took),
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
        "  => {what} at {}, first line at {}, done at {}{over}{cut}{}\n     {}",
        t.question.map(secs).unwrap_or_else(|| "-".into()),
        t.first_line.map(secs).unwrap_or_else(|| "-".into()),
        secs(t.total),
        context_note(r),
        fast_json(s)
    );
}

fn print_ignored(
    c: &Candidate,
    reason: IgnoreReason,
    score: i32,
    t: Timing,
    r: &Retrieval,
    output: Output,
) {
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
                "chunks": r.json(),
                "retrieval_us": us(r.took),
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
            wire::AudioClock::new(Instant::now(), 1.0),
            ContextSource::Off,
            None,
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

    /// Answer one request on loopback with a 401, like a remote API given a bad key.
    fn serve_one_401() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let body = "{\"type\":\"error\",\"error\":{\"message\":\"invalid x-api-key\"}}";
                let _ = write!(
                    stream,
                    "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    /// #59: `[llm].provider = "anthropic"` really answers through Anthropic (here a loopback
    /// server that rejects the key), the model dot is named after it with the "leaves this
    /// machine" flag, and the failed request turns the dot red. No network: `base_url` is local.
    #[test]
    fn a_remote_provider_named_in_the_config_is_the_one_asked() {
        let llm = config::Llm {
            provider: config::Provider::Anthropic,
            base_url: serve_one_401(),
            model: "claude-test".into(),
            ..config::Llm::default()
        };
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start(
            &llm,
            Output::Quiet,
            Policy::AnswerAll,
            Arc::clone(&hub),
            wire::SessionClock::start(1.0),
            wire::AudioClock::new(Instant::now(), 1.0),
            ContextSource::Off,
            None,
        );
        submit_with_candidate(&s, &hub, 1);
        assert_eq!(s.finish().failed, 1);
        let status = hub.snapshot().status;
        let label = status.provider.expect("announced");
        assert_eq!(
            (label.name.as_str(), label.model.as_str(), label.remote),
            ("anthropic", "claude-test", true)
        );
        assert!(
            matches!(&status.model, Health::Error(why) if why.contains("401")),
            "{:?}",
            status.model
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
            wire::AudioClock::new(Instant::now(), 1.0),
            ContextSource::Off,
            None,
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
            provider: Arc::new(provider.clone()),
            hub: &hub,
            clock: wire::SessionClock::start(1.0),
            audio: wire::AudioClock::new(Instant::now(), 1.0),
            output: Output::Quiet,
            cancel: &cancel,
            latest: None,
            reload: None,
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
            swap: false,
            answered: None,
            continues: false,
        };
        let mut tally = Tally::default();
        answerer
            .answer(job, &mut None, &mut Context::empty(), &mut tally)
            .unwrap();

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

    // --- Provider reload (#59) --------------------------------------------------------------

    fn wait_for_live(hub: &SessionHub, text: &str) {
        let until = Instant::now() + Duration::from_secs(10);
        while hub.snapshot().live.map(|q| q.text) != Some(text.to_owned()) {
            assert!(Instant::now() < until, "never showed {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Changing `[llm].provider` in the config file while a request streams: that request
    /// finishes on the provider it started on, the next question goes to the new one, and a
    /// change elsewhere in the file builds nothing.
    #[test]
    fn a_config_reload_swaps_the_provider_between_requests_never_mid_request() {
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[llm]\nprovider = \"ollama\"\n").unwrap();
        let llm = Config::load(&path).unwrap().llm;

        // A streams for about 2 s, so the file changes while its request is in flight. Both
        // replies restate the candidate, so #16's echo guard never retries them.
        let a = slow_reply("How would you design a rate limiter, from ollama?");
        let b = dmn_llm::MockProvider::json(&serde_json::json!({
            "is_question": true,
            "kind": "system_design",
            "question": "How would you design a rate limiter, from openai?",
            "focus": "tradeoffs",
            "approach": ["Clarify scope.", "Pick the data model.", "Name the failure modes."]
        }));
        let builds = Arc::new(AtomicUsize::new(0));
        let build: Builder = {
            let (a, b, builds) = (a.clone(), b.clone(), Arc::clone(&builds));
            Box::new(move |llm: &config::Llm| {
                builds.fetch_add(1, Ordering::SeqCst);
                Ok(match llm.provider {
                    config::Provider::Ollama => Arc::new(a.clone()) as Arc<dyn Provider>,
                    _ => Arc::new(b.clone()) as Arc<dyn Provider>,
                })
            })
        };
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_reload(
            &llm,
            path.clone(),
            build,
            Policy::AnswerAll,
            Arc::clone(&hub),
        );

        submit_with_candidate(&s, &hub, 1);
        std::thread::sleep(Duration::from_millis(300));
        let label = hub
            .snapshot()
            .status
            .provider
            .expect("announced before the first request");
        assert_eq!((label.name.as_str(), label.remote), ("ollama", false));
        std::fs::write(
            &path,
            "[llm]\nprovider = \"openai\"\nmodel = \"gpt-test\"\n",
        )
        .unwrap();
        wait_for_live(&hub, "How would you design a rate limiter, from ollama?");
        assert_eq!(
            b.requests().len(),
            0,
            "the request in flight stayed on ollama"
        );

        submit_with_candidate(&s, &hub, 2);
        wait_for_live(&hub, "How would you design a rate limiter, from openai?");
        // Not a provider field: nothing is rebuilt.
        std::fs::write(
            &path,
            "[llm]\nprovider = \"openai\"\nmodel = \"gpt-test\"\nslow_timeout_ms = 0\n",
        )
        .unwrap();
        submit_with_candidate(&s, &hub, 3);
        let tally = s.finish();

        assert_eq!(tally.shown, 3);
        assert_eq!(a.requests().len(), 1);
        assert_eq!(b.requests().len(), 2);
        assert_eq!(
            builds.load(Ordering::SeqCst),
            2,
            "the first provider and the switch"
        );
        let label = hub.snapshot().status.provider.expect("label");
        assert_eq!(
            (label.name.as_str(), label.model.as_str()),
            ("openai", "gpt-test")
        );
        assert!(label.remote, "openai sends transcript text off the machine");
    }

    /// A config file that no longer parses keeps the provider that works.
    #[test]
    fn a_broken_config_reload_keeps_the_current_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[llm]\nprovider = \"ollama\"\n").unwrap();
        let llm = Config::load(&path).unwrap().llm;
        let a = slow_reply("Still ollama?");
        let build: Builder = {
            let a = a.clone();
            Box::new(move |_: &config::Llm| Ok(Arc::new(a.clone()) as Arc<dyn Provider>))
        };
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_reload(
            &llm,
            path.clone(),
            build,
            Policy::AnswerAll,
            Arc::clone(&hub),
        );
        std::fs::write(&path, "[llm]\nprovider = \"nonsense\"\n").unwrap();
        submit_with_candidate(&s, &hub, 1);
        assert_eq!(s.finish().shown, 1);
        assert_eq!(a.requests().len(), 1);
        assert_eq!(
            hub.snapshot().status.provider.map(|p| p.name).as_deref(),
            Some("ollama")
        );
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

    // --- Slow phase (#55) --------------------------------------------------------------------

    fn fast_json_reply(question: &str, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "is_question": true,
            "kind": kind,
            "question": question,
            "focus": "tradeoffs",
            "approach": ["Clarify scope.", "Pick the data model.", "Name the failure modes."]
        })
    }

    fn slow_json_reply() -> serde_json::Value {
        serde_json::json!({
            "deeper": [{"label": "Core", "text": "Shard by key."}],
            "followups": ["What if a shard is hot?"],
            "tradeoffs": [{"option": "Sharding", "pro": "scales", "con": "rebalancing"}],
            "example": {"language": "text", "code": "10k QPS per shard"},
            "mention": ["Name the SLO."],
            "resume_context": []
        })
    }

    /// The slow reply's first field, then a stall of `stall`: a slow phase still running.
    fn stalled_slow_script(stall: Duration) -> Vec<dmn_llm::mock::Step> {
        let reply = slow_json_reply().to_string();
        let deeper_closes = reply.find("\"followups\"").unwrap();
        // Chunks are 7 characters: stall on the first chunk wholly after `deeper`'s value.
        let mut script = dmn_llm::mock::json_script(&slow_json_reply());
        script.insert(deeper_closes.div_ceil(7), dmn_llm::mock::Step::Pause(stall));
        script
    }

    /// Wait until `done` holds for the session, or `limit` passes.
    fn wait_for(
        hub: &SessionHub,
        limit: Duration,
        done: impl Fn(&dmn_session::Session) -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done(&hub.snapshot()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done(&hub.snapshot())
    }

    #[test]
    fn a_running_slow_phase_never_delays_the_next_fast_phase() {
        // Request 1: fast for candidate 1. Request 2: its slow phase, which stalls for 10 s.
        // Request 3: fast for candidate 2. Then every slow phase answers at once.
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design"))
            .then(stalled_slow_script(Duration::from_secs(10)))
            .then_json(&fast_json_reply("Q2", "system_design"))
            .then_json(&slow_json_reply());
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            Arc::clone(&hub),
            Context::empty(),
            Some(Duration::from_secs(20)),
        );
        submit_with_candidate(&s, &hub, 1);
        assert!(
            wait_for(&hub, Duration::from_secs(2), |_| provider.requests().len()
                == 2),
            "the slow phase for 1 is in flight"
        );
        let asked = Instant::now();
        submit_with_candidate(&s, &hub, 2);
        assert!(
            wait_for(&hub, Duration::from_secs(2), |s| s
                .live
                .as_ref()
                .is_some_and(|q| q.candidate == 2)),
            "candidate 2's question never showed"
        );
        let waited = asked.elapsed();
        assert!(
            waited < Duration::from_millis(500),
            "the fast phase waited {waited:?} behind a slow phase stalled for 10 s"
        );
        let tally = s.finish();
        assert_eq!(tally.slow_restarts, 1, "{tally:?}");
        assert_eq!(
            tally.slow_done, 2,
            "replay fills both, 1 again from the start: {tally:?}"
        );
        let live = hub.snapshot().live.unwrap();
        assert_eq!(live.sections.followups, ["What if a shard is hot?"]);
        assert_eq!(
            *live.sections.state(dmn_session::Section::Tradeoffs),
            dmn_session::SectionState::Done
        );
        assert_eq!(
            *live.sections.state(dmn_session::Section::ResumeContext),
            dmn_session::SectionState::Empty,
            "nothing to cite, so the row is hidden"
        );
    }

    #[test]
    fn a_superseded_questions_slow_phase_is_cancelled_and_dropped() {
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design"))
            .then(stalled_slow_script(Duration::from_secs(10)))
            .then_json(&fast_json_reply("Q2", "system_design"))
            .then_json(&slow_json_reply());
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::Supersede,
            Arc::clone(&hub),
            Context::empty(),
            Some(Duration::from_secs(20)),
        );
        submit_with_candidate(&s, &hub, 1);
        assert!(wait_for(&hub, Duration::from_secs(2), |_| provider
            .requests()
            .len()
            == 2));
        submit_with_candidate(&s, &hub, 2);
        let t = Instant::now();
        let tally = s.finish();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(
            (tally.slow_restarts, tally.slow_dropped, tally.slow_done),
            (1, 1, 1),
            "1's slow phase cut short and then dropped; 2's filled: {tally:?}"
        );
        assert_eq!(
            provider.requests().len(),
            4,
            "1's slow phase is never asked again"
        );
        assert_eq!(hub.snapshot().live.unwrap().candidate, 2);
    }

    #[test]
    fn a_timed_out_slow_phase_keeps_what_arrived_and_marks_the_rest_slow() {
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design"))
            .then(stalled_slow_script(Duration::from_secs(10)));
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider,
            Policy::AnswerAll,
            Arc::clone(&hub),
            Context::empty(),
            Some(Duration::from_millis(400)),
        );
        submit_with_candidate(&s, &hub, 1);
        let tally = s.finish();
        assert_eq!(tally.slow_timed_out, 1, "{tally:?}");
        let sections = hub.snapshot().live.unwrap().sections;
        use dmn_session::{Section, SectionState};
        assert_eq!(
            sections.deeper.len(),
            1,
            "the field before the stall arrived"
        );
        assert_eq!(*sections.state(Section::Deeper), SectionState::Done);
        for s in [Section::Followups, Section::Example, Section::ResumeContext] {
            assert_eq!(*sections.state(s), SectionState::Slow, "{s:?}");
        }
    }

    #[test]
    fn the_coding_mode_on_screen_decides_whether_a_solution_is_asked_for() {
        let solution = serde_json::json!({
            "deeper": [{"label": "Insight", "text": "Use a hash map."}],
            "followups": [], "tradeoffs": [], "mention": [], "resume_context": [],
            "example": {"language": "python", "code": "seen = {}"},
            "solution": {"language": "python", "code": "def two_sum(a, t):\n    return []"}
        });
        for (mode, asked) in [
            (dmn_session::CodingSolutions::Hints, false),
            (dmn_session::CodingSolutions::Full, true),
        ] {
            let provider = dmn_llm::MockProvider::json(&fast_json_reply("Two sum?", "coding"))
                .then_json(&solution);
            let hub = Arc::new(SessionHub::default());
            hub.dispatch(Action::CodingSolutions { mode });
            let s = Suggester::start_with_context(
                provider.clone(),
                Policy::AnswerAll,
                Arc::clone(&hub),
                Context::empty(),
                Some(Duration::from_secs(20)),
            );
            submit_with_candidate(&s, &hub, 1);
            s.finish();
            let slow = &provider.requests()[1];
            assert_eq!(
                slow.schema
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("solution"),
                asked,
                "{mode:?}"
            );
            let sections = hub.snapshot().live.unwrap().sections;
            assert_eq!(sections.solution.is_some(), asked, "{mode:?}");
        }
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

    /// #54: with the context fixtures loaded, every fast-phase prompt carries the job
    /// requirements, and a question about the resume carries the chunks retrieved for it.
    #[test]
    fn every_prompt_carries_the_job_requirements_and_the_retrieved_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::context::fixture_db(dir.path());
        let bullets = [
            "Design distributed services in Go or Rust",
            "Postgres sharding and Redis caching",
        ];
        {
            // The summary is already cached, as it would be after the first session.
            let store = dmn_store::Store::open(&db).unwrap();
            let lib = dmn_context::Library::load(&store).unwrap();
            let hash = &lib.job_description().unwrap().hash;
            let owned: Vec<String> = bullets.iter().map(|b| (*b).to_string()).collect();
            store.put_jd_summary(hash, &owned, 1).unwrap();
        }
        let context = Context::open(&ContextSource::Db {
            path: db,
            write: false,
        })
        .unwrap();
        let provider = slow_reply("Q").with_delay(Duration::ZERO);
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            Arc::clone(&hub),
            context,
            None,
        );
        let kafka = Candidate {
            text: "Tell me about a time you built something on Kafka.".into(),
            ..candidate()
        };
        let weather = Candidate {
            text: "How's the weather over there?".into(),
            ..candidate()
        };
        for (id, c) in [(1, candidate()), (2, kafka), (3, weather)] {
            hub.dispatch(wire::candidate(id, &c));
            s.submit(id, &c, String::new());
        }
        let tally = s.finish();
        assert_eq!(tally.retrievals, 3);
        assert_eq!(tally.context.requirements, 2);
        let sent = provider.requests();
        assert_eq!(sent.len(), 3);
        for req in &sent {
            let user = &req.messages.last().unwrap().content;
            assert!(user.starts_with("## Job requirements\n"), "{user}");
            for b in bullets {
                assert!(user.contains(&format!("- {b}\n")), "{b} missing: {user}");
            }
        }
        let rate = &sent[0].messages.last().unwrap().content;
        assert!(
            rate.contains("## Candidate's material\n[1] resume (resume.md): "),
            "{rate}"
        );
        assert!(rate.contains("rate limiter"), "{rate}");
        let kafka = &sent[1].messages.last().unwrap().content;
        assert!(
            kafka.contains("Kafka, with idempotent consumers"),
            "{kafka}"
        );
        let weather = &sent[2].messages.last().unwrap().content;
        assert!(
            !weather.contains("## Candidate's material"),
            "nothing matches small talk: {weather}"
        );
        assert!(
            !kafka.contains("## Matched story"),
            "no stories loaded, no story: the M1 behavioral prompt"
        );
    }

    /// #58: a behavioral question with the stories fixtures loaded carries its matched story
    /// (whose chunks are then not repeated), the STAR cards name it, the ranking reaches the
    /// session, and Swap asks again with the next story at temperature 0.
    #[test]
    fn a_behavioral_question_gets_its_matched_story_and_swap_asks_with_the_next() {
        use dmn_context::{SourceKind, ingest};
        let dir = tempfile::tempdir().unwrap();
        let db = crate::context::fixture_db(dir.path());
        {
            let store = dmn_store::Store::open(&db).unwrap();
            let stories = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../testdata/context/stories");
            ingest(&store, &stories, SourceKind::Stories, 1_700_000_050).unwrap();
        }
        let context = Context::open(&ContextSource::Db {
            path: db,
            write: false,
        })
        .unwrap();
        assert_eq!(context.info().stories, 8);
        let provider = dmn_llm::MockProvider::json(&serde_json::json!({
            "is_question": true,
            "kind": "behavioral",
            "question": "Tell me about a time you disagreed with a technical decision.",
            "focus": "conflict, evidence",
            "approach": ["Situation.", "Task.", "Action.", "Result."],
            "star": {"situation": "s", "task": "t", "action": "a", "result": "r"}
        }));
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            Arc::clone(&hub),
            context,
            None,
        );
        let text = "Tell me about a time you disagreed with a technical decision and how you \
                    handled it.";
        let asked = Candidate {
            text: text.into(),
            line: format!("Okay, first one. {text}"),
            ..candidate()
        };
        hub.dispatch(wire::candidate(1, &asked));
        s.submit(1, &asked, String::new());
        let deadline = Instant::now() + Duration::from_secs(10);
        let live = loop {
            if let Some(q) = hub.snapshot().live.filter(|q| !q.stories.is_empty()) {
                break q;
            }
            assert!(Instant::now() < deadline, "no stories reached the session");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(live.stories[0], "stories/disagreed-on-broker.md");
        assert!(live.stories.len() > 1, "{:?}", live.stories);
        assert_eq!(
            live.star.as_ref().and_then(|c| c.story.as_deref()),
            Some("stories/disagreed-on-broker.md"),
            "the header names the story the cards came from"
        );

        // `w`: the session moves on, then the pipeline asks again for this question.
        hub.dispatch(Action::SwapStory { candidate: 1 });
        s.swap(
            1,
            &asked,
            String::new(),
            CurrentQuestion {
                question: live.text.clone(),
                kind: dmn_llm::Kind::Behavioral,
            },
        );
        let tally = s.finish();
        assert_eq!((tally.shown, tally.regenerated), (2, 1), "{tally:?}");
        let next = live.stories[1].clone();
        let after = hub.snapshot().live.unwrap();
        assert_eq!(after.story_label(), Some(next.as_str()));
        assert_eq!(
            after.star.as_ref().and_then(|c| c.story.as_deref()),
            Some(next.as_str())
        );

        let sent = provider.requests();
        assert_eq!(sent.len(), 2);
        let first = &sent[0].messages.last().unwrap().content;
        assert!(
            first.contains("## Matched story (stories/disagreed-on-broker.md)\n"),
            "{first}"
        );
        assert!(
            !first.contains("stories (disagreed-on-broker.md)"),
            "its chunks are not repeated as material: {first}"
        );
        let swapped = &sent[1].messages.last().unwrap().content;
        assert!(
            swapped.contains(&format!("## Matched story ({next})\n")),
            "{swapped}"
        );
        assert!(swapped.contains("another of their stories"), "{swapped}");
        assert_eq!(sent[1].temperature, 0.0);
    }

    /// #54: with no sources loaded the prompt is exactly the M1 prompt.
    #[test]
    fn without_sources_the_prompt_is_the_m1_prompt() {
        let provider = slow_reply("Q").with_delay(Duration::ZERO);
        let hub = Arc::new(SessionHub::default());
        let dir = tempfile::tempdir().unwrap();
        let context = Context::open(&ContextSource::Db {
            path: dir.path().join("dmn.sqlite"),
            write: false,
        })
        .unwrap();
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            hub.clone(),
            context,
            None,
        );
        hub.dispatch(wire::candidate(1, &candidate()));
        s.submit(1, &candidate(), "Interviewer: So.".into());
        assert_eq!(s.finish().shown, 1);
        let m1 = dmn_llm::fast::fast_chat_request(&FastRequest {
            transcript: "Interviewer: So.".into(),
            ..FastRequest::new(candidate().text, candidate().score)
        });
        assert_eq!(provider.requests(), [m1]);
    }

    #[test]
    fn a_new_candidate_is_asked_at_temperature_0() {
        let job = Job {
            id: 2,
            candidate: candidate(),
            transcript: String::new(),
            regenerate: None,
            swap: false,
            answered: None,
            continues: false,
        };
        let req = request(&job, None);
        assert!(!req.regenerate);
        let chat = dmn_llm::fast::fast_chat_request(&req);
        assert_eq!(chat.temperature, 0.0);
    }

    #[test]
    fn a_continuation_is_asked_without_its_first_half_as_the_current_question() {
        let on_screen = CurrentQuestion {
            question: "How would you design a rate limiter?".into(),
            kind: Kind::SystemDesign,
        };
        let job = |continues| Job {
            id: 2,
            candidate: candidate(),
            transcript: String::new(),
            regenerate: None,
            swap: false,
            answered: None,
            continues,
        };
        assert_eq!(
            request(&job(false), Some(&on_screen)).current,
            Some(on_screen.clone()),
            "a new question sees the one on screen, so a clarification can attach"
        );
        assert_eq!(
            request(&job(true), Some(&on_screen)).current,
            None,
            "the rest of the same question is not a clarification of its first half"
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

    // --- Session memory (#57) ----------------------------------------------------------------

    fn you_said(start: f64, text: &str) -> Action {
        Action::Segment {
            speaker: dmn_session::Speaker::You,
            start,
            end: start + 5.0,
            text: text.into(),
            is_final: true,
            avg_logprob: -0.2,
            language: None,
        }
    }

    fn answered_1(answer: &str) -> Answered {
        Answered {
            id: 1,
            question: "Q1".into(),
            focus: "tradeoffs".into(),
            at: 1.0,
            answer: answer.into(),
        }
    }

    #[test]
    fn a_promise_made_in_one_answer_shows_on_the_next_question_until_it_is_covered() {
        let mut summarising = slow_json_reply();
        summarising["summary"] = serde_json::json!("rate limiter — covered token bucket");
        summarising["promise"] = serde_json::json!("clock skew");
        // Request 1: fast Q1. 2: its slow phase (nothing answered yet). 3: fast Q2. 4: Q2's
        // slow phase, which summarises the answer to Q1.
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design"))
            .then_json(&slow_json_reply())
            .then_json(&fast_json_reply("Q2", "system_design"))
            .then_json(&summarising);
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            Arc::clone(&hub),
            Context::empty(),
            Some(Duration::from_secs(20)),
        );
        hub.dispatch(wire::candidate(1, &candidate()));
        s.submit_after(1, &candidate(), String::new(), None, false);
        assert!(wait_for(&hub, Duration::from_secs(5), |_| provider
            .requests()
            .len()
            == 2));
        assert!(
            !provider.requests()[1].messages[1]
                .content
                .contains("Previous question"),
            "nothing answered before Q1"
        );
        let answer = "Token bucket per key. I'll come back to clock skew.";
        hub.dispatch(you_said(5.0, answer));

        hub.dispatch(wire::candidate(2, &candidate()));
        s.submit_after(
            2,
            &candidate(),
            String::new(),
            Some(answered_1(answer)),
            false,
        );
        let item = "Come back to clock skew: you said you would";
        assert!(
            wait_for(&hub, Duration::from_secs(5), |s| s
                .live
                .as_ref()
                .is_some_and(
                    |q| q.candidate == 2 && q.sections.mention.contains(&item.into())
                )),
            "the promise shows on Q2: {:?}",
            hub.snapshot().live.map(|q| q.sections.mention)
        );
        let slow2 = &provider.requests()[3].messages[1].content;
        assert!(
            slow2.contains("## Previous question and the candidate's answer"),
            "{slow2}"
        );
        assert!(
            slow2.contains("Candidate said: Token bucket per key."),
            "{slow2}"
        );
        let session = hub.snapshot();
        assert_eq!(
            session.summary[0].line,
            "00:01 rate limiter — covered token bucket; promised to return to clock skew"
        );

        // Covering it closes it.
        hub.dispatch(you_said(40.0, "Back to clock skew: NTP keeps it small."));
        let session = hub.snapshot();
        assert!(session.promises[0].mentioned);
        assert!(
            !session
                .live
                .unwrap()
                .sections
                .mention
                .contains(&item.into())
        );

        let tally = s.finish();
        assert_eq!(
            (tally.summaries, tally.summary_fallbacks),
            (2, 1),
            "Q1 from the model; Q2, never followed, from the fallback: {tally:?}"
        );
        let session = hub.snapshot();
        assert_eq!(session.summary.len(), 2, "one line per answered question");
        assert_eq!(session.summary[1].line, "00:01 Q2 — asked about tradeoffs");
    }

    #[test]
    fn every_prompt_carries_the_last_eight_summary_lines() {
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q11", "system_design"))
            .then_json(&slow_json_reply());
        let hub = Arc::new(SessionHub::default());
        for id in 1..=10u64 {
            hub.dispatch(Action::Summary {
                candidate: id,
                line: format!("{id:02}:00 topic {id} — covered it"),
                promise: None,
                at: 1,
            });
        }
        let s = Suggester::start_with_context(
            provider.clone(),
            Policy::AnswerAll,
            Arc::clone(&hub),
            Context::empty(),
            Some(Duration::from_secs(20)),
        );
        hub.dispatch(wire::candidate(11, &candidate()));
        s.submit(11, &candidate(), String::new());
        s.finish();
        let sent = provider.requests();
        assert_eq!(sent.len(), 2, "fast and slow");
        for req in &sent {
            let user = &req.messages[1].content;
            assert!(user.contains("## Session so far"), "{user}");
            assert!(!user.contains("topic 2 —"), "only the last 8: {user}");
            for id in 3..=10 {
                assert!(user.contains(&format!("topic {id} —")), "{user}");
            }
        }
    }

    #[test]
    fn with_the_slow_phase_off_every_answered_question_still_gets_its_line() {
        let provider = dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design"))
            .then_json(&fast_json_reply("Q2", "system_design"));
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with(provider, Policy::AnswerAll, Arc::clone(&hub));
        hub.dispatch(wire::candidate(1, &candidate()));
        s.submit(1, &candidate(), String::new());
        assert!(wait_for(&hub, Duration::from_secs(5), |s| s
            .live
            .as_ref()
            .is_some_and(|q| q.candidate == 1)));
        hub.dispatch(wire::candidate(2, &candidate()));
        s.submit_after(
            2,
            &candidate(),
            String::new(),
            Some(answered_1("Hash map.")),
            false,
        );
        let tally = s.finish();
        assert_eq!(
            (tally.summaries, tally.summary_fallbacks),
            (2, 2),
            "{tally:?}"
        );
        let lines: Vec<String> = hub.snapshot().summary.into_iter().map(|l| l.line).collect();
        assert_eq!(
            lines,
            [
                "00:01 Q1 — asked about tradeoffs",
                "00:01 Q2 — asked about tradeoffs"
            ]
        );
    }

    #[test]
    fn a_question_that_shows_then_fails_writes_the_previous_line_at_once() {
        // Q1 is on screen. Q2's question shows, then its reply breaks off: no slow phase will
        // summarise Q1, so its fallback line is written now, not at the end of the session.
        let mut broken = dmn_llm::mock::json_script(&fast_json_reply("Q2", "system_design"));
        let cut = broken.len() / 2;
        broken.truncate(cut);
        broken.push(dmn_llm::mock::Step::Error("model crashed".into()));
        let provider =
            dmn_llm::MockProvider::json(&fast_json_reply("Q1", "system_design")).then(broken);
        let hub = Arc::new(SessionHub::default());
        let s = Suggester::start_with(provider, Policy::AnswerAll, Arc::clone(&hub));
        hub.dispatch(wire::candidate(1, &candidate()));
        s.submit(1, &candidate(), String::new());
        assert!(wait_for(&hub, Duration::from_secs(5), |s| s
            .live
            .as_ref()
            .is_some_and(|q| q.candidate == 1)));
        hub.dispatch(wire::candidate(2, &candidate()));
        s.submit_after(
            2,
            &candidate(),
            String::new(),
            Some(answered_1("Hash map.")),
            false,
        );
        assert!(
            wait_for(&hub, Duration::from_secs(5), |s| s
                .summary
                .iter()
                .any(|l| l.candidate == 1)),
            "Q1's line is written once Q2 fails, before the session ends"
        );
        assert_eq!(hub.snapshot().live.unwrap().candidate, 2, "Q2 showed");
        let tally = s.finish();
        assert_eq!(tally.failed, 1, "{tally:?}");
    }
}
