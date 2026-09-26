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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_core::{config, wire};
use dmn_detect::Candidate;
use dmn_llm::fast::FAST_PHASE_TIMEOUT;
use dmn_llm::schema::Star;
use dmn_llm::{
    CancellationToken, CurrentQuestion, FastEvent, FastOutcome, FastRequest, IgnoreReason, Kind,
    LlmError, OllamaProvider, OllamaSettings, Provider, SlowFields, SlowOutcome, SlowRequest,
    Suggestion, run_fast_phase, run_slow_phase,
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
    /// `context` is where the sources and job requirements are read from (#54).
    pub fn start(
        llm: &config::Llm,
        output: Output,
        policy: Policy,
        hub: Arc<SessionHub>,
        clock: wire::SessionClock,
        audio: wire::AudioClock,
        context: ContextSource,
    ) -> Self {
        let llm = llm.clone();
        Self::spawn(policy, hub, move |ch| {
            worker(&llm, output, clock, audio, &ch, &context)
        })
    }

    /// Serve jobs on `provider` directly, with no warm-up, no context and no slow phase: the
    /// tests' way in.
    #[cfg(test)]
    fn start_with<P: Provider + Send + 'static>(
        provider: P,
        policy: Policy,
        hub: Arc<SessionHub>,
    ) -> Self {
        Self::start_with_context(provider, policy, hub, Context::empty(), None)
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
            let answerer = Answerer {
                rt: &rt,
                provider: &provider,
                hub: ch.hub,
                clock: wire::SessionClock::start(1.0),
                audio: wire::AudioClock::new(Instant::now(), 1.0),
                output: Output::Quiet,
                cancel: ch.cancel,
                latest: ch.latest,
            };
            let mut tally = Tally {
                context: context.info(),
                ..Tally::default()
            };
            let _ = serve(&answerer, &ch, slow, &mut context, &mut tally);
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
            swap: false,
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

    // #54: the sources and job requirements, loaded once before the first question. A new job
    // description is summarised here, off every question's path; candidates queue meanwhile.
    let mut context = Context::open(context).unwrap_or_else(|e| {
        output.warn(&format!("no context: {e}; prompts go without it"));
        Context::empty()
    });
    if let Err(e) = context.ensure_requirements(&rt, &provider, cancel) {
        output.warn(&format!(
            "job description not summarised: {e}; prompts go without its requirements"
        ));
    }
    tally.context = context.info();
    print_context(&context, output);

    let answerer = Answerer {
        rt: &rt,
        provider: &provider,
        hub,
        clock,
        audio,
        output,
        cancel,
        latest: ch.latest,
    };
    // `[llm].slow_timeout_ms = 0` turns the slow phase off.
    let slow = (llm.slow_timeout_ms > 0).then(|| Duration::from_millis(llm.slow_timeout_ms));
    if let Err(e) = serve(&answerer, ch, slow, &mut context, &mut tally) {
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
fn serve<P: Provider>(
    answerer: &Answerer<'_, P>,
    ch: &Channels<'_>,
    slow: Option<Duration>,
    context: &mut Context,
    tally: &mut Tally,
) -> Result<(), LlmError> {
    let mut current: Option<CurrentQuestion> = None;
    let mut pending: VecDeque<SlowJob> = VecDeque::new();
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
                    (slow, answerer.next_slow(&mut pending, tally))
                {
                    if answerer.run_slow(&mut next, timeout, ch.gate, context, tally)
                        == SlowRun::Interrupted
                    {
                        pending.push_front(next);
                    }
                    continue;
                }
                if closed {
                    tally.slow_dropped += pending.len();
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
        if let Some(next) = answerer.answer(job, &mut current, context, tally)?
            && slow.is_some()
        {
            // A regeneration replaces its question's earlier slow job.
            pending.retain(|p| p.id != next.id);
            pending.push_back(next);
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

/// Runs one job at a time against a provider (Ollama in `dmn`, a `MockProvider` in tests).
struct Answerer<'a, P: Provider> {
    rt: &'a tokio::runtime::Runtime,
    provider: &'a P,
    hub: &'a SessionHub,
    clock: wire::SessionClock,
    /// Stamps [`Action::Timing`] for the session log (#16).
    audio: wire::AudioClock,
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
        regenerate: job.regenerate.is_some() && !job.swap,
        swap: job.swap,
        ..FastRequest::new(job.candidate.text.clone(), job.candidate.score)
    }
}

impl<P: Provider> Answerer<'_, P> {
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
        let pick = match (&chosen, regenerating) {
            (Some(label), _) => StoryPick::Chosen(label),
            (None, true) => StoryPick::Off,
            (None, false) => StoryPick::Match,
        };
        let retrieval = context.retrieve_for(query, pick);
        tally.retrieved(retrieval.took);
        let mut req = request(&job, current.as_ref());
        req.job_requirements = context.requirements().to_vec();
        req.chunks = retrieval.chunks.clone();
        req.story = retrieval.story.clone();
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
            .block_on(run_fast_phase(self.provider, &req, token.clone(), |e| {
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
    fn next_slow(&self, pending: &mut VecDeque<SlowJob>, tally: &mut Tally) -> Option<SlowJob> {
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
        let before = pending.len();
        pending.retain(|p| on_screen(p.id));
        tally.slow_dropped += before - pending.len();
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

        hub.dispatch(Action::SlowStarted { candidate: job.id });
        let waited = job.ready.elapsed();
        let started = Instant::now();
        let mut fields = SlowFields::default();
        let mut arrived: Vec<(&'static str, Duration)> = Vec::new();
        let result = self
            .rt
            .block_on(run_slow_phase(self.provider, &req, token.clone(), |f| {
                arrived.push((f.name(), started.elapsed()));
                fields.apply(&f);
                hub.dispatch(wire::slow_field(job.id, &f));
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
        SlowRun::Finished
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
            wire::AudioClock::new(Instant::now(), 1.0),
            ContextSource::Off,
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
            wire::AudioClock::new(Instant::now(), 1.0),
            ContextSource::Off,
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
            audio: wire::AudioClock::new(Instant::now(), 1.0),
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
            swap: false,
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
        let asked = Candidate {
            text: "Tell me about a time you disagreed with a technical decision and how you \
                   handled it."
                .into(),
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
