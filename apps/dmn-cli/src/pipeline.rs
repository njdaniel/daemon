//! The pipeline `dmn replay` and `dmn run --replay` share: two WAVs → VAD → whisper → transcript
//! bus → detector → fast phase, all dispatched into one session ([`Recorder`]) that is written
//! to SQLite the way a live call's is.
//!
//! [`Pipeline::start`] builds and starts every stage; [`Pipeline::run`] is the loop that moves
//! segments from the bus into the session and the detector, and candidates into the fast phase;
//! [`Pipeline::finish`] drains and joins everything and returns a [`Report`]. What each command
//! does with the results is an [`Observer`]: `dmn replay` prints every segment and candidate,
//! `dmn run` prints nothing and draws the session instead.
//!
//! [`Controls`] is how the TUI's keys reach the pipeline from another thread: pin and unpin
//! dispatch straight into the session, mute gates the mic lane's VAD, regenerate goes to the
//! pipeline thread (which owns the candidates and the transcript window the request needs), and
//! quit stops the audio sources so the run ends early and cleanly.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dmn_audio::{AudioSource, FRAME_LEN, FRAME_MS, FrameSink, Source, Speed, WavSource};
use dmn_core::recorder::{Recorder, WriterReport, open_store};
use dmn_core::{Config, Paths, wire};
use dmn_detect::{Candidate, DetectConfig, Detector, Following, Sensitivity, candidate_channel};
use dmn_llm::{CurrentQuestion, Kind};
use dmn_session::{Action, Dot, Health, SessionHub};
use dmn_stt::{Model, SttConfig, SttStats, SttWorker, SttWorkerConfig, Transcriber, model_path};
use dmn_transcript::{Bus, DEFAULT_WINDOW, Segment, TranscriptLog};
use dmn_vad::{EnergyVad, Vad, VadConfig, VadWorker, WorkerStats, event_channel};

use crate::suggest::{Output, Policy, Suggester, Tally};

/// Bounded queue from each VAD thread to its whisper worker. A live call needs only a few slots,
/// but an unpaced replay hands the VAD a whole interview in seconds, so this holds every
/// utterance of a long fixture rather than dropping them (the SPEC's drop-don't-stall rule is
/// about a live call falling behind, not about replay losing half the interview).
const EVENT_CAPACITY: usize = 512;

/// The live ring depth, 2 s (docs/SPEC.md § Threading rules).
const DEFAULT_CAPACITY_FRAMES: usize = dmn_audio::DEFAULT_CAPACITY_FRAMES;

/// How long the loop waits for a segment before it looks at commands and the stop flag again.
const TICK: Duration = Duration::from_millis(50);

/// Ring frames per lane. A paced lane (`RealTime` or `Factor`) delivers frames on a wall-clock
/// schedule like a live call, so the live 2 s ring is enough and the VAD drains it under the
/// same real-time pressure a call would apply. Only a genuinely unpaced `WavSource` — `dmn
/// replay`'s `--speed` no longer produces one, but the type still allows it — needs a ring sized
/// to the file: it delivers a 12 minute file in about a second, so a 2 s ring would drop almost
/// all of it (the first run of this command lost 32 000 frames). Sizing that case's ring to the
/// file instead makes the producer wait-free and lossless, at 64 KB per second of audio: 12
/// minutes costs ~46 MB.
pub fn ring_frames(audio: Duration, speed: Speed) -> usize {
    match speed {
        Speed::RealTime | Speed::Factor(_) => DEFAULT_CAPACITY_FRAMES,
        Speed::Fast => {
            let frames = audio.as_millis() / u128::from(FRAME_MS);
            (frames as usize).saturating_add(DEFAULT_CAPACITY_FRAMES)
        }
    }
}

/// Whether to transcribe the 1 s partials of an open utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Partials {
    /// On for any paced speed (`RealTime` or `Factor`, which is every `--speed` this command
    /// accepts); off only for an unpaced source, which `--speed` itself never produces.
    Auto,
    On,
    Off,
}

impl Partials {
    /// Partials exist to keep a live view moving while someone is still speaking. Whether
    /// whisper can transcribe one before it is superseded is a question of whisper's real-time
    /// factor at the model and thread count in use, not of the replay speed itself — a paced
    /// lane at any factor gives the pipeline the same real-time pressure a live call would.
    /// Only a genuinely unpaced run, where the whole file lands in a fraction of a second,
    /// makes every partial stale before it starts.
    pub fn enabled(self, speed: Speed) -> bool {
        match self {
            Partials::Auto => !matches!(speed, Speed::Fast),
            Partials::On => true,
            Partials::Off => false,
        }
    }
}

/// What to run and how.
pub struct Options {
    pub interviewer: PathBuf,
    pub candidate: PathBuf,
    /// Paces `WavSource` delivery at this many multiples of wall-clock time; 1.0 is a live call.
    pub speed: f32,
    pub model: Option<String>,
    pub threads: Option<usize>,
    pub partials: Partials,
    /// Score with the SPEC's cue table exactly as written, with no form gate and no vetoes.
    pub spec_literal_detector: bool,
    /// Skip the LLM fast phase (#12): transcribe and detect only.
    pub no_llm: bool,
    /// Keep the session in memory only: write nothing to `dmn.sqlite` (#13).
    pub no_store: bool,
    /// Where the fast phase prints (`dmn replay`) or not (`dmn run`).
    pub output: Output,
    /// Whether a newer question cancels the older one's request (`dmn run`) or every candidate
    /// is answered (`dmn replay`).
    pub policy: Policy,
}

/// Facts about the running pipeline, for a header line.
#[derive(Debug, Clone)]
pub struct Info {
    pub model: String,
    pub threads: usize,
    pub speed: Speed,
    pub partials: bool,
    /// The longer of the two files.
    pub audio: Duration,
}

/// Runs the detector over the finals as they arrive.
///
/// Each interviewer final is scored the moment it arrives with [`Following::UNKNOWN`], exactly as
/// a live session would, and a line that clears the bar is emitted then; that is the latency the
/// SPEC's 100 ms budget is about. A line that falls short is held until the next final: the
/// "followed by >= 1.5 s of silence or a You segment" cue can only be judged once something
/// follows, and if that point lifts it over the bar it is emitted late. Such a candidate cannot
/// arrive within 100 ms of its final by definition, so it is counted apart.
pub struct Detecting {
    detector: Detector,
    /// An interviewer final that did not clear the bar on its own, with when it arrived.
    pending: Option<(Segment, Instant)>,
    found: Vec<Found>,
}

/// One emitted candidate and how long after its final it came out.
pub struct Found {
    pub candidate: Candidate,
    pub latency: Duration,
    /// Emitted only once the next final showed the line was followed.
    pub late: bool,
}

impl Detecting {
    pub fn new(spec_literal: bool, sensitivity: Sensitivity) -> Self {
        let cfg = if spec_literal {
            DetectConfig::spec_literal()
        } else {
            DetectConfig::default()
        };
        Self {
            detector: Detector::new(cfg.with_sensitivity(sensitivity)),
            pending: None,
            found: Vec::new(),
        }
    }

    /// Feed one segment; returns how many candidates became decidable because of it (they are
    /// the last ones in [`found`](Self::found)).
    fn feed(&mut self, seg: &Segment) -> usize {
        if !seg.is_final {
            return 0;
        }
        let before = self.found.len();
        if let Some((prev, arrived)) = self.pending.take() {
            let following = Following {
                silence: Some(seg.start.saturating_sub(prev.end)),
                next_speaker: Some(seg.source),
            };
            self.score(&prev, following, arrived, true);
        }
        if seg.source == Source::Interviewer
            && !self.score(seg, Following::UNKNOWN, Instant::now(), false)
        {
            self.pending = Some((seg.clone(), Instant::now()));
        }
        self.found.len() - before
    }

    /// Score whatever is still held after the audio ends.
    fn flush(&mut self) -> usize {
        let before = self.found.len();
        if let Some((prev, arrived)) = self.pending.take() {
            self.score(&prev, Following::UNKNOWN, arrived, true);
        }
        self.found.len() - before
    }

    /// Score one line; true when it became a candidate.
    fn score(&mut self, seg: &Segment, following: Following, arrived: Instant, late: bool) -> bool {
        match self.detector.observe(seg, following) {
            Some(candidate) => {
                self.found.push(Found {
                    candidate,
                    latency: arrived.elapsed(),
                    late,
                });
                true
            }
            None => false,
        }
    }

    pub fn found(&self) -> &[Found] {
        &self.found
    }

    /// Slowest final-to-candidate time among candidates emitted on arrival, for the check
    /// against the 100 ms budget.
    pub fn slowest(&self) -> Duration {
        self.found
            .iter()
            .filter(|f| !f.late)
            .map(|f| f.latency)
            .max()
            .unwrap_or(Duration::ZERO)
    }

    pub fn late(&self) -> usize {
        self.found.iter().filter(|f| f.late).count()
    }

    #[cfg(test)]
    pub fn detector(&self) -> &Detector {
        &self.detector
    }
}

/// `[detect].sensitivity` from the config file, in the detector's own type (dmn-detect does not
/// depend on dmn-core).
pub fn sensitivity(s: dmn_core::config::Sensitivity) -> Sensitivity {
    match s {
        dmn_core::config::Sensitivity::Eager => Sensitivity::Eager,
        dmn_core::config::Sensitivity::Balanced => Sensitivity::Balanced,
        dmn_core::config::Sensitivity::Strict => Sensitivity::Strict,
    }
}

/// What a command does with the pipeline's output as it happens. Called on the pipeline thread.
pub trait Observer {
    /// A segment from the bus, partial or final (an empty final closes a dropped utterance).
    fn segment(&mut self, _seg: &Segment) {}
    /// A detector candidate, with the session's id for it.
    fn candidate(&mut self, _id: u64, _found: &Found) {}
}

/// Observes nothing: `dmn run` reads the session instead.
pub struct Quiet;
impl Observer for Quiet {}

/// Work that must happen on the pipeline thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `r`: answer the live question again.
    Regenerate,
}

/// The TUI's handle on a running pipeline. Cheap to clone; safe from any thread.
#[derive(Clone)]
pub struct Controls {
    hub: Arc<SessionHub>,
    commands: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
}

impl Controls {
    /// `p`: pin the live question, or unpin it (which shows the newest queued question).
    pub fn toggle_pin(&self) {
        let snap = self.hub.snapshot();
        if snap.pinned {
            self.hub.dispatch(Action::Unpin);
        } else if snap.live.is_some() {
            self.hub.dispatch(Action::Pin);
        }
    }

    /// `r`: regenerate the live question (temperature 0.7, "a different angle").
    pub fn regenerate(&self) {
        let _ = self.commands.send(Command::Regenerate);
    }

    /// `m`: mute or unmute the mic. Muted, the mic lane's VAD hears only silence, so nothing
    /// you say reaches whisper, the transcript or the model; the mic dot shows it, unless the
    /// dot is already red: a failed mic lane stays red, muted or not.
    pub fn toggle_mute(&self) {
        let muted = !self.mic_muted.fetch_xor(true, Ordering::AcqRel);
        if matches!(self.hub.snapshot().status.mic, Health::Error(_)) {
            return;
        }
        self.hub.dispatch(Action::Status {
            dot: Dot::Mic,
            health: if muted { Health::Muted } else { Health::Ok },
        });
    }

    #[cfg(test)]
    pub fn is_muted(&self) -> bool {
        self.mic_muted.load(Ordering::Acquire)
    }

    /// `q`: stop the audio and end the run.
    pub fn quit(&self) {
        self.stop.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn hub(&self) -> &Arc<SessionHub> {
        &self.hub
    }

    /// Controls over a bare session with no pipeline behind them, and the channel `r` sends on.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<Command>) {
        let (commands, rx) = mpsc::channel();
        (
            Self {
                hub: Arc::new(SessionHub::default()),
                commands,
                stop: Arc::new(AtomicBool::new(false)),
                mic_muted: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }
}

/// The mic lane's VAD, with the mute gate in front: muted, every frame is non-speech, so an open
/// utterance closes after the usual hangover and no new one starts. The inner VAD still sees
/// every frame, so its noise floor stays current for when the mic is unmuted.
struct Gated {
    inner: EnergyVad,
    muted: Arc<AtomicBool>,
}

impl Vad for Gated {
    fn is_speech(&mut self, frame: &[f32; FRAME_LEN]) -> bool {
        let speech = self.inner.is_speech(frame);
        speech && !self.muted.load(Ordering::Relaxed)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

struct Lane {
    source: Source,
    wav: WavSource,
    vad: VadWorker,
    stt: SttWorker,
}

/// Everything the run produced, for the summary.
pub struct Report {
    pub stats: Vec<(Source, WorkerStats, SttStats)>,
    /// Audio, VAD or whisper failures; the command fails with these after its summary.
    pub failures: Vec<String>,
    pub wall: Duration,
    pub tally: Option<Tally>,
    pub session: Option<WriterReport>,
    pub detecting: Detecting,
    pub log: TranscriptLog,
    pub bus_dropped: u64,
    pub dropped_candidates: u64,
    /// `q` ended the run before the audio did.
    pub stopped: bool,
}

type Finisher = JoinHandle<(Vec<(Source, WorkerStats, SttStats)>, Vec<String>)>;

pub struct Pipeline {
    info: Info,
    /// When the interviewer's audio started: audio time 0 on the wall clock.
    audio_zero: Instant,
    bus: Arc<Bus>,
    segments: Receiver<Segment>,
    finisher: Finisher,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    commands_tx: mpsc::Sender<Command>,
    commands: mpsc::Receiver<Command>,
    recorder: Recorder,
    hub: Arc<SessionHub>,
    clock: wire::SessionClock,
    suggester: Option<Suggester>,
    detecting: Detecting,
    flushed: bool,
    log: TranscriptLog,
    candidates_tx: crossbeam_channel::Sender<Candidate>,
    candidates_rx: Receiver<Candidate>,
    next_candidate: u64,
    /// Session ids of the candidates in the detector's channel, oldest first.
    sent_ids: VecDeque<u64>,
    dropped_candidates: u64,
    /// Every candidate by session id, for `r`.
    known: HashMap<u64, Candidate>,
}

impl Pipeline {
    /// Build every stage, then start both files together, the session, and the fast phase.
    pub fn start(cfg: &Config, paths: &Paths, opts: &Options) -> Result<Self, Box<dyn Error>> {
        let name = opts.model.clone().unwrap_or_else(|| cfg.stt.model.clone());
        let model = model_path(&paths.models_dir, Model::from_name(&name)?);
        if !model.is_file() {
            return Err(format!(
                "whisper model {} is missing; fix: dmn doctor --fetch {name}",
                model.display()
            )
            .into());
        }
        if !opts.speed.is_finite() || opts.speed <= 0.0 {
            return Err(format!("--speed must be a positive number, not {}", opts.speed).into());
        }
        // #13: the session log. Opened before any audio moves so a bad database fails at once.
        let store = if opts.no_store {
            None
        } else {
            Some(open_store(paths)?)
        };
        let stt_cfg = SttConfig {
            threads: opts.threads.unwrap_or(usize::from(cfg.stt.threads)),
            ..SttConfig::default()
        };
        // #34: `--speed N` paces delivery at N× wall-clock.
        let speed = if (opts.speed - 1.0).abs() < f32::EPSILON {
            Speed::RealTime
        } else {
            Speed::Factor(opts.speed)
        };
        let partials = opts.partials.enabled(speed);
        let bus = Arc::new(Bus::new());
        let segments = bus.subscribe();
        let mic_muted = Arc::new(AtomicBool::new(false));

        // Load both whisper models and spawn every worker first, then start both files back to
        // back: loading a model takes the better part of a second, and starting each file as
        // its lane was built put the candidate's audio that far behind the interviewer's.
        let mut built = Vec::new();
        for (path, source) in [
            (&opts.interviewer, Source::Interviewer),
            (&opts.candidate, Source::You),
        ] {
            let gate = (source == Source::You).then(|| Arc::clone(&mic_muted));
            built.push(build_lane(
                path, source, speed, &model, &stt_cfg, partials, &bus, gate,
            )?);
        }
        let audio = built
            .iter()
            .map(|(l, _)| l.wav.duration())
            .max()
            .unwrap_or_default();
        let audio_zero = Instant::now();
        let mut lanes = Vec::new();
        for (mut lane, sink) in built {
            lane.wav.start(sink)?;
            lanes.push(lane);
        }

        // The same session hub + store writer a live call starts (dmn-core's `Recorder`), fed
        // the same actions: every segment from the bus, every candidate, every fast-phase field.
        let recorder = Recorder::start(store, (!opts.no_store).then_some(paths.db_file.as_path()));
        let hub = recorder.hub();
        // Session time runs at the replay speed, so every stored time lines up with the audio.
        let clock = wire::SessionClock::start(f64::from(opts.speed));
        hub.dispatch(Action::Started {
            label: format!(
                "replay {} + {}",
                opts.interviewer.display(),
                opts.candidate.display()
            ),
            at: clock.started_at(),
        });
        for dot in [Dot::Mic, Dot::System] {
            hub.dispatch(Action::Status {
                dot,
                health: Health::Ok,
            });
        }

        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let finisher = spawn_finisher(lanes, Arc::clone(&hub), &done, &stop);
        let (candidates_tx, candidates_rx) = candidate_channel(dmn_detect::DEFAULT_CAPACITY);
        // #12: each candidate goes through the LLM fast phase on its own thread.
        let suggester = (!opts.no_llm)
            .then(|| Suggester::start(&cfg.llm, opts.output, opts.policy, Arc::clone(&hub), clock));
        let (commands_tx, commands) = mpsc::channel();
        Ok(Self {
            info: Info {
                model: name,
                threads: stt_cfg.threads,
                speed,
                partials,
                audio,
            },
            audio_zero,
            bus,
            segments,
            finisher,
            done,
            stop,
            mic_muted,
            commands_tx,
            commands,
            recorder,
            hub,
            clock,
            suggester,
            detecting: Detecting::new(
                opts.spec_literal_detector,
                sensitivity(cfg.detect.sensitivity),
            ),
            flushed: false,
            log: TranscriptLog::new(),
            candidates_tx,
            candidates_rx,
            next_candidate: 0,
            sent_ids: VecDeque::new(),
            dropped_candidates: 0,
            known: HashMap::new(),
        })
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Audio time 0 on the wall clock: the instant both files started.
    pub fn audio_zero(&self) -> Instant {
        self.audio_zero
    }

    pub fn hub(&self) -> Arc<SessionHub> {
        Arc::clone(&self.hub)
    }

    pub fn controls(&self) -> Controls {
        Controls {
            hub: Arc::clone(&self.hub),
            commands: self.commands_tx.clone(),
            stop: Arc::clone(&self.stop),
            mic_muted: Arc::clone(&self.mic_muted),
        }
    }

    /// Move segments into the session and the detector, and candidates into the fast phase,
    /// until the audio has been transcribed in full (or [`Controls::quit`]). With `keep_alive`
    /// the loop then keeps serving commands until quit, so `r` still works on the last question
    /// after a replay's audio ends.
    pub fn run(&mut self, observer: &mut dyn Observer, keep_alive: bool) {
        loop {
            while let Ok(cmd) = self.commands.try_recv() {
                self.command(cmd);
            }
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            match self.segments.recv_timeout(TICK) {
                Ok(seg) => self.segment(seg, observer),
                Err(_) if self.done.load(Ordering::Acquire) && self.segments.is_empty() => {
                    self.flush(observer);
                    if !keep_alive {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
    }

    fn segment(&mut self, seg: Segment, observer: &mut dyn Observer) {
        self.hub.dispatch(wire::segment(&seg));
        observer.segment(&seg);
        let new = self.detecting.feed(&seg);
        self.emit(new, observer);
        self.log.apply(seg);
    }

    /// Score the line the detector still holds once the audio has ended.
    fn flush(&mut self, observer: &mut dyn Observer) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        let new = self.detecting.flush();
        self.emit(new, observer);
    }

    /// Send the last `new` candidates through the detector's channel into the session and the
    /// fast phase, with the last 60 s of transcript.
    fn emit(&mut self, new: usize, observer: &mut dyn Observer) {
        let found = self.detecting.found();
        for f in &found[found.len() - new..] {
            self.next_candidate += 1;
            if self.candidates_tx.try_send(f.candidate.clone()).is_ok() {
                self.sent_ids.push_back(self.next_candidate);
            } else {
                self.dropped_candidates += 1;
            }
            observer.candidate(self.next_candidate, f);
        }
        for candidate in self.candidates_rx.try_iter() {
            let Some(id) = self.sent_ids.pop_front() else {
                break;
            };
            self.hub.dispatch(wire::candidate(id, &candidate));
            if let Some(s) = &self.suggester {
                s.submit(id, &candidate, self.log.render_window(DEFAULT_WINDOW));
            }
            self.known.insert(id, candidate);
        }
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Regenerate => {
                let Some(s) = &self.suggester else {
                    return;
                };
                let snap = self.hub.snapshot();
                let Some(live) = snap.live else {
                    return;
                };
                let Some(candidate) = self.known.get(&live.candidate) else {
                    return;
                };
                let Some(kind) = kind(&live.kind) else {
                    return;
                };
                self.hub.dispatch(Action::Regenerate {
                    candidate: live.candidate,
                });
                s.regenerate(
                    live.candidate,
                    candidate,
                    self.log.render_window(DEFAULT_WINDOW),
                    CurrentQuestion {
                        question: live.text,
                        kind,
                    },
                );
            }
        }
    }

    /// Drain and join every stage, end the session, and report.
    pub fn finish(mut self, observer: &mut dyn Observer) -> Result<Report, Box<dyn Error>> {
        let stopped = self.stop.load(Ordering::Acquire);
        if !stopped {
            self.flush(observer);
        }
        // `q` may land while the audio is still playing: the finisher sees the flag and stops
        // the sources.
        let (stats, failures) = self
            .finisher
            .join()
            .map_err(|_| "pipeline thread panicked")?;
        // Segments the stages flushed while stopping still belong in the session and the log.
        while let Ok(seg) = self.segments.try_recv() {
            self.hub.dispatch(wire::segment(&seg));
            observer.segment(&seg);
            self.log.apply(seg);
        }
        let wall = self.audio_zero.elapsed();
        // Answer whatever is still queued; the transcript is complete by now. A stopped run
        // cancels instead.
        let tally = self
            .suggester
            .take()
            .map(|s| if stopped { s.stop() } else { s.finish() });
        let session = self.recorder.finish(self.clock.now())?;
        Ok(Report {
            stats,
            failures,
            wall,
            tally,
            session,
            detecting: self.detecting,
            log: self.log,
            bus_dropped: self.bus.dropped(),
            dropped_candidates: self.dropped_candidates,
            stopped,
        })
    }
}

/// The session stores a question's kind as the prompt contract's string.
fn kind(s: &str) -> Option<Kind> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

/// Wait for both files to be delivered (or for `stop`), then close each lane in order: the VAD
/// flushes its open utterance and closes its channel, which ends the whisper worker once it has
/// drained everything. A truncated WAV or a dead thread fails the command and turns that lane's
/// dot red, rather than quietly shortening the transcript.
fn spawn_finisher(
    lanes: Vec<Lane>,
    hub: Arc<SessionHub>,
    done: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
) -> Finisher {
    let done = Arc::clone(done);
    let stop = Arc::clone(stop);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) && lanes.iter().any(|l| l.wav.is_running()) {
            std::thread::sleep(TICK);
        }
        let stopping = stop.load(Ordering::Acquire);
        let mut stats = Vec::new();
        let mut failures = Vec::new();
        for mut lane in lanes {
            let label = lane.source.label();
            let mut lane_failures = Vec::new();
            if let Err(e) = lane.wav.stop() {
                lane_failures.push(format!("{label} audio: {e}"));
            }
            let vad = if stopping {
                lane.vad.stop()
            } else {
                lane.vad.finish()
            };
            let vad = vad.unwrap_or_else(|e| {
                lane_failures.push(format!("{label} VAD: {e}"));
                WorkerStats::default()
            });
            let stt = lane.stt.finish();
            if stt.errors > 0 {
                lane_failures.push(format!(
                    "{label} whisper: {} inference(s) failed",
                    stt.errors
                ));
            }
            if let Some(first) = lane_failures.first() {
                // SPEC § Transcript & status: the dots are the only place errors appear.
                hub.dispatch(Action::Status {
                    dot: match lane.source {
                        Source::You => Dot::Mic,
                        Source::Interviewer | Source::Unknown => Dot::System,
                    },
                    health: Health::Error(first.clone()),
                });
            }
            failures.extend(lane_failures);
            stats.push((lane.source, vad, stt));
        }
        done.store(true, Ordering::Release);
        (stats, failures)
    })
}

/// Build one lane with its file opened but not started; the returned sink starts it.
#[allow(clippy::too_many_arguments)]
fn build_lane(
    path: &Path,
    source: Source,
    speed: Speed,
    model: &Path,
    stt_cfg: &SttConfig,
    partials: bool,
    bus: &Arc<Bus>,
    mute: Option<Arc<AtomicBool>>,
) -> Result<(Lane, FrameSink), Box<dyn Error>> {
    let wav = WavSource::open(path, source, speed)?;
    let (sink, frames) = dmn_audio::frame_channel(source, ring_frames(wav.duration(), speed));
    let (tx, rx) = event_channel(EVENT_CAPACITY);
    let mut vad_cfg = VadConfig::default();
    if !partials {
        vad_cfg.segmenter.partial_every = None;
    }
    let transcriber = Transcriber::load(model, stt_cfg.clone())?;
    let publisher = Arc::clone(bus);
    let stt = SttWorker::spawn(
        Box::new(transcriber),
        rx,
        Box::new(move |seg: Segment| publisher.publish(seg)),
        SttWorkerConfig {
            partials,
            ..SttWorkerConfig::default()
        },
        &source.label().to_lowercase(),
    )?;
    let energy = EnergyVad::new(vad_cfg.energy.clone());
    let vad: Box<dyn Vad> = match mute {
        Some(muted) => Box::new(Gated {
            inner: energy,
            muted,
        }),
        None => Box::new(energy),
    };
    let vad = VadWorker::spawn_with(frames, vad, &vad_cfg.segmenter, tx)?;
    Ok((
        Lane {
            source,
            wav,
            vad,
            stt,
        },
        sink,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_sensitivity_reaches_the_detector() {
        let threshold = |s| {
            Detecting::new(false, sensitivity(s))
                .detector()
                .config()
                .effective_threshold()
        };
        assert_eq!(threshold(dmn_core::config::Sensitivity::Eager), 1);
        assert_eq!(threshold(dmn_core::config::Sensitivity::Balanced), 2);
        assert_eq!(threshold(dmn_core::config::Sensitivity::Strict), 3);
    }

    #[test]
    fn partials_follow_the_pacing_unless_forced() {
        assert!(Partials::Auto.enabled(Speed::RealTime));
        assert!(Partials::Auto.enabled(Speed::Factor(4.0)), "still paced");
        assert!(
            !Partials::Auto.enabled(Speed::Fast),
            "stale before they are run"
        );
        assert!(Partials::On.enabled(Speed::Fast));
        assert!(!Partials::Off.enabled(Speed::RealTime));
    }

    #[test]
    fn an_unpaced_lane_gets_a_ring_that_holds_the_whole_file() {
        // 12 minutes at 20 ms per frame, plus the live margin.
        assert_eq!(ring_frames(Duration::from_secs(725), Speed::Fast), 36_350);
        // Paced replay behaves like a live call: 2 s, and the VAD keeps up, at any factor.
        assert_eq!(
            ring_frames(Duration::from_secs(725), Speed::RealTime),
            DEFAULT_CAPACITY_FRAMES
        );
        assert_eq!(
            ring_frames(Duration::from_secs(725), Speed::Factor(4.0)),
            DEFAULT_CAPACITY_FRAMES
        );
    }

    #[test]
    fn the_mute_gate_turns_speech_into_silence() {
        let loud = [0.5f32; FRAME_LEN];
        let muted = Arc::new(AtomicBool::new(false));
        let mut vad = Gated {
            inner: EnergyVad::new(VadConfig::default().energy),
            muted: Arc::clone(&muted),
        };
        let heard = (0..50).any(|_| vad.is_speech(&loud));
        assert!(heard, "a loud frame is speech while unmuted");
        muted.store(true, Ordering::Relaxed);
        assert!(
            (0..50).all(|_| !vad.is_speech(&loud)),
            "muted, nothing is speech"
        );
    }

    #[test]
    fn a_session_kind_maps_back_to_the_prompt_contract() {
        assert_eq!(kind("system_design"), Some(Kind::SystemDesign));
        assert_eq!(kind("clarification"), Some(Kind::Clarification));
        assert_eq!(kind("nonsense"), None);
    }

    #[test]
    fn controls_pin_toggles_and_mute_shows_on_the_mic_dot() {
        let (controls, rx) = Controls::for_test();
        let hub = Arc::clone(controls.hub());
        controls.toggle_pin();
        assert!(!hub.snapshot().pinned, "nothing on screen to pin");
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "Q".into(),
            kind: "system_design".into(),
        });
        controls.toggle_pin();
        assert!(hub.snapshot().pinned);
        controls.toggle_pin();
        assert!(!hub.snapshot().pinned);

        controls.toggle_mute();
        assert!(controls.is_muted());
        assert_eq!(hub.snapshot().status.mic, Health::Muted);
        controls.toggle_mute();
        assert_eq!(hub.snapshot().status.mic, Health::Ok);

        // A failed mic lane stays red through mute and unmute.
        let failed = Health::Error("You audio: truncated file".into());
        hub.dispatch(Action::Status {
            dot: Dot::Mic,
            health: failed.clone(),
        });
        controls.toggle_mute();
        controls.toggle_mute();
        assert_eq!(hub.snapshot().status.mic, failed);
        assert!(!controls.is_muted(), "the gate itself still toggles");

        controls.regenerate();
        assert_eq!(rx.try_recv(), Ok(Command::Regenerate));
        controls.quit();
        assert!(controls.is_stopped());
    }
}
