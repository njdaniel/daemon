//! The pipeline `dmn replay`, `dmn run --replay` and the live `dmn run` share: two audio lanes
//! (two WAVs, or live PipeWire capture of the mic and the call's output) → VAD → whisper →
//! transcript bus → echo guard → detector → fast phase, all dispatched into one session
//! ([`Recorder`]) that is written to SQLite.
//!
//! The echo guard ([`EchoGuard`], #60) sits where both lanes' finals meet, just past the bus: a
//! mic final that copies the loopback within 300 ms is dropped and counted (`[audio].echo_guard`
//! turns it off); any other mic final waits up to 1.5 s (4 s while an overlapping interviewer
//! utterance is still open) for its possible original, on the wall clock, since the skew between
//! the two whisper lanes is wall time whatever the replay speed. Only the You lane waits.
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
//!
//! A monitor thread watches the lanes while they run (#16): it counts ring-buffer overruns into
//! the session, turns live capture state into the mic / system dots, moves the [`AudioClock`]'s
//! anchor as the interviewer lane consumes frames (so [`Action::Timing`] marks are on the clock
//! segment times use), and samples the process's CPU time into the session log every few
//! seconds. `dmn bench --live` reads all of it back from the store.

use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use dmn_audio::{
    AudioError, AudioSource, CaptureState, CaptureStatus, CaptureTarget, FRAME_LEN, FRAME_MS,
    FrameSink, PipeWireContext, PipeWireSource, PwRecordSource, Source, Speed, WavSource,
};
use dmn_core::config::Vad as VadChoice;
use dmn_core::recorder::{Recorder, WriterReport, open_store};
use dmn_core::wire::AudioClock;
use dmn_core::{Config, Paths, wire};
use dmn_detect::{Candidate, DetectConfig, Detector, Following, Sensitivity, candidate_channel};
use dmn_llm::{CurrentQuestion, Kind};
use dmn_session::{Action, CodingSolutions, Dot, Health, Section, SessionHub, Stage};
use dmn_stt::{
    DeviceChoice, Model, Placement, SttConfig, SttStats, SttWorker, SttWorkerConfig, Transcriber,
    model_path,
};
use dmn_transcript::{Bus, DEFAULT_WINDOW, EchoGuard, Segment, TranscriptLog, Verdict};
use dmn_vad::{
    EnergyVad, SileroVad, SileroVadConfig, Vad, VadConfig, VadWorker, WorkerStats, event_channel,
};

use crate::context::ContextSource;
use crate::suggest::{Answered, Output, Policy, Suggester, Tally};

/// Bounded queue from each VAD thread to its whisper worker. A live call needs only a few slots,
/// but an unpaced replay hands the VAD a whole interview in seconds, so this holds every
/// utterance of a long fixture rather than dropping them (the SPEC's drop-don't-stall rule is
/// about a live call falling behind, not about replay losing half the interview).
const EVENT_CAPACITY: usize = 512;

/// The live ring depth, 2 s (docs/SPEC.md § Threading rules).
const DEFAULT_CAPACITY_FRAMES: usize = dmn_audio::DEFAULT_CAPACITY_FRAMES;

/// How long the loop waits for a segment before it looks at commands and the stop flag again.
const TICK: Duration = Duration::from_millis(50);

/// How much of an answer the slow phase reads to summarise it (#57), in characters: its start
/// and its end when it runs longer, since a promise ("I'll come back to...") can come anywhere.
const ANSWER_CHARS: usize = 2400;

/// The candidate's (`You`) finals that start in `[from, to)`, joined, cut to [`ANSWER_CHARS`]
/// (its first and last halves) when longer.
fn answer_between(finals: &[Segment], from: Duration, to: Duration) -> String {
    let text = finals
        .iter()
        .filter(|s| s.source == Source::You && s.start >= from && s.start < to)
        .map(|s| s.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= ANSWER_CHARS {
        return text;
    }
    let half = ANSWER_CHARS / 2;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{} … {}", head.trim_end(), tail.trim_start())
}

/// How often the monitor samples the process's CPU time into the session log.
const CPU_EVERY: Duration = Duration::from_secs(5);

/// How long a live run waits for PipeWire's first registry enumeration.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Where the two lanes' audio comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Two WAV files, paced at [`Options::speed`]: `dmn replay` and `dmn run --replay`.
    Files {
        interviewer: PathBuf,
        candidate: PathBuf,
    },
    /// Live capture (#16): `mic` is You, `interviewer` is the call's app output stream (or the
    /// default sink monitor). Runs until quit.
    Live {
        mic: CaptureTarget,
        interviewer: CaptureTarget,
    },
}

impl Input {
    /// The session label.
    fn label(&self) -> String {
        match self {
            Input::Files {
                interviewer,
                candidate,
            } => format!("replay {} + {}", interviewer.display(), candidate.display()),
            Input::Live { mic, interviewer } => {
                format!("live {} + {}", describe(mic), describe(interviewer))
            }
        }
    }
}

/// A capture target as the session label and the dots' tooltips name it.
pub fn describe(t: &CaptureTarget) -> String {
    match t {
        CaptureTarget::Mic(None) => "mic (default source)".into(),
        CaptureTarget::Mic(Some(name)) => format!("mic {name}"),
        CaptureTarget::App { application, .. } => format!("app {application}"),
        CaptureTarget::SinkMonitor => "all system audio (sink monitor)".into(),
    }
}

/// What to run and how.
pub struct Options {
    pub input: Input,
    /// Paces `WavSource` delivery at this many multiples of wall-clock time; 1.0 is a live call.
    /// A live input always runs at 1.0.
    pub speed: f32,
    pub model: Option<String>,
    pub threads: Option<usize>,
    /// Trailing silence that ends an utterance, over `[stt].vad_close_ms`.
    pub vad_close_ms: Option<u16>,
    /// Which VAD decides speech, over `[stt].vad` (#62).
    pub vad: Option<VadChoice>,
    /// Where whisper runs (#61), over `[stt].device`.
    pub device: Option<dmn_core::config::SttDevice>,
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
    /// `dmn run --solutions`: the coding-solutions mode for this run, over
    /// `[ui].coding_solutions` (#50).
    pub coding_solutions: Option<CodingSolutions>,
}

/// Facts about the running pipeline, for a header line.
#[derive(Debug, Clone)]
pub struct Info {
    pub model: String,
    pub threads: usize,
    /// Where whisper runs, as [`placement_summary`] words it (#61).
    pub stt_device: String,
    /// "energy" or "silero".
    pub vad: &'static str,
    pub speed: Speed,
    pub partials: bool,
    /// The longer of the two files; zero for a live input, which has no length.
    pub audio: Duration,
    /// Live capture rather than files.
    pub live: bool,
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
    /// The question still being asked (#63), if the last interviewer lines were one.
    chain: Option<Chain>,
    /// Recent You finals' audio spans, for "did the candidate speak in between".
    you: VecDeque<(Duration, Duration)>,
}

/// A question the interviewer may still be asking: the candidate it started from and the
/// interviewer lines since, until the candidate speaks or a pause hands the floor back.
#[derive(Debug, Clone)]
struct Chain {
    /// Index in `found` of the candidate the chain started from.
    cand: usize,
    /// Where the chain's last interviewer final ended.
    end: Duration,
    /// Interviewer lines after that candidate that were not candidates themselves: "Imagine a
    /// delivery app." (candidate) "Orders change state every few seconds." (this) "How would you
    /// get those updates to the phone?" (the continuation).
    between: Vec<String>,
}

/// One emitted candidate and how long after its final it came out.
pub struct Found {
    pub candidate: Candidate,
    pub latency: Duration,
    /// Emitted only once the next final showed the line was followed.
    pub late: bool,
    /// #63: this candidate continues the one before it: the same question, split at a pause.
    /// Its text is the combined question, and it replaces the earlier one on screen.
    pub continues: bool,
}

/// #63: an interviewer final that starts within this long after the previous interviewer final,
/// with no You speech in between, continues it. Since #16 closes an utterance on 500 ms of
/// silence, a question with a mid-sentence pause ("And if the state lived in Redis … and Redis
/// went down, what then?") or a 12 s forced split arrives as two finals; each part scored alone
/// became two candidates for one question. This is the SPEC's hand-back silence
/// ([`dmn_detect::FOLLOW_SILENCE`]): a pause shorter than that has not handed the floor back.
pub const CONTINUE_GAP: Duration = dmn_detect::FOLLOW_SILENCE;

/// A chain spans at most this much audio from its first candidate's start: past it, a long
/// monologue's next question is a new question, not a refinement of one asked half a minute ago.
pub const CONTINUE_SPAN: Duration = Duration::from_secs(30);

/// How many recent You spans [`Detecting`] keeps; only the last few can fall inside a gap.
const YOU_SPANS: usize = 16;

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
            chain: None,
            you: VecDeque::new(),
        }
    }

    /// Feed one segment; returns how many candidates became decidable because of it (they are
    /// the last ones in [`found`](Self::found)).
    fn feed(&mut self, seg: &Segment) -> usize {
        // An empty final is dmn-stt closing an utterance it heard nothing in (a breath, a
        // filtered hallucination): no speech, so it neither scores nor breaks a question in two.
        if !seg.is_final || seg.text.trim().is_empty() {
            return 0;
        }
        let before = self.found.len();
        if seg.source == Source::You {
            if self.you.len() == YOU_SPANS {
                self.you.pop_front();
            }
            self.you.push_back((seg.start, seg.end));
        }
        if let Some((prev, arrived)) = self.pending.take() {
            let following = Following {
                silence: Some(seg.start.saturating_sub(prev.end)),
                next_speaker: Some(seg.source),
            };
            // A line that only clears the bar once followed starts a question of its own.
            if self.score(&prev, following, arrived, true)
                && self.chain.as_ref().is_none_or(|c| c.end == prev.end)
            {
                self.chain = Some(Chain {
                    cand: self.found.len() - 1,
                    end: prev.end,
                    between: Vec::new(),
                });
            }
        }
        if seg.source == Source::Interviewer {
            let open = self.chain.take().filter(|c| self.continues_chain(c, seg));
            if self.score(seg, Following::UNKNOWN, Instant::now(), false) {
                if let Some(chain) = &open {
                    self.continue_from(chain);
                }
                self.chain = Some(Chain {
                    cand: self.found.len() - 1,
                    end: seg.end,
                    between: Vec::new(),
                });
            } else {
                self.pending = Some((seg.clone(), Instant::now()));
                self.chain = open.map(|mut c| {
                    c.end = seg.end;
                    c.between.push(seg.text.trim().to_owned());
                    c
                });
            }
        }
        self.found.len() - before
    }

    /// Whether `seg` is more of the question `chain` holds: it starts within [`CONTINUE_GAP`] of
    /// the chain's last line, no You final lies in the gap, and the chain stays inside
    /// [`CONTINUE_SPAN`]. Judged on audio time, not arrival order: at replay speed, or with
    /// whisper busy, the candidate's answer can be transcribed before the interviewer's second
    /// half arrives.
    fn continues_chain(&self, chain: &Chain, seg: &Segment) -> bool {
        let gap = seg.start.checked_sub(chain.end).unwrap_or(Duration::ZERO);
        let Some(first) = self.found.get(chain.cand) else {
            return false;
        };
        let span = seg
            .end
            .checked_sub(first.candidate.start)
            .unwrap_or(Duration::ZERO);
        let spoke = self
            .you
            .iter()
            .any(|&(start, stop)| start < seg.start && stop > chain.end);
        gap <= CONTINUE_GAP && span <= CONTINUE_SPAN && !spoke
    }

    /// Turn the candidate just pushed into the continuation of `chain`: the combined question,
    /// from where the chain started. The screen then shows one question, refined, not two.
    fn continue_from(&mut self, chain: &Chain) {
        let Some(earlier) = self.found.get(chain.cand).map(|f| f.candidate.clone()) else {
            return;
        };
        let Some(last) = self.found.last_mut() else {
            return;
        };
        let join = |first: &str, rest: &str| {
            std::iter::once(first.trim())
                .chain(chain.between.iter().map(String::as_str))
                .chain(std::iter::once(rest.trim()))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let c = &mut last.candidate;
        c.start = earlier.start;
        c.text = join(&earlier.text, &c.text);
        c.line = join(&earlier.line, &c.line);
        last.continues = true;
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
                    continues: false,
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
    /// `w` (#58): build candidate `candidate`'s STAR block from the story the session moved on
    /// to.
    Swap { candidate: u64 },
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

    /// `s`: switch coding solutions between hints and full (#50). One action per press, so the
    /// session log replays to the same mode.
    pub fn toggle_coding_solutions(&self) {
        let mode = self.hub.snapshot().coding_solutions.toggled();
        self.hub.dispatch(Action::CodingSolutions { mode });
    }

    /// `w` (#58): Swap the live behavioral question's STAR block to the next-ranked story,
    /// wrapping round to the first. The session moves on at once (the header names the new
    /// story, the old cards dim); the pipeline then asks the model for cards from that story.
    /// With one story or none, the session notes there is no other and nothing is asked.
    pub fn swap_story(&self) {
        let Some(live) = self.hub.snapshot().live else {
            return;
        };
        let candidate = live.candidate;
        self.hub.dispatch(Action::SwapStory { candidate });
        if live.stories.len() > 1 {
            let _ = self.commands.send(Command::Swap { candidate });
        }
    }

    /// `1`–`6`: open `section` under the live question's Approach (closing the open one), or
    /// close it (#56). The reducer ignores a row that is not on screen; one action per press,
    /// so the session log replays to the same screen.
    pub fn toggle_section(&self, section: Section) {
        self.hub.dispatch(Action::ToggleSection { section });
    }

    #[cfg(test)]
    pub fn is_muted(&self) -> bool {
        self.mic_muted.load(Ordering::Acquire)
    }

    /// `q`: stop the audio and end the run.
    pub fn quit(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Quit was asked for (by `q`, or by `dmn run --duration`).
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
    inner: Box<dyn Vad>,
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

/// A lane's audio source: a file, which ends, or a live capture, which runs until stopped.
enum Audio {
    Wav(WavSource),
    Live {
        source: Box<dyn AudioSource>,
        /// The PipeWire capture's state, for the dots; `None` for the `pw-record` fallback.
        status: Option<Arc<CaptureStatus>>,
        /// What it captures, for the dots' tooltips.
        target: String,
    },
}

impl Audio {
    /// Still delivering: a file until its end (or a failure), a live capture until stopped. A
    /// `PipeWireSource` never finishes on its own; the `pw-record` fallback does when its child
    /// exits or its reader fails, and then the run ends and reports why, instead of looking
    /// healthy with no audio.
    fn running(&self) -> bool {
        match self {
            Audio::Wav(w) => w.is_running(),
            Audio::Live { source, .. } => !source.is_finished(),
        }
    }

    fn start(&mut self, sink: FrameSink) -> Result<(), AudioError> {
        match self {
            Audio::Wav(w) => w.start(sink),
            Audio::Live { source, .. } => source.start(sink),
        }
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        match self {
            Audio::Wav(w) => w.stop(),
            Audio::Live { source, .. } => source.stop(),
        }
    }
}

struct Lane {
    source: Source,
    audio: Audio,
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
    /// Mic finals the echo guard dropped as copies of the loopback (#60).
    pub echo_dropped: u64,
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
    /// Audio time for [`Action::Timing`] and the headless latency report.
    audio: AudioClock,
    suggester: Option<Suggester>,
    detecting: Detecting,
    /// Between the bus and everything downstream (#60); `[audio].echo_guard`.
    echo: EchoGuard,
    flushed: bool,
    log: TranscriptLog,
    candidates_tx: crossbeam_channel::Sender<Candidate>,
    candidates_rx: Receiver<Candidate>,
    next_candidate: u64,
    /// Session ids of the candidates in the detector's channel, oldest first, and whether each
    /// continues the one before it (#63).
    sent_ids: VecDeque<(u64, bool)>,
    /// The last candidate handed to the session: the one a continuation replaces.
    last_sent: Option<u64>,
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
        let live = matches!(opts.input, Input::Live { .. });
        // CLAUDE.md: never write audio to disk unless `privacy.record_audio` is true. Recording
        // itself is not built yet, so a config asking for it is refused rather than silently
        // ignored: whoever set it expects a recording.
        if live && cfg.privacy.record_audio {
            return Err(
                "[privacy].record_audio = true, but recording a live session is not \
                        built yet; set it to false (the default) to run without recording"
                    .into(),
            );
        }
        // #13: the session log. Opened before any audio moves so a bad database fails at once.
        let store = if opts.no_store {
            None
        } else {
            Some(open_store(paths)?)
        };
        let stt_cfg = SttConfig {
            threads: opts.threads.unwrap_or(usize::from(cfg.stt.threads)),
            device: device_choice(opts.device.unwrap_or(cfg.stt.device)),
            ..SttConfig::default()
        };
        // #34: `--speed N` paces delivery at N× wall-clock. A live call is 1× by definition.
        let speed = if live || (opts.speed - 1.0).abs() < f32::EPSILON {
            Speed::RealTime
        } else {
            Speed::Factor(opts.speed)
        };
        let partials = opts.partials.enabled(speed);
        let vad_close =
            Duration::from_millis(u64::from(opts.vad_close_ms.unwrap_or(cfg.stt.vad_close_ms)));
        // #62: Silero's model is checked before any audio moves, so a missing or wrong file is
        // one clear error naming `dmn doctor --fetch silero`, not a failure mid-call.
        let vad_choice = opts.vad.unwrap_or(cfg.stt.vad);
        let silero = match vad_choice {
            VadChoice::Energy => None,
            VadChoice::Silero => {
                let path = dmn_vad::silero::model_path(&paths.models_dir);
                dmn_vad::silero::verify_model(&path)?;
                Some(path)
            }
        };
        let bus = Arc::new(Bus::new());
        let segments = bus.subscribe();
        let mic_muted = Arc::new(AtomicBool::new(false));

        // #61: whisper may take GPU memory. Load the local LLM first, so that if it (re)loads —
        // a `num_ctx` different from what is loaded, or a cold start — it sizes itself against a
        // GPU whisper has not claimed yet, rather than pushing more of itself onto the CPU.
        if !opts.no_llm && stt_cfg.device != DeviceChoice::Cpu {
            warm_local_llm(&cfg.llm);
        }

        // Load both whisper models and spawn every worker first, then start both sources back
        // to back: loading a model takes the better part of a second, and starting each file as
        // its lane was built put the candidate's audio that far behind the interviewer's.
        let sources = match &opts.input {
            Input::Files {
                interviewer,
                candidate,
            } => vec![
                (
                    Source::Interviewer,
                    Audio::Wav(WavSource::open(interviewer, Source::Interviewer, speed)?),
                ),
                (
                    Source::You,
                    Audio::Wav(WavSource::open(candidate, Source::You, speed)?),
                ),
            ],
            Input::Live { mic, interviewer } => live_sources(mic, interviewer),
        };
        let audio = sources
            .iter()
            .filter_map(|(_, a)| match a {
                Audio::Wav(w) => Some(w.duration()),
                Audio::Live { .. } => None,
            })
            .max()
            .unwrap_or_default();
        // The interviewer lane is built first on purpose (#61): its finals are on the question
        // → Approach path, so when the GPU has room for only one whisper context it gets it,
        // and the candidate's lane falls back to the CPU. `live_sources` lists the mic first, so
        // the order is enforced here for every input.
        let mut built = Vec::new();
        for (source, audio) in interviewer_first(sources) {
            let gate = (source == Source::You).then(|| Arc::clone(&mic_muted));
            let ring = match &audio {
                Audio::Wav(w) => ring_frames(w.duration(), speed),
                Audio::Live { .. } => DEFAULT_CAPACITY_FRAMES,
            };
            built.push(build_lane(
                source,
                audio,
                ring,
                &model,
                &stt_cfg,
                vad_close,
                silero.as_deref(),
                partials,
                &bus,
                gate,
            )?);
        }
        let placements: Vec<Placement> = built.iter().map(|(_, _, p)| p.clone()).collect();
        let stt_device = placement_summary(&placements);
        let audio_zero = Instant::now();
        let mut lanes = Vec::new();
        for (mut lane, sink, _) in built {
            lane.audio.start(sink)?;
            lanes.push(lane);
        }

        // The same session hub + store writer a live call starts (dmn-core's `Recorder`), fed
        // the same actions: every segment from the bus, every candidate, every fast-phase field.
        let recorder = Recorder::start(store, (!opts.no_store).then_some(paths.db_file.as_path()));
        let hub = recorder.hub();
        // Session time runs at the replay speed, so every stored time lines up with the audio.
        let pace = if live { 1.0 } else { f64::from(opts.speed) };
        let clock = wire::SessionClock::start(pace);
        let audio_clock = AudioClock::new(audio_zero, pace);
        hub.dispatch(Action::Started {
            label: opts.input.label(),
            at: clock.started_at(),
        });
        // In the log from the start, so every stored question records the mode it ran under.
        hub.dispatch(Action::CodingSolutions {
            mode: opts.coding_solutions.unwrap_or(cfg.ui.coding_solutions),
        });
        // #61: where whisper ended up, for the status line; a GPU that fell back says so.
        hub.dispatch(Action::Transcriber {
            device: stt_device.clone(),
            fallback: placements.iter().any(Placement::is_fallback),
        });
        for lane in &lanes {
            // A file is fine once open; a live capture's dot follows its state (the monitor).
            let health = match &lane.audio {
                Audio::Wav(_) | Audio::Live { status: None, .. } => Health::Ok,
                Audio::Live {
                    status: Some(_), ..
                } => Health::Unknown,
            };
            hub.dispatch(Action::Status {
                dot: dot(lane.source),
                health,
            });
        }

        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let finisher = spawn_finisher(
            lanes,
            Monitor::new(
                Arc::clone(&hub),
                audio_clock.clone(),
                Arc::clone(&mic_muted),
            ),
            &done,
            &stop,
        );
        let (candidates_tx, candidates_rx) = candidate_channel(dmn_detect::DEFAULT_CAPACITY);
        // #12: each candidate goes through the LLM fast phase on its own thread; #54: with the
        // stored sources and job requirements, read-only under `--no-store`.
        let suggester = (!opts.no_llm).then(|| {
            Suggester::start(
                &cfg.llm,
                opts.output,
                opts.policy,
                Arc::clone(&hub),
                clock,
                audio_clock.clone(),
                ContextSource::Db {
                    path: paths.db_file.clone(),
                    write: !opts.no_store,
                },
                // #59: `[llm]` changes in the config file swap the provider between requests.
                Some(paths.config_file.clone()),
            )
        });
        let (commands_tx, commands) = mpsc::channel();
        Ok(Self {
            info: Info {
                model: name,
                threads: stt_cfg.threads,
                stt_device,
                vad: match vad_choice {
                    VadChoice::Energy => "energy",
                    VadChoice::Silero => "silero",
                },
                speed,
                partials,
                audio,
                live,
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
            audio: audio_clock,
            suggester,
            detecting: Detecting::new(
                opts.spec_literal_detector,
                sensitivity(cfg.detect.sensitivity),
            ),
            echo: EchoGuard::new(cfg.audio.echo_guard),
            flushed: false,
            log: TranscriptLog::new(),
            candidates_tx,
            candidates_rx,
            next_candidate: 0,
            sent_ids: VecDeque::new(),
            last_sent: None,
            dropped_candidates: 0,
            known: HashMap::new(),
        })
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Audio time: the clock [`Action::Timing`] marks and the headless report use.
    pub fn audio_clock(&self) -> AudioClock {
        self.audio.clone()
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
            // Release held mic finals whose wait is over, even when both lanes have gone quiet.
            if self.echo.held() > 0 {
                let verdicts = self.echo.expire(self.audio_zero.elapsed());
                self.verdicts(verdicts, observer);
            }
        }
    }

    /// One segment from the bus, through the echo guard first (#60).
    fn segment(&mut self, seg: Segment, observer: &mut dyn Observer) {
        let verdicts = self.echo.observe(seg, self.audio_zero.elapsed());
        self.verdicts(verdicts, observer);
    }

    fn verdicts(&mut self, verdicts: Vec<Verdict>, observer: &mut dyn Observer) {
        for verdict in verdicts {
            match verdict {
                Verdict::Publish(seg) => self.publish(seg, observer),
                Verdict::Drop(seg) => self.retract(seg, observer),
            }
        }
    }

    /// A segment the guard let through: session, observer, detector, log.
    fn publish(&mut self, seg: Segment, observer: &mut dyn Observer) {
        self.hub.dispatch(wire::segment(&seg));
        observer.segment(&seg);
        let new = self.detecting.feed(&seg);
        self.emit(new, observer);
        self.log.apply(seg);
    }

    /// A mic final the guard dropped as a copy of the loopback: counted into the session, and
    /// its already-shown partial closed with an empty final of the same key (dmn-stt's own "no
    /// text" marker). Its text never reaches the screen, the transcript, the store or the
    /// detector.
    fn retract(&mut self, seg: Segment, observer: &mut dyn Observer) {
        let close = retraction(seg);
        self.hub.dispatch(Action::EchoDropped {
            total: self.echo.dropped(),
        });
        self.hub.dispatch(wire::segment(&close));
        observer.segment(&close);
        self.log.apply(close);
    }

    /// Score the line the detector still holds once the audio has ended.
    fn flush(&mut self, observer: &mut dyn Observer) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        // Whatever the guard still holds goes first: those finals are part of the transcript
        // the detector's last line is scored against.
        let verdicts = self.echo.finish();
        self.verdicts(verdicts, observer);
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
                self.sent_ids.push_back((self.next_candidate, f.continues));
            } else {
                self.dropped_candidates += 1;
            }
            observer.candidate(self.next_candidate, f);
        }
        for candidate in self.candidates_rx.try_iter() {
            let Some((id, continues)) = self.sent_ids.pop_front() else {
                break;
            };
            self.hub.dispatch(wire::candidate(id, &candidate));
            // #63: the same question, split at a pause. Its reply replaces the earlier one's
            // question where it is, instead of arriving as a second question.
            let continued = if continues {
                self.last_sent.inspect(|&prev| {
                    self.hub.dispatch(Action::Continues {
                        candidate: id,
                        continued: prev,
                    });
                })
            } else {
                None
            };
            self.last_sent = Some(id);
            self.hub.dispatch(Action::Timing {
                candidate: id,
                stage: Stage::Emitted,
                at: self.audio.now(),
            });
            if let Some(s) = &self.suggester {
                // A continuation is the question still being asked: nothing has been answered.
                let answered = match continued {
                    Some(_) => None,
                    None => self.answered(&candidate),
                };
                s.submit_after(
                    id,
                    &candidate,
                    self.log.render_window(DEFAULT_WINDOW),
                    answered,
                    continued.is_some(),
                );
            }
            self.known.insert(id, candidate);
        }
    }

    /// #57, session memory: the question on screen as `next` arrives, and what the candidate
    /// has said since it was asked. The next question's slow phase summarises it.
    fn answered(&self, next: &Candidate) -> Option<Answered> {
        let live = self.hub.snapshot().live?;
        let asked = self.known.get(&live.candidate)?.start;
        Some(Answered {
            id: live.candidate,
            question: live.text,
            focus: live.focus,
            at: asked.as_secs_f64(),
            answer: answer_between(self.log.finals(), asked, next.start),
        })
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
            Command::Swap { candidate: id } => {
                let Some(s) = &self.suggester else {
                    return;
                };
                let snap = self.hub.snapshot();
                let Some(q) = snap
                    .live
                    .into_iter()
                    .chain(snap.queued)
                    .find(|q| q.candidate == id)
                else {
                    return;
                };
                let (Some(candidate), Some(kind)) = (self.known.get(&id), kind(&q.kind)) else {
                    return;
                };
                s.swap(
                    id,
                    candidate,
                    self.log.render_window(DEFAULT_WINDOW),
                    CurrentQuestion {
                        question: q.text,
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
        // Detection is already flushed, so these are recorded, not scored; they still pass the
        // echo guard, and whatever it holds is released.
        let mut pending = Vec::new();
        while let Ok(seg) = self.segments.try_recv() {
            pending.extend(self.echo.observe(seg, self.audio_zero.elapsed()));
        }
        pending.extend(self.echo.finish());
        for verdict in pending {
            let seg = match verdict {
                Verdict::Publish(seg) => seg,
                Verdict::Drop(seg) => {
                    self.hub.dispatch(Action::EchoDropped {
                        total: self.echo.dropped(),
                    });
                    retraction(seg)
                }
            };
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
            echo_dropped: self.echo.dropped(),
            stopped,
        })
    }
}

/// The empty final that closes a dropped echo copy's partial: same key, no text.
fn retraction(seg: Segment) -> Segment {
    Segment {
        text: String::new(),
        ..seg
    }
}

/// The session stores a question's kind as the prompt contract's string.
fn kind(s: &str) -> Option<Kind> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

/// The dot a lane's health shows on.
fn dot(source: Source) -> Dot {
    match source {
        Source::You => Dot::Mic,
        Source::Interviewer | Source::Unknown => Dot::System,
    }
}

/// What a live capture's dot should show: red while disconnected or failed (the dots are the
/// only place errors appear, SPEC § Transcript & status), grey until frames flow, muted while
/// the mic is muted, green while streaming.
pub fn capture_health(
    state: CaptureState,
    last_error: Option<String>,
    target: &str,
    muted: bool,
    all_audio: bool,
) -> Health {
    match state {
        CaptureState::Error => {
            Health::Error(last_error.unwrap_or_else(|| format!("{target}: capture failed")))
        }
        CaptureState::Disconnected => Health::Error(format!("{target} is gone; waiting for it")),
        _ if muted => Health::Muted,
        // SPEC § Audio capture: the sink-monitor fallback shows a "capturing all system audio"
        // badge, so a misspelled or absent app never passes for the call.
        CaptureState::Streaming if all_audio => Health::AllAudio,
        CaptureState::Streaming => Health::Ok,
        CaptureState::Idle | CaptureState::Connecting => Health::Unknown,
    }
}

/// Watches the lanes while they run; see the module docs.
struct Monitor {
    hub: Arc<SessionHub>,
    clock: AudioClock,
    mic_muted: Arc<AtomicBool>,
    started: Instant,
    cpu0: Option<f64>,
    last_cpu: Option<Instant>,
    overruns: u64,
    /// Interviewer frames consumed at the last anchor.
    anchored_frames: u64,
    /// Whisper inference time so far, over both lanes, at the last tick.
    whisper_ms: u64,
}

impl Monitor {
    fn new(hub: Arc<SessionHub>, clock: AudioClock, mic_muted: Arc<AtomicBool>) -> Self {
        Self {
            hub,
            clock,
            mic_muted,
            started: Instant::now(),
            cpu0: wire::process_cpu_seconds(),
            last_cpu: None,
            overruns: 0,
            anchored_frames: 0,
            whisper_ms: 0,
        }
    }

    fn tick(&mut self, lanes: &[Lane]) {
        let now = Instant::now();
        self.whisper_ms = lanes.iter().map(|l| l.stt.stats().inference_ms).sum();
        let mut overruns = 0;
        let mut snapshot = None;
        for lane in lanes {
            let stats = lane.vad.stats();
            overruns += stats.overruns;
            let Audio::Live { status, target, .. } = &lane.audio else {
                continue;
            };
            // Segment times count the frames a lane's VAD consumed; anchor the audio clock to
            // the interviewer's, whose segments the questions are.
            if lane.source == Source::Interviewer && stats.frames > self.anchored_frames {
                self.anchored_frames = stats.frames;
                let audio = stats.frames as f64 * f64::from(FRAME_MS) / 1000.0;
                self.clock.anchor(now, audio);
            }
            let Some(status) = status else {
                continue;
            };
            let muted = lane.source == Source::You && self.mic_muted.load(Ordering::Acquire);
            let want = capture_health(
                status.state(),
                status.last_error(),
                target,
                muted,
                status.capturing_all_system_audio(),
            );
            let snap = snapshot.get_or_insert_with(|| self.hub.snapshot().status);
            let shown = match dot(lane.source) {
                Dot::Mic => &snap.mic,
                Dot::System => &snap.system,
                Dot::Model => &snap.model,
            };
            if *shown != want {
                self.hub.dispatch(Action::Status {
                    dot: dot(lane.source),
                    health: want,
                });
            }
        }
        if overruns > self.overruns {
            self.overruns = overruns;
            self.hub.dispatch(Action::Overruns { total: overruns });
        }
        if self
            .last_cpu
            .is_none_or(|t| now.duration_since(t) >= CPU_EVERY)
        {
            self.sample_cpu(now);
        }
    }

    /// One CPU sample into the session log: CPU seconds since the monitor started.
    fn sample_cpu(&mut self, now: Instant) {
        self.last_cpu = Some(now);
        if let (Some(cpu0), Some(cpu)) = (self.cpu0, wire::process_cpu_seconds()) {
            self.hub.dispatch(Action::Cpu {
                at: self.clock.at(now),
                wall: now.duration_since(self.started).as_secs_f64(),
                cpu: cpu - cpu0,
                whisper: self.whisper_ms as f64 / 1000.0,
            });
        }
    }
}

/// Wait for both files to be delivered (or, live, for `stop`) while the [`Monitor`] watches,
/// then close each lane in order: the VAD flushes its open utterance and closes its channel,
/// which ends the whisper worker once it has drained everything. A truncated WAV or a dead
/// thread fails the command and turns that lane's dot red, rather than quietly shortening the
/// transcript.
fn spawn_finisher(
    lanes: Vec<Lane>,
    mut monitor: Monitor,
    done: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
) -> Finisher {
    let done = Arc::clone(done);
    let stop = Arc::clone(stop);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) && lanes.iter().any(|l| l.audio.running()) {
            monitor.tick(&lanes);
            std::thread::sleep(TICK);
        }
        monitor.tick(&lanes);
        monitor.sample_cpu(Instant::now());
        let hub = Arc::clone(&monitor.hub);
        let stopping = stop.load(Ordering::Acquire);
        let mut stats = Vec::new();
        let mut failures = Vec::new();
        for mut lane in lanes {
            let label = lane.source.label();
            let mut lane_failures = Vec::new();
            if let Err(e) = lane.audio.stop() {
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
                    dot: dot(lane.source),
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

/// The live sources: PipeWire capture of `mic` and `interviewer` on one shared context, or,
/// when PipeWire cannot be reached, the `pw-record` emergency fallback (SPEC § Decisions), which
/// can target the mic by name but only the whole sink monitor for the interviewer.
fn live_sources(mic: &CaptureTarget, interviewer: &CaptureTarget) -> Vec<(Source, Audio)> {
    let connected = PipeWireContext::connect().and_then(|ctx| {
        let table = ctx.wait_for_sync(SYNC_TIMEOUT);
        match table.disconnected {
            Some(why) => Err(dmn_audio::PipeWireError::Connect(why)),
            None => Ok(Arc::new(ctx)),
        }
    });
    match connected {
        Ok(ctx) => [(Source::You, mic), (Source::Interviewer, interviewer)]
            .into_iter()
            .map(|(source, target)| {
                let capture = PipeWireSource::new(Arc::clone(&ctx), source, target.clone());
                let status = Some(capture.status());
                (
                    source,
                    Audio::Live {
                        source: Box::new(capture),
                        status,
                        target: describe(target),
                    },
                )
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "PipeWire unreachable; capturing through pw-record");
            let mic_name = match mic {
                CaptureTarget::Mic(Some(name)) => Some(name.as_str()),
                _ => None,
            };
            vec![
                (
                    Source::You,
                    Audio::Live {
                        source: Box::new(PwRecordSource::new(Source::You, mic_name, false)),
                        status: None,
                        target: describe(mic),
                    },
                ),
                (
                    Source::Interviewer,
                    Audio::Live {
                        source: Box::new(PwRecordSource::new(Source::Interviewer, None, true)),
                        status: None,
                        target: describe(&CaptureTarget::SinkMonitor),
                    },
                ),
            ]
        }
    }
}

/// Build one lane around its (not yet started) source; the returned sink starts it.
#[allow(clippy::too_many_arguments)]
fn build_lane(
    source: Source,
    audio: Audio,
    ring: usize,
    model: &Path,
    stt_cfg: &SttConfig,
    vad_close: Duration,
    silero: Option<&Path>,
    partials: bool,
    bus: &Arc<Bus>,
    mute: Option<Arc<AtomicBool>>,
) -> Result<(Lane, FrameSink, Placement), Box<dyn Error>> {
    let (sink, frames) = dmn_audio::frame_channel(source, ring);
    let (tx, rx) = event_channel(EVENT_CAPACITY);
    let mut vad_cfg = VadConfig::default();
    vad_cfg.segmenter.close = vad_close;
    if !partials {
        vad_cfg.segmenter.partial_every = None;
    }
    let transcriber = Transcriber::load(model, stt_cfg.clone())?;
    let placement = transcriber.placement().clone();
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
    // Each lane gets its own Silero session: the model carries recurrent state per stream.
    let inner: Box<dyn Vad> = match silero {
        Some(path) => Box::new(SileroVad::load(path, SileroVadConfig::default())?),
        None => Box::new(EnergyVad::new(vad_cfg.energy.clone())),
    };
    let vad: Box<dyn Vad> = match mute {
        Some(muted) => Box::new(Gated { inner, muted }),
        None => inner,
    };
    let vad = VadWorker::spawn_with(frames, vad, &vad_cfg.segmenter, tx)?;
    Ok((
        Lane {
            source,
            audio,
            vad,
            stt,
        },
        sink,
        placement,
    ))
}

/// Block until the configured Ollama model is loaded (#61). A failure is only logged: the
/// suggester's own warm-up reports it on the model dot. Remote providers use no local GPU.
fn warm_local_llm(llm: &dmn_core::config::Llm) {
    if llm.provider != dmn_core::config::Provider::Ollama {
        return;
    }
    let warmed = wire::provider(llm)
        .map_err(|e| e.to_string())
        .and_then(|p| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(p.warm_up())
                .map_err(|e| e.to_string())
        });
    match warmed {
        Ok(()) => tracing::info!(model = %llm.model, "LLM loaded before GPU whisper"),
        Err(e) => tracing::warn!(error = %e, "LLM warm-up before GPU whisper failed"),
    }
}

/// `[stt].device` in `dmn-stt`'s terms (`dmn-stt` does not depend on `dmn-core`).
pub fn device_choice(d: dmn_core::config::SttDevice) -> DeviceChoice {
    match d {
        dmn_core::config::SttDevice::Cpu => DeviceChoice::Cpu,
        dmn_core::config::SttDevice::Gpu => DeviceChoice::Gpu,
        dmn_core::config::SttDevice::Auto => DeviceChoice::Auto,
    }
}

/// The interviewer's lane before the candidate's, whatever order `sources` came in.
fn interviewer_first<T>(mut sources: Vec<(Source, T)>) -> Vec<(Source, T)> {
    sources.sort_by_key(|(source, _)| *source != Source::Interviewer);
    sources
}

/// One line for where the lanes' whisper runs: the shared placement when both lanes agree (the
/// usual case), else each lane's.
fn placement_summary(placements: &[Placement]) -> String {
    match placements {
        [] => String::new(),
        [first, rest @ ..] if rest.iter().all(|p| p.same_device(first)) => first.label(),
        all => all
            .iter()
            .map(Placement::label)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_lanes_on_the_same_device_read_as_one() {
        let cpu = Placement::Cpu { reason: None };
        assert_eq!(placement_summary(&[cpu.clone(), cpu.clone()]), "CPU");
        let fell_back = Placement::Cpu {
            reason: Some("Vulkan0 failed: out of memory".into()),
        };
        assert_eq!(
            placement_summary(&[cpu, fell_back]),
            "CPU; CPU (fallback: Vulkan0 failed: out of memory)"
        );
        assert_eq!(placement_summary(&[]), "");

        // Two lanes on one GPU: free memory differs after the first load, the device does not.
        let gpu = |free: u64| {
            Placement::Gpu(dmn_stt::GpuDevice {
                ordinal: 0,
                name: "Vulkan0".into(),
                description: "RTX 4090".into(),
                integrated: false,
                free_bytes: free,
                total_bytes: 25_800_000_000,
            })
        };
        let both = placement_summary(&[gpu(2_600_000_000), gpu(1_300_000_000)]);
        assert!(!both.contains("; "), "one device, one label: {both}");
    }

    #[test]
    fn the_interviewer_lane_is_built_first_whatever_the_input_order() {
        // `live_sources` lists the mic first; the GPU must still go to the interviewer's lane.
        let live = vec![(Source::You, "mic"), (Source::Interviewer, "app")];
        assert_eq!(
            interviewer_first(live),
            [(Source::Interviewer, "app"), (Source::You, "mic")]
        );
        let files = vec![(Source::Interviewer, "i.wav"), (Source::You, "c.wav")];
        assert_eq!(interviewer_first(files.clone()), files);
    }

    #[test]
    fn echo_retraction_closes_the_copys_partial_and_keeps_nothing() {
        let partial = Segment {
            source: Source::You,
            start: Duration::from_secs(10),
            end: Duration::from_secs(11),
            text: "How would you".into(),
            is_final: false,
            avg_logprob: -0.2,
        };
        let copy = Segment {
            end: Duration::from_secs(12),
            text: "How would you design a rate limiter?".into(),
            is_final: true,
            ..partial.clone()
        };
        let close = retraction(copy.clone());
        assert_eq!(close.key(), copy.key(), "same utterance");
        assert!(close.is_final && close.text.is_empty());
        let mut log = TranscriptLog::new();
        log.apply(partial);
        log.apply(close);
        assert!(log.finals().is_empty(), "the copy's text never lands");
        assert!(
            log.render_window(DEFAULT_WINDOW).is_empty(),
            "and its partial is gone from the window the LLM sees"
        );
    }

    fn final_at(source: Source, start_ms: u64, end_ms: u64, text: &str) -> Segment {
        Segment {
            source,
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.into(),
            is_final: true,
            avg_logprob: -0.2,
        }
    }

    fn detecting() -> Detecting {
        Detecting::new(false, sensitivity(dmn_core::config::Sensitivity::Balanced))
    }

    /// sysdesign's first question as whisper split it at #16's 500 ms close: two finals 0.6 s
    /// apart, each a candidate on its own (#63).
    const FIRST: &str = "How would you design a rate limiter for a globally distributed API?";
    const SECOND: &str = "And how would you handle the case where the limiter itself goes down?";

    #[test]
    fn a_question_split_at_a_pause_continues_as_one_candidate() {
        let mut d = detecting();
        assert_eq!(
            d.feed(&final_at(Source::Interviewer, 19_980, 26_000, FIRST)),
            1
        );
        assert!(!d.found()[0].continues);
        assert_eq!(
            d.feed(&final_at(Source::Interviewer, 26_560, 29_740, SECOND)),
            1
        );
        let second = &d.found()[1];
        assert!(
            second.continues,
            "the rest of the question, not a second one"
        );
        assert_eq!(second.candidate.start, Duration::from_millis(19_980));
        assert_eq!(second.candidate.text, format!("{FIRST} {SECOND}"));
    }

    #[test]
    fn the_candidates_answer_in_the_gap_ends_the_question() {
        let mut d = detecting();
        d.feed(&final_at(Source::Interviewer, 0, 5_000, FIRST));
        d.feed(&final_at(Source::You, 5_200, 5_900, "Sure."));
        d.feed(&final_at(Source::Interviewer, 6_000, 9_000, SECOND));
        assert!(
            !d.found()[1].continues,
            "the candidate spoke: a new question"
        );
    }

    #[test]
    fn an_answer_transcribed_early_but_spoken_later_does_not_end_it() {
        // At replay speed the answer (spoken after the second half) can arrive first.
        let mut d = detecting();
        d.feed(&final_at(Source::Interviewer, 19_980, 26_000, FIRST));
        d.feed(&final_at(
            Source::You,
            31_040,
            43_040,
            "Okay, first I'd clarify the scope.",
        ));
        d.feed(&final_at(Source::Interviewer, 26_560, 29_740, SECOND));
        assert!(d.found()[1].continues);
    }

    #[test]
    fn a_pause_that_hands_the_floor_back_starts_a_new_question() {
        let mut d = detecting();
        d.feed(&final_at(Source::Interviewer, 0, 5_000, FIRST));
        let gap = u64::try_from(CONTINUE_GAP.as_millis()).unwrap_or(1_500) + 100;
        d.feed(&final_at(
            Source::Interviewer,
            5_000 + gap,
            9_000 + gap,
            SECOND,
        ));
        assert!(!d.found()[1].continues);
    }

    #[test]
    fn a_forced_split_at_12_s_continues() {
        let mut d = detecting();
        d.feed(&final_at(
            Source::Interviewer,
            148_400,
            160_400,
            "Sorry about that. Where were we? Right, the pipeline. Let's switch gears to something \
             more hands-on. Suppose you have a service whose P99 latency doubled overnight.",
        ));
        d.feed(&final_at(
            Source::Interviewer,
            161_180,
            162_940,
            "How do you go about finding the cause?",
        ));
        let found = d.found();
        assert_eq!(
            found.len(),
            2,
            "{:?}",
            found.iter().map(|f| &f.candidate.text).collect::<Vec<_>>()
        );
        assert!(found[1].continues);
    }

    #[test]
    fn an_empty_final_between_the_halves_does_not_break_the_question() {
        let mut d = detecting();
        d.feed(&final_at(Source::Interviewer, 19_980, 26_000, FIRST));
        assert_eq!(
            d.feed(&final_at(Source::Interviewer, 26_100, 26_300, "")),
            0
        );
        d.feed(&final_at(Source::Interviewer, 26_560, 29_740, SECOND));
        assert!(d.found()[1].continues);
    }

    #[test]
    fn a_question_in_three_parts_continues_through_the_middle_statement() {
        let mut d = detecting();
        d.feed(&final_at(
            Source::Interviewer,
            208_700,
            212_700,
            "Imagine you're building the notification service for a food delivery app.",
        ));
        d.feed(&final_at(
            Source::Interviewer,
            213_600,
            216_000,
            "Orders change state every few seconds.",
        ));
        d.feed(&final_at(
            Source::Interviewer,
            216_900,
            219_600,
            "How would you get those updates to the customer's phone quickly?",
        ));
        let last = d.found().last().unwrap();
        assert!(last.continues);
        assert_eq!(last.candidate.start, Duration::from_millis(208_700));
        assert_eq!(
            last.candidate.text,
            "Imagine you're building the notification service for a food delivery app. Orders \
             change state every few seconds. How would you get those updates to the customer's \
             phone quickly?"
        );
    }

    #[test]
    fn a_chain_ends_after_30_s() {
        let mut d = detecting();
        d.feed(&final_at(Source::Interviewer, 0, 5_000, FIRST));
        let mut at = 5_000;
        for _ in 0..5 {
            d.feed(&final_at(
                Source::Interviewer,
                at + 500,
                at + 6_000,
                "That's the setup.",
            ));
            at += 6_000;
        }
        d.feed(&final_at(Source::Interviewer, at + 500, at + 4_000, SECOND));
        assert!(
            !d.found().last().unwrap().continues,
            "34 s in: a new question"
        );
    }

    #[test]
    fn only_a_candidate_is_continued() {
        let mut d = detecting();
        d.feed(&final_at(
            Source::Interviewer,
            0,
            3_000,
            "Okay, that makes sense.",
        ));
        d.feed(&final_at(Source::Interviewer, 3_400, 7_000, FIRST));
        assert_eq!(d.found().len(), 1);
        assert!(
            !d.found()[0].continues,
            "a statement before a question is not a candidate"
        );
    }

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
    fn an_answer_is_the_candidates_words_between_two_questions() {
        let seg = |source, start: u64, text: &str| Segment {
            source,
            start: Duration::from_secs(start),
            end: Duration::from_secs(start + 3),
            text: text.into(),
            is_final: true,
            avg_logprob: -0.2,
        };
        let finals = [
            seg(Source::You, 5, "Before the question."),
            seg(Source::Interviewer, 10, "Design a cache."),
            seg(Source::You, 14, "LRU in front of Redis."),
            seg(Source::Interviewer, 20, "Mm-hm."),
            seg(Source::You, 22, " I'll come back to eviction. "),
            seg(Source::You, 40, "After the next question."),
        ];
        let from = Duration::from_secs(10);
        let to = Duration::from_secs(35);
        assert_eq!(
            answer_between(&finals, from, to),
            "LRU in front of Redis. I'll come back to eviction."
        );
        assert_eq!(answer_between(&finals, to, from), "", "an empty range");

        let long = [seg(Source::You, 12, &"word ".repeat(1000))];
        let cut = answer_between(&long, from, to);
        assert!(cut.chars().count() <= ANSWER_CHARS + 3, "{}", cut.len());
        assert!(cut.contains(" … "), "its start and its end are kept");
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
            inner: Box::new(EnergyVad::new(VadConfig::default().energy)),
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

    /// A live source that has already ended, like a `pw-record` child that exited.
    struct Ended;

    impl AudioSource for Ended {
        fn source(&self) -> Source {
            Source::Interviewer
        }
        fn start(&mut self, _sink: FrameSink) -> Result<(), AudioError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
        fn is_finished(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_live_source_that_ends_on_its_own_stops_the_run() {
        let audio = Audio::Live {
            source: Box::new(Ended),
            status: None,
            target: "all system audio (pw-record)".into(),
        };
        assert!(
            !audio.running(),
            "a dead fallback must not look like a live call"
        );
    }

    #[test]
    fn a_live_capture_dot_follows_its_state() {
        let t = "app Firefox";
        assert_eq!(
            capture_health(CaptureState::Connecting, None, t, false, false),
            Health::Unknown
        );
        assert_eq!(
            capture_health(CaptureState::Streaming, None, t, false, false),
            Health::Ok
        );
        assert_eq!(
            capture_health(CaptureState::Streaming, None, t, true, false),
            Health::Muted
        );
        assert_eq!(
            capture_health(CaptureState::Disconnected, None, t, true, false),
            Health::Error("app Firefox is gone; waiting for it".into()),
            "an error shows even while muted"
        );
        assert_eq!(
            capture_health(CaptureState::Error, Some("format".into()), t, false, false),
            Health::Error("format".into())
        );
        assert_eq!(
            capture_health(CaptureState::Streaming, None, t, false, true),
            Health::AllAudio,
            "the app was not found, so the sink monitor stands in and says so"
        );
        assert_eq!(
            capture_health(CaptureState::Streaming, None, t, true, true),
            Health::Muted,
            "mute is the user's own choice and wins"
        );
    }

    #[test]
    fn a_live_input_is_labelled_by_what_it_captures() {
        let live = Input::Live {
            mic: CaptureTarget::Mic(None),
            interviewer: CaptureTarget::interviewer("Firefox"),
        };
        assert_eq!(live.label(), "live mic (default source) + app Firefox");
        assert_eq!(
            describe(&CaptureTarget::interviewer("")),
            "all system audio (sink monitor)"
        );
        let files = Input::Files {
            interviewer: "a.wav".into(),
            candidate: "b.wav".into(),
        };
        assert_eq!(files.label(), "replay a.wav + b.wav");
    }

    #[test]
    fn a_live_run_refuses_record_audio_rather_than_ignoring_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.models_dir).unwrap();
        let mut cfg = Config::default();
        cfg.privacy.record_audio = true;
        // A model file must exist for start to get as far as the privacy check.
        std::fs::write(
            model_path(&paths.models_dir, Model::from_name(&cfg.stt.model).unwrap()),
            b"",
        )
        .unwrap();
        let err = Pipeline::start(
            &cfg,
            &paths,
            &Options {
                input: Input::Live {
                    mic: CaptureTarget::Mic(None),
                    interviewer: CaptureTarget::SinkMonitor,
                },
                speed: 1.0,
                model: None,
                threads: None,
                vad_close_ms: None,
                vad: None,
                device: None,
                partials: Partials::Auto,
                spec_literal_detector: false,
                no_llm: true,
                no_store: true,
                output: Output::Quiet,
                policy: Policy::Supersede,
                coding_solutions: None,
            },
        )
        .err()
        .expect("refused")
        .to_string();
        assert!(err.contains("record_audio"), "{err}");
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
