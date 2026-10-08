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
//! quit stops the audio sources so the run ends early and cleanly. Enter (#96) goes to the
//! pipeline thread too: it sends the asking lane's last turn ([`last_turn`]) to the fast phase
//! as a candidate the detector never scored, for a question the detector missed.
//!
//! Interview assistance (#79) is the layer past the transcript: the detector, the context and
//! the LLM, held in [`Assistance`]. Off (`[assist].enabled = false`, `dmn run --no-assist`, or
//! Ctrl+Shift+A), every segment still reaches the session, the store and the log, but none is scored and
//! nothing is asked of the model; on again, the next question is detected as usual. A run that
//! starts with it off never warms the LLM, and with a local Ollama asks it to unload before
//! whisper loads, so whisper can have the GPU; the suggester then starts on the first Ctrl+Shift+A.
//! With `[speak]` on, `t` translates on `[llm].model` unless `[speak].model` names a translator
//! of its own (#113), in which case only that one is loaded while assistance is off.
//!
//! `t` (#109) is the pipeline thread's too, and independent of assistance: [`Speaking`] takes the
//! You lane's last turn (as [`last_turn`] builds the asking lane's for Enter) and hands it to the
//! session's [`Speaker`], which translates it, synthesizes it in the user's voice and plays it
//! into `dmn-voice`. The loop counts the seconds on the status line, takes its failure notice
//! down after 5 s, and turns the speaker's playback windows into audio time for the echo guard,
//! which drops what either lane heard while dmn was speaking.
//!
//! The voice mode (#114) is [`Speaking`]'s too: in pass-through mode (the default) the user's
//! own microphone goes through `dmn-voice` and `t` is as above; in translate mode (`l`, or
//! `[speak].mode`) the microphone is shut off from the node and every finalized You turn the
//! guard lets through is queued and spoken translated, in order, one at a time.
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
use dmn_detect::{
    Candidate, DetectConfig, Detector, Following, QuestionSource, Sensitivity, candidate_channel,
};
use dmn_llm::{CurrentQuestion, Kind};
use dmn_session::{
    Action, CodingSolutions, Dot, Health, PickerOp, Section, Session, SessionHub, Stage, VoiceMode,
};
use dmn_stt::{
    DeviceChoice, Model, Placement, SttConfig, SttStats, SttWorker, SttWorkerConfig, Transcriber,
    model_path,
};
use dmn_transcript::{Bus, DEFAULT_WINDOW, EchoGuard, Segment, TranscriptLog, Verdict};
use dmn_vad::{
    EnergyVad, SileroVad, SileroVadConfig, Vad, VadConfig, VadWorker, WorkerStats, event_channel,
};
use tokio::sync::watch;

use crate::context::ContextSource;
use crate::speaker::{Language, Speaker, Window};
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
    /// The audio's language (#77), over `[stt].language`.
    pub language: Option<dmn_core::config::SttLanguage>,
    pub partials: Partials,
    /// Score with the SPEC's cue table exactly as written, with no form gate and no vetoes.
    pub spec_literal_detector: bool,
    /// `--question-source` (#90), over `[detect].question_source`: whose finals are scored and
    /// answered. Fixed for the session.
    pub question_source: Option<dmn_core::config::QuestionSource>,
    /// Skip the LLM fast phase (#12): transcribe and detect only.
    pub no_llm: bool,
    /// Interview assistance at the start (#79), over `[assist].enabled`: `Some(false)` is
    /// `dmn run --no-assist`. Off, nothing is detected and the LLM is never asked.
    pub assist: Option<bool>,
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
    /// `[stt].language` as configured: "en", "auto" or a code (#77).
    pub language: String,
    pub speed: Speed,
    pub partials: bool,
    /// The longer of the two files; zero for a live input, which has no length.
    pub audio: Duration,
    /// Live capture rather than files.
    pub live: bool,
}

/// Runs the detector over the finals as they arrive.
///
/// Each final from a lane `[detect].question_source` selects (#90; the Interviewer lane by
/// default) is scored the moment it arrives with [`Following::UNKNOWN`], exactly as a live
/// session would, and a line that clears the bar is emitted then; that is the latency the SPEC's
/// 100 ms budget is about. A line that falls short is held until the next final: the "followed
/// by 1.5 s of silence or the other lane" cue can only be judged once something follows, and if
/// that point lifts it over the bar it is emitted late. Such a candidate cannot arrive within
/// 100 ms of its final by definition, so it is counted apart.
pub struct Detecting {
    detector: Detector,
    /// A final from an eligible lane that did not clear the bar on its own, with when it arrived.
    /// One slot whatever the source: the next final from either lane resolves it.
    pending: Option<(Segment, Instant)>,
    found: Vec<Found>,
    /// The question still being asked (#63), if the last lines on one lane were one.
    chain: Option<Chain>,
    /// Recent finals' audio spans per lane, for "did the other side speak in between".
    spans: Spans,
}

/// A question one lane may still be asking: the candidate it started from and that lane's lines
/// since, until the other side speaks or a pause hands the floor back.
#[derive(Debug, Clone)]
struct Chain {
    /// Index in `found` of the candidate the chain started from.
    cand: usize,
    /// The lane the chain is on (#90). A final from the other lane never continues it.
    source: Source,
    /// Where the chain's last final ended.
    end: Duration,
    /// Lines on the same lane after that candidate that were not candidates themselves:
    /// "Imagine a delivery app." (candidate) "Orders change state every few seconds." (this)
    /// "How would you get those updates to the phone?" (the continuation).
    between: Vec<String>,
}

/// The recent audio spans of each lane's finals (#90). With the default source only the You
/// lane's are ever read, which is what `Detecting` kept before.
#[derive(Debug, Clone, Default)]
struct Spans {
    you: VecDeque<(Duration, Duration)>,
    interviewer: VecDeque<(Duration, Duration)>,
}

impl Spans {
    fn lane(&mut self, source: Source) -> Option<&mut VecDeque<(Duration, Duration)>> {
        match source {
            Source::You => Some(&mut self.you),
            Source::Interviewer => Some(&mut self.interviewer),
            // A single-stream fallback is no lane: nothing scores it, and nothing interrupts.
            Source::Unknown => None,
        }
    }

    fn push(&mut self, source: Source, span: (Duration, Duration)) {
        if let Some(lane) = self.lane(source) {
            if lane.len() == YOU_SPANS {
                lane.pop_front();
            }
            lane.push_back(span);
        }
    }

    /// Whether the lane that is not `source` spoke across `(after, before)`.
    fn spoke_between(&self, source: Source, after: Duration, before: Duration) -> bool {
        let other = match source {
            Source::You => &self.interviewer,
            Source::Interviewer | Source::Unknown => &self.you,
        };
        other
            .iter()
            .any(|&(start, stop)| start < before && stop > after)
    }
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

/// How many recent spans [`Detecting`] keeps per lane; only the last few can fall inside a gap.
const YOU_SPANS: usize = 16;

/// #96: how long the status line says `not a question` after the model declined what Enter sent.
pub const NOT_A_QUESTION_FOR: Duration = Duration::from_secs(5);
/// #102: how long the status line says `lookup failed`.
pub const LOOKUP_FAILED_FOR: Duration = Duration::from_secs(5);

/// #109: how long the status line says why a `t` press spoke nothing.
pub const SPEAK_FAILED_FOR: Duration = Duration::from_secs(5);
/// #109: how long after the last sample played the echo guard's playback window stays open:
/// the VAD's hangover and the node's own buffer.
const SPEAK_TAIL: Duration = Duration::from_millis(300);

/// One status-line notice (#96, #102, #109): `showing` is the id whose notice the session has
/// up, `tracked` the one this thread has seen and since when. Once it has been up for `lasts`,
/// the notice is taken down with the action `expired` makes for it ([`Action::NoticeExpired`]
/// for a candidate's). Returns what to track from now.
fn expire(
    hub: &SessionHub,
    showing: Option<u64>,
    tracked: Option<(u64, Instant)>,
    now: Instant,
    lasts: Duration,
    expired: fn(u64) -> Action,
) -> Option<(u64, Instant)> {
    match (showing, tracked) {
        (Some(id), Some((seen, since))) if id == seen => {
            if now.saturating_duration_since(since) < lasts {
                return tracked;
            }
            hub.dispatch(expired(id));
            None
        }
        (Some(id), _) => Some((id, now)),
        (None, _) => None,
    }
}

/// #96: what Enter sends, the asking lane's last turn: that lane's most recent finals with no
/// final from the other lane between them, in spoken order, going back at most
/// [`CONTINUE_SPAN`] from the turn's end. It is one candidate whose text is those finals joined;
/// the fast phase already reduces "what was just said" to the question in it, so a turn that
/// opens with remarks and ends with the question needs no rule here. The lane is the one `asks`
/// selects (#90); with `both`, whichever spoke last. `None` until that lane has said something.
///
/// "Between" is judged on audio time, as a split question's gap is ([`Spans::spoke_between`]):
/// the other lane's "mm-hm" inside a line does not end the turn, one in the gap before it does.
/// The detector never scored this candidate, so it has score 0 and no cues.
///
/// An empty final (an utterance whisper heard nothing in, or an echo copy's retraction) is no
/// speech here either, as for a split question (#63): on the other lane it does not cut the
/// turn, and on the asking lane it is neither the turn's last line nor one of its lines.
/// [`TranscriptLog`] keeps no such final (dmn-stt trims its text, and the log drops an empty
/// one), so this holds for any slice, not only the log's.
pub(crate) fn last_turn(finals: &[Segment], asks: QuestionSource) -> Option<Candidate> {
    let finals: Vec<&Segment> = finals
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .collect();
    let last = *finals
        .iter()
        .filter(|s| asks.allows(s.source))
        .max_by_key(|s| s.end)?;
    let lane = last.source;
    let mut turn = vec![last];
    let mut first = last;
    for &seg in finals
        .iter()
        .rev()
        .filter(|s| s.source == lane && s.start < last.start)
    {
        // A single-stream fallback is no lane (see `Spans`): it interrupts nothing.
        let spoke = finals.iter().any(|o| {
            o.source != lane
                && o.source != Source::Unknown
                && o.start < first.start
                && o.end > seg.end
        });
        if spoke || last.end.saturating_sub(seg.start) > CONTINUE_SPAN {
            break;
        }
        turn.push(seg);
        first = seg;
    }
    let text = turn
        .iter()
        .rev()
        .map(|s| s.text.trim())
        .collect::<Vec<_>>()
        .join(" ");
    Some(Candidate {
        source: lane,
        start: first.start,
        end: last.end,
        line: text.clone(),
        text,
        score: 0,
        cues: Vec::new(),
    })
}

impl Detecting {
    pub fn new(
        spec_literal: bool,
        sensitivity: Sensitivity,
        question_source: QuestionSource,
    ) -> Self {
        let cfg = if spec_literal {
            DetectConfig::spec_literal()
        } else {
            DetectConfig::default()
        };
        Self {
            detector: Detector::new(
                cfg.with_sensitivity(sensitivity)
                    .with_question_source(question_source),
            ),
            pending: None,
            found: Vec::new(),
            chain: None,
            spans: Spans::default(),
        }
    }

    /// Whose finals this run scores (#90).
    pub fn question_source(&self) -> QuestionSource {
        self.detector.config().question_source
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
        self.spans.push(seg.source, (seg.start, seg.end));
        if let Some((prev, arrived)) = self.pending.take() {
            let following = Following {
                silence: Some(seg.start.saturating_sub(prev.end)),
                next_speaker: Some(seg.source),
            };
            // A line that only clears the bar once followed starts a question of its own.
            if self.score(&prev, following, arrived, true)
                && self
                    .chain
                    .as_ref()
                    .is_none_or(|c| c.source == prev.source && c.end == prev.end)
            {
                self.chain = Some(Chain {
                    cand: self.found.len() - 1,
                    source: prev.source,
                    end: prev.end,
                    between: Vec::new(),
                });
            }
        }
        if self.question_source().allows(seg.source) {
            // #90: a chain never crosses lanes. In `both`, a You question asked while an
            // interviewer chain is open ends that chain and starts its own.
            let open = self
                .chain
                .take()
                .filter(|c| c.source == seg.source && self.continues_chain(c, seg));
            if self.score(seg, Following::UNKNOWN, Instant::now(), false) {
                if let Some(chain) = &open {
                    self.continue_from(chain);
                }
                self.chain = Some(Chain {
                    cand: self.found.len() - 1,
                    source: seg.source,
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
    /// the chain's last line, no final from the *other* lane lies in the gap (#90), and the chain
    /// stays inside [`CONTINUE_SPAN`]. Judged on audio time, not arrival order: at replay speed,
    /// or with whisper busy, the answer can be transcribed before the question's second half
    /// arrives.
    fn continues_chain(&self, chain: &Chain, seg: &Segment) -> bool {
        let gap = seg.start.checked_sub(chain.end).unwrap_or(Duration::ZERO);
        let Some(first) = self.found.get(chain.cand) else {
            return false;
        };
        let span = seg
            .end
            .checked_sub(first.candidate.start)
            .unwrap_or(Duration::ZERO);
        let spoke = self.spans.spoke_between(chain.source, chain.end, seg.start);
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

    /// Forget the line held for its follower and the question being asked (#79): assistance
    /// went off, and whatever follows the gap must not complete a question from before it.
    fn pause(&mut self) {
        self.pending = None;
        self.chain = None;
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

/// `[detect].question_source` (#90) in the detector's own type, as [`sensitivity`] above.
pub fn question_source(s: dmn_core::config::QuestionSource) -> QuestionSource {
    match s {
        dmn_core::config::QuestionSource::Interviewer => QuestionSource::Interviewer,
        dmn_core::config::QuestionSource::You => QuestionSource::You,
        dmn_core::config::QuestionSource::Both => QuestionSource::Both,
    }
}

/// `[detect].question_source` (#90) as the session records it, for the status line and the
/// listening placeholder.
pub fn session_question_source(s: dmn_core::config::QuestionSource) -> dmn_session::QuestionSource {
    match s {
        dmn_core::config::QuestionSource::Interviewer => dmn_session::QuestionSource::Interviewer,
        dmn_core::config::QuestionSource::You => dmn_session::QuestionSource::You,
        dmn_core::config::QuestionSource::Both => dmn_session::QuestionSource::Both,
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
    /// Ctrl+Shift+A (#79): interview assistance on or off from the next segment.
    Assist { on: bool },
    /// Enter (#96): send the asking lane's last turn to the model, detector or no detector.
    Ask,
    /// `t` (#109): speak the You lane's last turn in `[speak].target`, assistance or not.
    Speak,
    /// `l` (#114): pass-through ↔ translate mode.
    Voice,
    /// Enter on the picker (#115): speak the language at `index` in the session's list from
    /// the next utterance on, and save it to `[speak].target`.
    SpeakTarget { index: usize },
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
    /// `p`: pin the live question, or unpin it (which shows the newest queued question). Nothing
    /// while an earlier question is on screen (#94): the key acts on the live one, which the
    /// user is not looking at.
    pub fn toggle_pin(&self) {
        let snap = self.hub.snapshot();
        if snap.earlier().is_some() {
            return;
        }
        if snap.pinned {
            self.hub.dispatch(Action::Unpin);
        } else if snap.live.is_some() {
            self.hub.dispatch(Action::Pin);
        }
    }

    /// `r`: regenerate the live question (temperature 0.7, "a different angle"). An earlier
    /// question (#94) is read-only, so while one is on screen the key asks nothing.
    pub fn regenerate(&self) {
        if self.hub.snapshot().earlier().is_some() {
            return;
        }
        let _ = self.commands.send(Command::Regenerate);
    }

    /// Enter (#96): make the last thing the interviewer said (the asking lane's last turn) the
    /// question, for when the detector missed it. The pipeline thread builds the candidate from
    /// its transcript log; it sends nothing while assistance is off, before that lane has
    /// spoken, or when that turn already has its question. A press whose request failed can be
    /// pressed again.
    pub fn ask(&self) {
        let _ = self.commands.send(Command::Ask);
    }

    /// `t` (#109): translate what you last said into `[speak].target` and speak it into
    /// `dmn-voice` in your voice. The pipeline thread takes the turn from its transcript log; it
    /// does nothing while `[speak]` is off or while a press is still in flight, and says
    /// `nothing to speak` before you have said anything. Assistance on or off does not matter.
    pub fn speak(&self) {
        let _ = self.commands.send(Command::Speak);
    }

    /// `l` (#114): switch `dmn-voice` between pass-through (your own voice, live) and translate
    /// mode (every turn you finish, translated and spoken in your cloned voice). The pipeline
    /// thread owns the mode and the queue of turns, so the key only tells it; it does nothing
    /// while `[speak]` is off.
    pub fn toggle_voice(&self) {
        let _ = self.commands.send(Command::Voice);
    }

    /// #115: whether the language picker is open, which gives `↑ ↓`, Enter and Esc to it.
    pub fn picker_open(&self) -> bool {
        self.hub.snapshot().picker.is_some()
    }

    /// #115: a key on the language picker (open it, move the cursor, close it). The reducer
    /// owns the overlay, so the session changes at once; it ignores the key while `[speak]`
    /// is off or no language is listed.
    pub fn picker(&self, op: PickerOp) {
        self.hub.dispatch(Action::Picker { op });
    }

    /// #115: Enter on the picker: the language under the cursor is the target from the next
    /// utterance on. The overlay closes at once; the pipeline thread, which owns the speaker,
    /// dispatches the change and saves it to the config. Nothing while the picker is closed.
    pub fn choose_language(&self) {
        let snap = self.hub.snapshot();
        let Some(index) = snap.picker.map(|p| p.cursor) else {
            return;
        };
        self.hub.dispatch(Action::Picker {
            op: PickerOp::Close,
        });
        if index < snap.languages.len() {
            let _ = self.commands.send(Command::SpeakTarget { index });
        }
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
    /// Nothing while an earlier question is on screen (#94): it is read-only.
    pub fn swap_story(&self) {
        let snap = self.hub.snapshot();
        if snap.earlier().is_some() {
            return;
        }
        let Some(live) = snap.live else {
            return;
        };
        let candidate = live.candidate;
        self.hub.dispatch(Action::SwapStory { candidate });
        if live.stories.len() > 1 {
            let _ = self.commands.send(Command::Swap { candidate });
        }
    }

    /// Ctrl+Shift+A (#79): interview assistance off (transcribe and translate only) or on again. The
    /// session changes at once, so the screen switches layout on the key; the pipeline stops or
    /// resumes detecting from its next segment.
    pub fn toggle_assist(&self) {
        let on = self.hub.snapshot().assist_off;
        self.hub.dispatch(Action::Assist { enabled: on });
        let _ = self.commands.send(Command::Assist { on });
    }

    /// `1`–`6`: open `section` under the Approach of the question on screen (closing the open
    /// one), or close it (#56). The reducer ignores a row that is not on screen; one action per
    /// press, so the session log replays to the same screen.
    pub fn toggle_section(&self, section: Section) {
        self.hub.dispatch(Action::ToggleSection { section });
    }

    /// A left click (#95): toggle the section `at` finds under the pointer in the session as it
    /// is now, as its number key does. `None` (a click on no row) dispatches nothing.
    pub fn click(&self, at: impl FnOnce(&Session) -> Option<Section>) {
        if let Some(section) = at(&self.hub.snapshot()) {
            self.toggle_section(section);
        }
    }

    /// `←` (#94): show the question before the one on screen. It holds the screen as a pin
    /// does: the pipeline goes on answering behind it, and the status line counts what arrives.
    pub fn earlier(&self) {
        self.hub.dispatch(Action::Earlier);
    }

    /// `→` (#94): show the question after the earlier one on screen, which from the newest of
    /// them is the live question again.
    pub fn later(&self) {
        self.hub.dispatch(Action::Later);
    }

    /// `↑ ↓ PgUp PgDn` (#94): scroll the open section to the offset `to` works out from the
    /// session; the caller knows the pane, which decides how many rows the section has. `None`
    /// means the key moves nothing, and then nothing is dispatched.
    pub fn scroll(&self, to: impl FnOnce(&Session) -> Option<u16>) {
        if let Some(offset) = to(&self.hub.snapshot()) {
            self.hub.dispatch(Action::Scroll { offset });
        }
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
    /// Between the bus and everything downstream (#60); `[audio].echo_guard`.
    echo: EchoGuard,
    flushed: bool,
    log: TranscriptLog,
    /// The detector and the LLM (#79): on or off, the transcript above is the same.
    assist: Assistance,
    /// `t` (#109): off until [`Pipeline::attach_speaker`].
    speaking: Speaking,
}

/// Starts the suggester the first time assistance is on (#79).
type Launch = Box<dyn FnOnce() -> Suggester + Send>;

/// Interview assistance (#79): the detector, the candidates it emits and the suggester that
/// answers them. Fed every published segment; off, it ignores them all and asks nothing.
pub struct Assistance {
    on: bool,
    hub: Arc<SessionHub>,
    /// Audio time for [`Action::Timing`].
    audio: AudioClock,
    detecting: Detecting,
    /// `None` under `--no-llm`, and until the first time assistance is on.
    suggester: Option<Suggester>,
    /// How to start the suggester, until it has been.
    launch: Option<Launch>,
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
    /// The turn Enter last sent (#96), as its candidate, its lane and where it ends: a second
    /// press on the same turn sends nothing, unless that candidate's request failed.
    asked: Option<(u64, Source, Duration)>,
    /// The session as the screen has it, to see `not a question` go up (#96).
    session: watch::Receiver<Session>,
    /// The `not a question` notice now up, as its candidate and when this thread first saw it.
    notice: Option<(u64, Instant)>,
    /// #102: the `lookup failed` notice now up, the same way.
    lookup_notice: Option<(u64, Instant)>,
}

impl Pipeline {
    /// Build every stage, then start both files together, the session, and the fast phase.
    pub fn start(cfg: &Config, paths: &Paths, opts: &Options) -> Result<Self, Box<dyn Error>> {
        let name = opts.model.clone().unwrap_or_else(|| cfg.stt.model.clone());
        let model_id = Model::from_name(&name)?;
        let model = model_path(&paths.models_dir, model_id);
        if !model.is_file() {
            return Err(format!(
                "whisper model {} is missing; fix: dmn doctor --fetch {name}",
                model.display()
            )
            .into());
        }
        // #77: anything but English needs a multilingual model; refused here, before any audio
        // moves, with the fix, rather than per utterance.
        let language = language_choice(
            &opts
                .language
                .clone()
                .unwrap_or_else(|| cfg.stt.language.clone()),
        )?;
        language.check(model_id)?;
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
            language: language.clone(),
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
        // #79: with assistance off the LLM is not wanted at all, so a local one is asked to
        // leave instead (Ollama keeps a model 30 minutes after its last request), and whisper
        // gets the whole GPU.
        let assist = opts.assist.unwrap_or(cfg.assist.enabled);
        // #90: `--question-source` over `[detect].question_source`, fixed for the session.
        let asks = opts.question_source.unwrap_or(cfg.detect.question_source);
        if !opts.no_llm {
            prepare_local_models(cfg, assist, stt_cfg.device != DeviceChoice::Cpu);
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
            let lane_cfg = SttConfig {
                language: lane_language(source, &stt_cfg.language, cfg.speak.enabled),
                ..stt_cfg.clone()
            };
            built.push(build_lane(
                source,
                audio,
                ring,
                &model,
                &lane_cfg,
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
        hub.dispatch(Action::Assist { enabled: assist });
        // #90: whose questions this session answers, in the log from the start, so the status
        // line and the listening placeholder are a function of session state alone.
        hub.dispatch(Action::QuestionSource {
            source: session_question_source(asks),
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
        // #12: each candidate goes through the LLM fast phase on its own thread; #54: with the
        // stored sources and job requirements, read-only under `--no-store`. #79: started when
        // assistance is first on, so a run with it off never loads the model.
        let launch = (!opts.no_llm).then(|| {
            let (llm, lookup) = (cfg.llm.clone(), cfg.lookup.clone());
            let (output, policy) = (opts.output, opts.policy);
            let hub = Arc::clone(&hub);
            let audio = audio_clock.clone();
            let context = ContextSource::Db {
                path: paths.db_file.clone(),
                write: !opts.no_store,
            };
            // #59: `[llm]` changes in the config file swap the provider between requests.
            let config_file = paths.config_file.clone();
            Box::new(move || {
                // Started on a later Ctrl+Shift+A, perhaps long after the config was read: an
                // `[llm]` edit made meanwhile applies from the first request (Copilot, PR #80).
                // #102: and so does a `[lookup]` edit.
                let (llm, lookup) = llm_now(&config_file, llm, lookup);
                Suggester::start(
                    &llm,
                    &lookup,
                    output,
                    policy,
                    hub,
                    clock,
                    audio,
                    context,
                    Some(config_file),
                )
            }) as Launch
        });
        let (commands_tx, commands) = mpsc::channel();
        let assist = Assistance::new(
            assist,
            Arc::clone(&hub),
            audio_clock.clone(),
            Detecting::new(
                opts.spec_literal_detector,
                sensitivity(cfg.detect.sensitivity),
                question_source(asks),
            ),
            launch,
        );
        let speaking = Speaking::new(Arc::clone(&hub));
        Ok(Self {
            speaking,
            info: Info {
                model: name,
                threads: stt_cfg.threads,
                stt_device,
                vad: match vad_choice {
                    VadChoice::Energy => "energy",
                    VadChoice::Silero => "silero",
                },
                language: language.label().to_owned(),
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
            assist,
            audio: audio_clock,
            echo: EchoGuard::new(cfg.audio.echo_guard),
            flushed: false,
            log: TranscriptLog::new(),
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

    /// #109: `t` speaks through `speaker` from now on (`dmn run` with `[speak]` on); the
    /// status line offers the key. #114: `dmn-voice` carries `mode` from now, translating
    /// into `target`; the user's own language is `en` when whisper runs English-only. #115:
    /// `languages` is what the picker offers, and a choice is saved to `config_file`.
    pub fn attach_speaker(
        &mut self,
        speaker: Speaker,
        mode: VoiceMode,
        target: String,
        languages: Vec<Language>,
        config_file: Option<PathBuf>,
    ) {
        let own = (self.info.language == "en").then(|| "en".to_owned());
        self.speaking
            .attach(speaker, mode, target, own, languages, config_file);
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
            self.assist.expire_notice(Instant::now());
            self.speaking
                .tick(Instant::now(), &mut self.echo, &self.audio);
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

    /// A segment the guard let through: session, observer, detector (while assistance is on),
    /// log.
    fn publish(&mut self, seg: Segment, observer: &mut dyn Observer) {
        self.hub.dispatch(wire::segment(&seg));
        observer.segment(&seg);
        self.assist.feed(&seg, &self.log, observer);
        self.speaking.heard(&seg);
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
        self.assist.flush(&self.log, observer);
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Speak => self.speaking.press(&self.log),
            Command::Voice => self.speaking.toggle(),
            Command::SpeakTarget { index } => self.speaking.choose(index),
            cmd => self.assist.command(cmd, &self.log),
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
        // #109: a press in flight stops with the run; nothing is dispatched after `Ended`.
        self.speaking.finish();
        // Answer whatever is still queued; the transcript is complete by now. A stopped run
        // cancels instead.
        let (detecting, tally, dropped_candidates) = self.assist.finish(stopped);
        let session = self.recorder.finish(self.clock.now())?;
        Ok(Report {
            stats,
            failures,
            wall,
            tally,
            session,
            detecting,
            log: self.log,
            bus_dropped: self.bus.dropped(),
            dropped_candidates,
            echo_dropped: self.echo.dropped(),
            stopped,
        })
    }
}

impl Assistance {
    /// `launch` starts the suggester (`None` under `--no-llm`): now when `on`, else on the first
    /// [`Command::Assist`] that turns assistance on.
    pub fn new(
        on: bool,
        hub: Arc<SessionHub>,
        audio: AudioClock,
        detecting: Detecting,
        launch: Option<Launch>,
    ) -> Self {
        let (candidates_tx, candidates_rx) = candidate_channel(dmn_detect::DEFAULT_CAPACITY);
        let mut a = Self {
            on: false,
            session: hub.watch(),
            notice: None,
            lookup_notice: None,
            asked: None,
            hub,
            audio,
            detecting,
            suggester: None,
            launch,
            candidates_tx,
            candidates_rx,
            next_candidate: 0,
            sent_ids: VecDeque::new(),
            last_sent: None,
            dropped_candidates: 0,
            known: HashMap::new(),
        };
        a.set(on);
        a
    }

    /// On or off from the next segment. Off forgets the line the detector holds, so nothing
    /// said before the gap completes a question after it; on starts the suggester the first
    /// time. A request already streaming finishes; nothing new is asked while off.
    fn set(&mut self, on: bool) {
        self.on = on;
        if !on {
            self.detecting.pause();
        } else if let Some(launch) = self.launch.take() {
            self.suggester = Some(launch());
        }
    }

    /// One published segment: scored, and any candidate it completes sent on, while on.
    /// `log` is the transcript before `seg`.
    fn feed(&mut self, seg: &Segment, log: &TranscriptLog, observer: &mut dyn Observer) {
        if !self.on {
            return;
        }
        let new = self.detecting.feed(seg);
        self.emit(new, log, observer);
    }

    /// Score the line the detector still holds once the audio has ended.
    fn flush(&mut self, log: &TranscriptLog, observer: &mut dyn Observer) {
        if !self.on {
            return;
        }
        let new = self.detecting.flush();
        self.emit(new, log, observer);
    }

    /// Send the last `new` candidates through the detector's channel into the session and the
    /// fast phase, with the last 60 s of transcript.
    fn emit(&mut self, new: usize, log: &TranscriptLog, observer: &mut dyn Observer) {
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
        while let Ok(candidate) = self.candidates_rx.try_recv() {
            let Some((id, continues)) = self.sent_ids.pop_front() else {
                break;
            };
            self.send(id, candidate, continues, false, log);
        }
    }

    /// Candidate `id` into the session and the fast phase, with the last 60 s of transcript:
    /// the one path every candidate takes, the detector's and (`asked`, #96) Enter's.
    fn send(
        &mut self,
        id: u64,
        candidate: Candidate,
        continues: bool,
        asked: bool,
        log: &TranscriptLog,
    ) {
        self.hub.dispatch(wire::candidate(id, &candidate));
        if asked {
            self.hub.dispatch(Action::Asked { candidate: id });
        }
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
                None => self.answered(&candidate, log),
            };
            s.submit_after(
                id,
                &candidate,
                log.render_window(DEFAULT_WINDOW),
                answered,
                continued.is_some(),
            );
        }
        self.known.insert(id, candidate);
    }

    /// Enter (#96): send the asking lane's last turn ([`last_turn`]) to the fast phase as a
    /// candidate, without the detector; from there it is a candidate like any other. Nothing is
    /// sent before that lane has spoken, or when the turn's last line already has its question:
    /// on screen (`r` asks that one again), queued behind a pin, or on its way.
    ///
    /// A second press on the same turn sends nothing while the first is in flight, once it was
    /// shown, or after the model declined it (the same prompt at temperature 0 gets the same
    /// answer). It sends again when the first one's request failed: a timeout, an unreachable
    /// model or a malformed reply is the tool letting the user down a second time, and the key
    /// must not go dead until the interviewer speaks again. The session says which it was
    /// ([`Session::ask_failed`]).
    fn ask(&mut self, log: &TranscriptLog) {
        if self.suggester.is_none() {
            return;
        }
        let Some(turn) = last_turn(log.finals(), self.detecting.question_source()) else {
            return;
        };
        let snap = self.hub.snapshot();
        if let Some((id, source, end)) = self.asked
            && (source, end) == (turn.source, turn.end)
            && snap.ask_failed != Some(id)
        {
            return;
        }
        let has_question = snap
            .live
            .iter()
            .flat_map(|q| {
                std::iter::once(q.candidate).chain(q.clarifications.iter().map(|c| c.candidate))
            })
            .chain(snap.queued.iter().map(|q| q.candidate))
            .chain(snap.pending)
            .filter_map(|id| self.known.get(&id))
            .any(|c| c.source == turn.source && c.end >= turn.end);
        if has_question {
            return;
        }
        self.next_candidate += 1;
        self.asked = Some((self.next_candidate, turn.source, turn.end));
        self.send(self.next_candidate, turn, false, true, log);
    }

    /// #96: take `not a question` off the status line once it has been up for
    /// [`NOT_A_QUESTION_FOR`]. The reducer has no clock, so the pipeline loop, which comes by
    /// every [`TICK`], says when.
    /// #102: `lookup failed` the same way, for [`LOOKUP_FAILED_FOR`].
    fn expire_notice(&mut self, now: Instant) {
        let (asked, lookup) = {
            let s = self.session.borrow();
            (s.not_a_question, s.lookup_failed)
        };
        let expired = |candidate| Action::NoticeExpired { candidate };
        self.notice = expire(
            &self.hub,
            asked,
            self.notice,
            now,
            NOT_A_QUESTION_FOR,
            expired,
        );
        self.lookup_notice = expire(
            &self.hub,
            lookup,
            self.lookup_notice,
            now,
            LOOKUP_FAILED_FOR,
            expired,
        );
    }

    /// #57, session memory: the question on screen as `next` arrives, and what the candidate
    /// has said since it was asked. The next question's slow phase summarises it.
    fn answered(&self, next: &Candidate, log: &TranscriptLog) -> Option<Answered> {
        let live = self.hub.snapshot().live?;
        let asked = self.known.get(&live.candidate)?.start;
        Some(Answered {
            id: live.candidate,
            question: live.text,
            focus: live.focus,
            at: asked.as_secs_f64(),
            answer: answer_between(log.finals(), asked, next.start),
        })
    }

    /// A key's work on the pipeline thread. `r`, `w` and Enter ask the model nothing while off.
    fn command(&mut self, cmd: Command, log: &TranscriptLog) {
        if let Command::Assist { on } = cmd {
            self.set(on);
            return;
        }
        if !self.on {
            return;
        }
        match cmd {
            Command::Assist { .. }
            | Command::Speak
            | Command::Voice
            | Command::SpeakTarget { .. } => {}
            Command::Ask => self.ask(log),
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
                    log.render_window(DEFAULT_WINDOW),
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
                    log.render_window(DEFAULT_WINDOW),
                    CurrentQuestion {
                        question: q.text,
                        kind,
                    },
                );
            }
        }
    }

    /// End assistance: answer what is still queued (or cancel it, when `stopped`), and hand
    /// back the detector, the suggester's tally, and how many candidates the channel dropped.
    fn finish(mut self, stopped: bool) -> (Detecting, Option<Tally>, u64) {
        let tally = self
            .suggester
            .take()
            .map(|s| if stopped { s.stop() } else { s.finish() });
        (self.detecting, tally, self.dropped_candidates)
    }
}

/// `t` (#109) on the pipeline thread: which press is which, the seconds on the status line,
/// the failure notice's 5 s, and the echo guard's playback windows. The [`Speaker`] does the
/// work on its own thread; the session says whether a press is in flight, so the reducer owns
/// that state and a press while one is in flight does nothing.
///
/// #114: the voice mode too. In pass-through mode (the default) the user's own microphone goes
/// through `dmn-voice` and `t` speaks one turn translated over it, as #109. In translate mode
/// the microphone is shut off from the node and every finalized You turn is queued here and
/// spoken in order, one at a time (the next starts when the session says the last is done); a
/// final that arrives while one waits joins it (the `last_turn` grouping: a sentence the VAD
/// split is one utterance), and `t` queues the last turn again. `l` switches: leaving translate
/// mode drops what waits (not what plays) and opens the gate again.
pub struct Speaking {
    hub: Arc<SessionHub>,
    session: watch::Receiver<Session>,
    /// `None` while `[speak]` is off: `t` does nothing.
    speaker: Option<Speaker>,
    /// Presses so far, which number them.
    presses: u64,
    /// The press in flight and when it was made, for its seconds.
    pressed: Option<(u64, Instant)>,
    /// The failure notice now up, as its press and when this thread first saw it.
    notice: Option<(u64, Instant)>,
    /// #114: what `dmn-voice` carries.
    mode: VoiceMode,
    /// #114: `[speak].target`, for the session's voice state.
    target: String,
    /// #114: the language the user is taken to speak, when the config says.
    own: Option<String>,
    /// #114: turns waiting to be spoken in translate mode, oldest first.
    queue: VecDeque<Utterance>,
    /// #115: the languages the picker offers, in the config's order, with their voices.
    languages: Vec<Language>,
    /// #115: where a chosen language is saved (`[speak].target`); `None` saves nothing.
    config_file: Option<PathBuf>,
}

/// #114: one thing to speak: a turn's text and the session seconds of the You lines in it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Utterance {
    text: String,
    from: Duration,
    to: Duration,
    /// The interviewer spoke after this turn was finalized (#114): a later You final is a new
    /// turn, not more of this one, as #109's `last_turn` grouping ends at the other speaker.
    sealed: bool,
}

impl Speaking {
    pub fn new(hub: Arc<SessionHub>) -> Self {
        Self {
            session: hub.watch(),
            hub,
            speaker: None,
            presses: 0,
            pressed: None,
            notice: None,
            mode: VoiceMode::Passthrough,
            target: String::new(),
            own: None,
            queue: VecDeque::new(),
            languages: Vec::new(),
            config_file: None,
        }
    }

    /// `[speak]` on: `t` speaks through `speaker`; `dmn-voice` carries `mode` from now (#114),
    /// translating into `target`; `own` is the user's own language when known. #115: the
    /// session gets `languages` for the picker; a choice is saved to `config_file`.
    fn attach(
        &mut self,
        speaker: Speaker,
        mode: VoiceMode,
        target: String,
        own: Option<String>,
        languages: Vec<Language>,
        config_file: Option<PathBuf>,
    ) {
        speaker.set_passthrough(mode == VoiceMode::Passthrough);
        self.speaker = Some(speaker);
        self.mode = mode;
        self.target = target;
        self.own = own;
        self.languages = languages;
        self.config_file = config_file;
        self.hub.dispatch(Action::SpeakEnabled { enabled: true });
        self.announce();
        self.hub.dispatch(Action::SpeakLanguages {
            languages: self.languages.iter().map(Language::listed).collect(),
        });
    }

    fn announce(&self) {
        self.hub.dispatch(Action::Voice {
            mode: self.mode,
            target: self.target.clone(),
            own: self.own.clone(),
        });
    }

    /// #115: the picker chose the language at `index`: the speaker takes it from its next
    /// job (one in flight finishes in its own), the session shows it (the tag, the marker row,
    /// the marks on later lines), and it is saved to `[speak].target` so the next run starts
    /// with it. A file that cannot be written is the usual 5 s notice, not a stop: the run
    /// speaks the chosen language either way. The same language again changes nothing.
    fn choose(&mut self, index: usize) {
        let Some(speaker) = &self.speaker else {
            return;
        };
        let Some(language) = self.languages.get(index).cloned() else {
            return;
        };
        if language.code == self.target {
            return;
        }
        speaker.set_language(language.clone());
        self.target = language.code.clone();
        self.hub.dispatch(Action::SpeakTarget {
            target: language.code.clone(),
        });
        let Some(path) = &self.config_file else {
            return;
        };
        match Config::write_speak_target(path, &language.code) {
            Ok(()) => tracing::info!(
                code = %language.code,
                config = %path.display(),
                "speak: language saved as [speak].target"
            ),
            Err(e) => {
                tracing::warn!(error = %e, config = %path.display(), "speak: cannot save the language to the config");
                self.hub.dispatch(Action::SpeakFailed {
                    id: crate::speaker::STARTUP_NOTICE,
                    reason: format!(
                        "{NOT_SAVED}: {}",
                        e.to_string().chars().take(40).collect::<String>()
                    ),
                });
            }
        }
    }

    /// #114: `l`: pass-through ↔ translate. Leaving translate mode drops the turns that wait
    /// (the one playing finishes) and opens the microphone's gate again; entering it shuts the
    /// gate. The session gets the mode (and writes the marker line). Nothing while `[speak]`
    /// is off.
    fn toggle(&mut self) {
        let Some(speaker) = &self.speaker else {
            return;
        };
        self.mode = self.mode.toggled();
        if self.mode == VoiceMode::Passthrough {
            self.queue.clear();
        }
        speaker.set_passthrough(self.mode == VoiceMode::Passthrough);
        self.announce();
    }

    /// One `t`: the You lane's last turn to the speaker. In pass-through mode as #109: nothing
    /// while a press is in flight. In translate mode (#114) the turn is queued behind whatever
    /// is speaking, as a re-run of the last automatic turn. `nothing to speak` before the user
    /// has said anything, either way; nothing while `[speak]` is off.
    fn press(&mut self, log: &TranscriptLog) {
        if self.speaker.is_none() {
            return;
        }
        let in_flight = self.session.borrow().speech.is_some();
        if self.mode == VoiceMode::Passthrough && in_flight {
            return;
        }
        let Some(turn) = last_turn(log.finals(), QuestionSource::You) else {
            self.presses += 1;
            self.hub.dispatch(Action::SpeakFailed {
                id: self.presses,
                reason: NOTHING_TO_SPEAK.to_owned(),
            });
            return;
        };
        let utterance = Utterance {
            text: turn.text,
            from: turn.start,
            to: turn.end,
            sealed: false,
        };
        match self.mode {
            VoiceMode::Passthrough => self.start(utterance),
            VoiceMode::Translate => {
                self.queue.push_back(utterance);
                self.start_next();
            }
        }
    }

    /// #114: a segment the guard let through. In translate mode a final You line with text is
    /// a turn to speak: queued, or joined to the turn already waiting (the VAD split one
    /// sentence; what is said while another plays is one utterance), then started if nothing
    /// is in flight. An interviewer final with text seals the waiting turn: what the user says
    /// after the interviewer spoke is a turn of its own, as `last_turn` groups them (#109).
    fn heard(&mut self, seg: &Segment) {
        if self.speaker.is_none()
            || self.mode != VoiceMode::Translate
            || !seg.is_final
            || seg.text.trim().is_empty()
        {
            return;
        }
        match seg.source {
            Source::You => {}
            Source::Interviewer => {
                if let Some(waiting) = self.queue.back_mut() {
                    waiting.sealed = true;
                }
                return;
            }
            Source::Unknown => return,
        }
        match self.queue.back_mut() {
            Some(waiting) if !waiting.sealed => {
                waiting.text.push(' ');
                waiting.text.push_str(seg.text.trim());
                waiting.to = waiting.to.max(seg.end);
            }
            _ => self.queue.push_back(Utterance {
                text: seg.text.trim().to_owned(),
                from: seg.start,
                to: seg.end,
                sealed: false,
            }),
        }
        self.start_next();
    }

    /// #114: start the oldest waiting turn when nothing is in flight.
    fn start_next(&mut self) {
        if self.session.borrow().speech.is_some() {
            return;
        }
        if let Some(next) = self.queue.pop_front() {
            self.start(next);
        }
    }

    /// Hand one utterance to the speaker as the next press: the session shows it in flight,
    /// knows which lines it speaks (#114), and this thread counts its seconds.
    fn start(&mut self, utterance: Utterance) {
        let Some(speaker) = &self.speaker else {
            return;
        };
        self.presses += 1;
        let id = self.presses;
        self.hub.dispatch(Action::SpeakStarted { id });
        self.hub.dispatch(Action::SpeakTurn {
            id,
            from: utterance.from.as_secs_f64(),
            to: utterance.to.as_secs_f64(),
        });
        self.pressed = Some((id, Instant::now()));
        if !speaker.speak(id, utterance.text) {
            self.hub.dispatch(Action::SpeakFailed {
                id,
                reason: "speak failed: the speaker stopped".to_owned(),
            });
        }
    }

    /// Every loop: the press's seconds, the notice's expiry, the playback windows into
    /// `echo` on `audio`'s clock, and (#114) the next waiting turn once the last is done.
    fn tick(&mut self, now: Instant, echo: &mut EchoGuard, audio: &AudioClock) {
        let Some(speaker) = &self.speaker else {
            return;
        };
        for window in speaker.windows() {
            let at = |t: Instant| Duration::from_secs_f64(audio.at(t).max(0.0));
            match window {
                Window::From(t) => echo.speaking_from(at(t)),
                Window::Until(t) => echo.speaking_until(at(t) + SPEAK_TAIL),
            }
        }
        let (speech, failed) = {
            let s = self.session.borrow();
            (
                s.speech.as_ref().map(|sp| (sp.id, sp.secs)),
                s.speak_failed.as_ref().map(|n| n.id),
            )
        };
        if let (Some((id, shown)), Some((pressed, at))) = (speech, self.pressed)
            && id == pressed
        {
            let secs =
                u32::try_from(now.saturating_duration_since(at).as_secs()).unwrap_or(u32::MAX);
            if secs > shown {
                self.hub.dispatch(Action::SpeakElapsed { id, secs });
            }
        }
        self.notice = expire(
            &self.hub,
            failed,
            self.notice,
            now,
            SPEAK_FAILED_FOR,
            |id| Action::SpeakNoticeExpired { id },
        );
        self.start_next();
    }

    fn finish(&mut self) {
        self.queue.clear();
        if let Some(speaker) = self.speaker.take() {
            speaker.stop();
        }
    }
}

/// #109: what the status line says when `t` is pressed before you have said anything.
pub const NOTHING_TO_SPEAK: &str = "nothing to speak";

/// #115: what the status line says for 5 s when the picker's choice could not be written to
/// the config file (the run speaks the chosen language all the same).
pub const NOT_SAVED: &str = "language not saved";

/// #109: the language whisper runs one lane in. With `[speak]` on, the You lane keeps the
/// user's own words: under `[stt].language = "auto"` or a code (#77), whisper's translate task
/// would turn them into English before `t` could translate them into the interview's language,
/// so that lane detects its language and transcribes without translating
/// ([`dmn_stt::Language::Detect`]). The Interviewer lane, and both lanes with `[speak]` off or
/// `language = "en"`, are as configured.
pub fn lane_language(
    source: Source,
    language: &dmn_stt::Language,
    speak: bool,
) -> dmn_stt::Language {
    if speak && source == Source::You && language.translates() {
        dmn_stt::Language::Detect
    } else {
        language.clone()
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

/// The local models before whisper loads (#61, #79, #113). The answer model (`[llm].model`) is
/// wanted with assistance on, and with it off only when `t` translates on it too (#109:
/// `[speak]` on with no `[speak].model` of its own); not wanted, a local one is asked to leave
/// (Ollama keeps a model 30 minutes after its last request), so a translation-only session
/// holds just the small translator and whisper and the TTS server get the rest of the GPU.
/// Ctrl+Shift+A loads it later, as the suggester starts. With whisper on the GPU, what is wanted
/// is loaded first, so it sizes itself against a GPU whisper has not claimed yet; the speaker
/// warms the translator itself otherwise.
fn prepare_local_models(cfg: &Config, assist: bool, gpu_whisper: bool) {
    let translator = cfg.speak.enabled.then(|| cfg.translator_llm());
    let llm_wanted = assist
        || translator
            .as_ref()
            .is_some_and(|t| t.model == cfg.llm.model);
    if !llm_wanted {
        unload_local_llm(&cfg.llm);
    } else if gpu_whisper {
        warm_local_llm(&cfg.llm);
    }
    if let Some(t) = translator.filter(|t| t.model != cfg.llm.model)
        && gpu_whisper
    {
        warm_local_llm(&t);
    }
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

/// `[llm]` as the config file says now, for a suggester started after the run began (#79).
/// The file wins only when it exists and parses; otherwise the settings the run started with
/// stand, so a missing file (the tests', or one deleted mid-run) never swaps in the defaults, and
/// a half-written edit is ignored rather than fatal.
fn llm_now(
    config_file: &Path,
    llm: dmn_core::config::Llm,
    lookup: dmn_core::config::Lookup,
) -> (dmn_core::config::Llm, dmn_core::config::Lookup) {
    if !config_file.is_file() {
        return (llm, lookup);
    }
    match Config::load(config_file) {
        Ok(cfg) => (cfg.llm, cfg.lookup),
        Err(e) => {
            tracing::warn!(error = %e, "config unreadable when assistance started; keeping [llm] and [lookup] from start-up");
            (llm, lookup)
        }
    }
}

/// #79: ask a local Ollama to drop `[llm].model` from memory, for a run that starts with
/// assistance off, before whisper loads. Ollama on another machine, or any other provider, holds
/// nothing here and is left alone. A failure (Ollama not running, say) is a warning: there is
/// then nothing loaded to free.
fn unload_local_llm(llm: &dmn_core::config::Llm) {
    if llm.provider != dmn_core::config::Provider::Ollama || llm.leaves_machine() {
        return;
    }
    let unloaded = dmn_llm::OllamaProvider::new(llm.endpoint(), llm.model.clone())
        .map_err(|e| e.to_string())
        .and_then(|p| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(p.unload())
                .map_err(|e| e.to_string())
        });
    match unloaded {
        Ok(()) => tracing::info!(model = %llm.model, "assistance off: LLM unloaded before whisper"),
        Err(e) => tracing::warn!(error = %e, "assistance off: LLM unload failed"),
    }
}

/// `[stt].device` in `dmn-stt`'s terms (`dmn-stt` does not depend on `dmn-core`).
/// `[stt].language` / `--language` as `dmn-stt` understands it (#77): the config checks the
/// shape, whisper's own list decides whether it knows the code.
pub fn language_choice(
    l: &dmn_core::config::SttLanguage,
) -> Result<dmn_stt::Language, dmn_stt::SttError> {
    dmn_stt::Language::parse(l.label())
}

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
            language: None,
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
            language: None,
        }
    }

    fn detecting() -> Detecting {
        detecting_for(QuestionSource::Interviewer)
    }

    fn detecting_for(source: QuestionSource) -> Detecting {
        Detecting::new(
            false,
            sensitivity(dmn_core::config::Sensitivity::Balanced),
            source,
        )
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

    /// #90: a question on the mic lane, and the two halves of one split at a pause.
    const MINE: &str = "How does a token bucket differ from a sliding window counter?";
    const MINE_REST: &str = "And which one would you pick for a public API?";

    fn sources(d: &Detecting) -> Vec<Source> {
        d.found().iter().map(|f| f.candidate.source).collect()
    }

    #[test]
    fn you_only_scores_the_mic_lane_and_ignores_the_interviewer() {
        let mut d = detecting_for(QuestionSource::You);
        assert_eq!(d.feed(&final_at(Source::Interviewer, 0, 5_000, FIRST)), 0);
        assert_eq!(d.feed(&final_at(Source::You, 6_000, 9_000, MINE)), 1);
        assert_eq!(
            d.feed(&final_at(
                Source::You,
                12_000,
                15_000,
                "First I'd clarify the scope of the limiter."
            )),
            0,
            "a statement on the enabled lane is not a question"
        );
        d.flush();
        assert_eq!(sources(&d), [Source::You]);
        assert_eq!(d.found()[0].candidate.text, MINE);
    }

    #[test]
    fn the_default_source_still_ignores_the_mic_lane() {
        let mut d = detecting();
        assert_eq!(d.feed(&final_at(Source::You, 0, 3_000, MINE)), 0);
        assert_eq!(
            d.feed(&final_at(Source::Interviewer, 4_000, 9_000, FIRST)),
            1
        );
        d.flush();
        assert_eq!(sources(&d), [Source::Interviewer]);
    }

    #[test]
    fn both_scores_each_lane_and_a_chain_never_crosses_lanes() {
        let mut d = detecting_for(QuestionSource::Both);
        // An Interviewer question, then a You question 1 s later: inside CONTINUE_GAP, but on
        // the other lane, so it is a question of its own.
        assert_eq!(d.feed(&final_at(Source::Interviewer, 0, 5_000, FIRST)), 1);
        assert_eq!(d.feed(&final_at(Source::You, 6_000, 9_000, MINE)), 1);
        assert_eq!(sources(&d), [Source::Interviewer, Source::You]);
        assert!(!d.found()[1].continues, "two candidates, not one continued");
        assert_eq!(d.found()[1].candidate.text, MINE);
        assert_eq!(d.found()[1].candidate.start, Duration::from_millis(6_000));
        // The Interviewer's next line does not continue the first question either: You spoke.
        assert_eq!(
            d.feed(&final_at(Source::Interviewer, 9_500, 12_000, SECOND)),
            1
        );
        assert!(!d.found()[2].continues);
    }

    #[test]
    fn a_you_question_split_at_a_pause_continues_as_one_candidate() {
        for source in [QuestionSource::You, QuestionSource::Both] {
            let mut d = detecting_for(source);
            assert_eq!(d.feed(&final_at(Source::You, 10_000, 14_000, MINE)), 1);
            assert_eq!(d.feed(&final_at(Source::You, 14_600, 17_000, MINE_REST)), 1);
            let second = &d.found()[1];
            assert!(second.continues, "{source:?}: #63's rule on the You lane");
            assert_eq!(second.candidate.source, Source::You);
            assert_eq!(second.candidate.start, Duration::from_millis(10_000));
            assert_eq!(second.candidate.text, format!("{MINE} {MINE_REST}"));
        }
    }

    #[test]
    fn the_interviewer_speaking_in_the_gap_ends_a_you_question() {
        let mut d = detecting_for(QuestionSource::You);
        d.feed(&final_at(Source::You, 10_000, 14_000, MINE));
        d.feed(&final_at(Source::Interviewer, 14_100, 14_500, "Sure."));
        d.feed(&final_at(Source::You, 14_600, 17_000, MINE_REST));
        assert!(
            !d.found()[1].continues,
            "the other lane spoke: a new question"
        );
    }

    #[test]
    fn a_you_line_that_needs_its_follower_is_scored_once_the_interviewer_answers() {
        // Strict: `?` alone (2 points, no Interviewer cue) is under the bar of 3 until the
        // other lane follows it.
        let mut d = Detecting::new(
            false,
            sensitivity(dmn_core::config::Sensitivity::Strict),
            QuestionSource::You,
        );
        assert_eq!(d.feed(&final_at(Source::You, 0, 1_000, "The p99?")), 0);
        assert_eq!(
            d.feed(&final_at(
                Source::Interviewer,
                1_200,
                3_000,
                "About two seconds."
            )),
            1
        );
        assert!(d.found()[0].late);
        assert_eq!(d.found()[0].candidate.source, Source::You);
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
            Detecting::new(false, sensitivity(s), QuestionSource::Interviewer)
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
            language: None,
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
                language: None,
                partials: Partials::Auto,
                spec_literal_detector: false,
                question_source: None,
                assist: None,
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

    // --- Interview assistance on and off (#79) ----------------------------------------------

    #[test]
    fn controls_assist_flips_the_session_and_tells_the_pipeline() {
        let (controls, rx) = Controls::for_test();
        let hub = Arc::clone(controls.hub());
        assert!(!hub.snapshot().assist_off, "on by default");
        controls.toggle_assist();
        assert!(hub.snapshot().assist_off);
        assert_eq!(rx.try_recv(), Ok(Command::Assist { on: false }));
        controls.toggle_assist();
        assert!(!hub.snapshot().assist_off);
        assert_eq!(rx.try_recv(), Ok(Command::Assist { on: true }));
    }

    /// Assistance over a mock model that records every request, and whether the suggester was
    /// ever started.
    fn assistance(on: bool) -> (Assistance, dmn_llm::MockProvider, Arc<AtomicBool>) {
        assistance_for(on, QuestionSource::Interviewer)
    }

    /// [`assistance`] with `[detect].question_source` set (#90).
    fn assistance_for(
        on: bool,
        source: QuestionSource,
    ) -> (Assistance, dmn_llm::MockProvider, Arc<AtomicBool>) {
        let hub = Arc::new(SessionHub::default());
        let mock = dmn_llm::MockProvider::new(vec![dmn_llm::mock::Step::Error("mock".into())]);
        let launched = Arc::new(AtomicBool::new(false));
        let launch = {
            let (hub, mock, launched) = (Arc::clone(&hub), mock.clone(), Arc::clone(&launched));
            Box::new(move || {
                launched.store(true, Ordering::Release);
                Suggester::start_with(mock, Policy::AnswerAll, hub)
            }) as Launch
        };
        let a = Assistance::new(
            on,
            hub,
            AudioClock::new(Instant::now(), 1.0),
            detecting_for(source),
            Some(launch),
        );
        (a, mock, launched)
    }

    /// Publish `seg` into `a` as the pipeline does: assistance first, then the log.
    fn publish(a: &mut Assistance, log: &mut TranscriptLog, seg: Segment) {
        a.feed(&seg, log, &mut Quiet);
        log.apply(seg);
    }

    const LATER: &str = "How would you shard the database behind the limiter?";

    #[test]
    fn with_assistance_on_a_question_is_detected_and_answered_as_before() {
        let (mut a, mock, launched) = assistance(true);
        let mut log = TranscriptLog::new();
        assert!(
            launched.load(Ordering::Acquire),
            "on at start: the suggester starts at once"
        );
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        a.flush(&log, &mut Quiet);
        let (d, tally, _) = a.finish(false);
        assert_eq!(d.found().len(), 1);
        assert!(tally.is_some());
        assert_eq!(mock.requests().len(), 1, "one fast-phase request");
    }

    #[test]
    fn with_assistance_off_nothing_is_detected_and_the_model_is_never_asked() {
        let (mut a, mock, launched) = assistance(false);
        let hub = Arc::clone(&a.hub);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        publish(
            &mut a,
            &mut log,
            final_at(
                Source::You,
                27_000,
                30_000,
                "I'd start with a token bucket.",
            ),
        );
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 31_000, 36_000, LATER),
        );
        a.flush(&log, &mut Quiet);
        a.command(Command::Regenerate, &log);
        assert_eq!(log.finals().len(), 3, "the transcript itself is untouched");
        assert!(
            !launched.load(Ordering::Acquire),
            "no suggester, so no warm-up either"
        );
        assert_eq!(
            hub.snapshot().counts.candidates,
            0,
            "no candidate reached the session"
        );
        let (d, tally, _) = a.finish(false);
        assert!(d.found().is_empty(), "no detector candidates");
        assert!(tally.is_none());
        assert!(mock.requests().is_empty(), "no provider request");
    }

    #[test]
    fn assistance_on_again_resumes_detection_with_the_next_question() {
        let (mut a, mock, launched) = assistance(false);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        a.command(Command::Assist { on: true }, &log);
        assert!(
            launched.load(Ordering::Acquire),
            "the first Ctrl+Shift+A starts the suggester"
        );
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 40_000, 45_000, LATER),
        );
        a.flush(&log, &mut Quiet);
        let (d, _, _) = a.finish(false);
        let found: Vec<&str> = d
            .found()
            .iter()
            .map(|f| f.candidate.text.as_str())
            .collect();
        assert_eq!(
            found,
            [LATER],
            "the question asked while off is not answered retroactively"
        );
        assert_eq!(mock.requests().len(), 1);
    }

    #[test]
    fn you_only_answers_the_mic_question_and_never_asks_about_the_interviewers() {
        let (mut a, mock, _) = assistance_for(true, QuestionSource::You);
        let hub = Arc::clone(&a.hub);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        publish(
            &mut a,
            &mut log,
            final_at(Source::You, 27_000, 30_000, MINE),
        );
        a.flush(&log, &mut Quiet);
        // Labels and the transcript are the source's business not at all.
        let lanes: Vec<Source> = log.finals().iter().map(|s| s.source).collect();
        assert_eq!(lanes, [Source::Interviewer, Source::You]);
        assert_eq!(hub.snapshot().counts.candidates, 1);
        let (d, _, _) = a.finish(false);
        assert_eq!(sources(&d), [Source::You]);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one request, for the mic question");
        let user = &requests[0].messages[1].content;
        assert!(
            user.contains(&format!("## You just said\n{MINE}")),
            "{user}"
        );
        assert!(!user.contains("## Interviewer just said"), "{user}");
    }

    #[test]
    fn with_assistance_off_no_source_detects_or_asks_anything() {
        for source in [QuestionSource::You, QuestionSource::Both] {
            let (mut a, mock, launched) = assistance_for(false, source);
            let hub = Arc::clone(&a.hub);
            let mut log = TranscriptLog::new();
            publish(
                &mut a,
                &mut log,
                final_at(Source::Interviewer, 20_000, 26_000, FIRST),
            );
            publish(
                &mut a,
                &mut log,
                final_at(Source::You, 27_000, 30_000, MINE),
            );
            a.flush(&log, &mut Quiet);
            assert_eq!(
                log.finals().len(),
                2,
                "{source:?}: the transcript is untouched"
            );
            assert!(
                !launched.load(Ordering::Acquire),
                "{source:?}: no suggester"
            );
            assert_eq!(hub.snapshot().counts.candidates, 0, "{source:?}");
            // On again: the next eligible question, and only that one.
            a.command(Command::Assist { on: true }, &log);
            publish(
                &mut a,
                &mut log,
                final_at(Source::You, 40_000, 44_000, MINE_REST),
            );
            a.flush(&log, &mut Quiet);
            let (d, _, _) = a.finish(false);
            let found: Vec<&str> = d
                .found()
                .iter()
                .map(|f| f.candidate.text.as_str())
                .collect();
            assert_eq!(found, [MINE_REST], "{source:?}");
            assert_eq!(mock.requests().len(), 1, "{source:?}");
        }
    }

    #[test]
    fn in_both_an_echoed_interviewer_question_is_one_candidate_not_two() {
        // No headphones: the mic hears the interviewer's question a moment later. The echo
        // guard sits above assistance, so the copy is dropped before the detector sees it.
        let (mut a, mock, _) = assistance_for(true, QuestionSource::Both);
        let mut log = TranscriptLog::new();
        let mut echo = EchoGuard::new(true);
        let now = Duration::from_secs(27);
        let mut verdicts = echo.observe(final_at(Source::Interviewer, 20_000, 26_000, FIRST), now);
        verdicts.extend(echo.observe(final_at(Source::You, 20_150, 26_200, FIRST), now));
        verdicts.extend(echo.finish());
        for verdict in verdicts {
            let seg = match verdict {
                Verdict::Publish(seg) => seg,
                Verdict::Drop(seg) => retraction(seg),
            };
            publish(&mut a, &mut log, seg);
        }
        a.flush(&log, &mut Quiet);
        assert_eq!(echo.dropped(), 1, "the mic copy");
        let (d, _, _) = a.finish(false);
        assert_eq!(sources(&d), [Source::Interviewer]);
        assert_eq!(mock.requests().len(), 1);
    }

    /// #90: `continuous/` under `you`. Only the candidate's own lines are scored, among them the
    /// two questions the script gives the candidate (turns.json: 25.2 s and 432.4 s), and the
    /// transcript is what the default source produces. Run with `just probe` on a dev machine.
    #[test]
    #[ignore = "needs the fixture WAVs (just fixtures-fetch) and the whisper model (dmn doctor --fetch small.en)"]
    fn the_continuous_fixture_under_you_scores_only_the_candidates_lane() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/continuous");
        let run = |question_source| {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths {
                models_dir: Paths::from_xdg().unwrap().models_dir,
                ..Paths::under(dir.path())
            };
            let mut pipeline = Pipeline::start(
                &Config::default(),
                &paths,
                &Options {
                    input: Input::Files {
                        interviewer: fixture.join("interviewer.wav"),
                        candidate: fixture.join("candidate.wav"),
                    },
                    speed: 8.0,
                    model: None,
                    threads: None,
                    vad_close_ms: None,
                    vad: None,
                    device: Some(dmn_core::config::SttDevice::Cpu),
                    language: None,
                    partials: Partials::Off,
                    spec_literal_detector: false,
                    question_source,
                    assist: Some(true),
                    no_llm: true,
                    no_store: true,
                    output: Output::Quiet,
                    policy: Policy::AnswerAll,
                    coding_solutions: None,
                },
            )
            .unwrap();
            pipeline.run(&mut Quiet, false);
            pipeline.finish(&mut Quiet).unwrap()
        };
        let you = run(Some(dmn_core::config::QuestionSource::You));
        let found = you.detecting.found();
        assert!(!found.is_empty());
        assert!(found.iter().all(|f| f.candidate.source == Source::You));
        for asked in ["one global limit", "on-call rotation"] {
            assert!(
                found.iter().any(|f| f.candidate.line.contains(asked)),
                "the candidate's question about the {asked}"
            );
        }
        let default = run(None);
        assert!(
            default
                .detecting
                .found()
                .iter()
                .all(|f| f.candidate.source == Source::Interviewer)
        );
        let lines = |r: &Report| -> Vec<(Source, String)> {
            r.log
                .finals()
                .iter()
                .map(|s| (s.source, s.text.clone()))
                .collect()
        };
        assert_eq!(
            lines(&you),
            lines(&default),
            "the transcript and its labels do not depend on the source"
        );
    }

    /// #79 with #77: the Spanish fixture replayed with `language = "auto"` and assistance off.
    /// The translated transcript flows as it does with assistance on; nothing is detected and
    /// the LLM is never started. Run with `just probe` on a dev machine.
    #[test]
    #[ignore = "needs the es fixture WAVs (just fixtures-fetch) and the multilingual model (dmn doctor --fetch small)"]
    fn the_spanish_fixture_is_translated_with_assistance_off_and_nothing_is_asked() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/es");
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            models_dir: Paths::from_xdg().unwrap().models_dir,
            ..Paths::under(dir.path())
        };
        // A local Ollama no one listens on: the start-up unload fails quietly instead of
        // unloading the dev machine's model, and any request would fail loudly.
        let mut cfg = Config::default();
        cfg.llm.base_url = "http://127.0.0.1:9".into();
        let mut pipeline = Pipeline::start(
            &cfg,
            &paths,
            &Options {
                input: Input::Files {
                    interviewer: fixture.join("interviewer.wav"),
                    candidate: fixture.join("candidate.wav"),
                },
                // 10:45 of audio in about a minute; whisper small keeps up on 8 threads.
                speed: 10.0,
                model: Some("small".into()),
                threads: None,
                vad_close_ms: None,
                vad: None,
                device: Some(dmn_core::config::SttDevice::Cpu),
                language: Some(dmn_core::config::SttLanguage::Auto),
                partials: Partials::Off,
                spec_literal_detector: false,
                question_source: None,
                assist: Some(false),
                no_llm: false,
                no_store: true,
                output: Output::Quiet,
                policy: Policy::AnswerAll,
                coding_solutions: None,
            },
        )
        .unwrap();
        let hub = pipeline.hub();
        pipeline.run(&mut Quiet, false);
        let report = pipeline.finish(&mut Quiet).unwrap();
        let finals: Vec<&Segment> = report
            .log
            .finals()
            .iter()
            .filter(|s| !s.text.trim().is_empty())
            .collect();
        let es = finals
            .iter()
            .filter(|s| s.language.as_deref() == Some("es"))
            .count();
        assert!(finals.len() >= 20, "only {} finals", finals.len());
        assert!(
            es * 10 >= finals.len() * 9,
            "{es} of {} detected as es",
            finals.len()
        );
        assert!(
            report.detecting.found().is_empty(),
            "no detector candidates"
        );
        assert!(report.tally.is_none(), "the suggester never started");
        let snap = hub.snapshot();
        assert!(snap.assist_off);
        assert_eq!(snap.counts.candidates, 0);
        assert!(snap.live.is_none());
        assert_eq!(snap.status.language, "es");
    }

    #[test]
    fn a_suggester_started_later_reads_llm_from_the_config_file_as_it_is_now() {
        // Copilot on PR #80: assistance off at start, `[llm]` edited, then Ctrl+Shift+A.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let started_with = (Config::default().llm, Config::default().lookup);
        let now = |started: &(dmn_core::config::Llm, dmn_core::config::Lookup)| {
            llm_now(&file, started.0.clone(), started.1.clone())
        };
        assert_eq!(
            now(&started_with),
            started_with,
            "no file: the start-up settings, not the defaults"
        );
        std::fs::write(
            &file,
            "[llm]\nmodel = \"llama3.1:8b\"\n[lookup]\nenabled = true\n",
        )
        .unwrap();
        let (llm, lookup) = now(&started_with);
        assert_eq!(llm.model, "llama3.1:8b");
        assert!(lookup.enabled, "#102: `[lookup]` as it is now too");
        std::fs::write(&file, "[llm]\nmodel = ").unwrap();
        assert_eq!(
            now(&started_with),
            started_with,
            "a half-written edit keeps the start-up settings"
        );
    }

    #[test]
    fn assistance_off_forgets_the_question_being_asked() {
        // FIRST opens a question; its second half arrives after assistance went off and on
        // again. It is a question of its own, not a continuation of one the screen never showed.
        let (mut a, _, _) = assistance(true);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        a.command(Command::Assist { on: false }, &log);
        a.command(Command::Assist { on: true }, &log);
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 26_600, 31_000, SECOND),
        );
        let (d, _, _) = a.finish(false);
        assert_eq!(d.found().len(), 2);
        assert!(
            !d.found()[1].continues,
            "the chain was forgotten when assistance went off"
        );
    }

    // --- The ask key (#96) -------------------------------------------------------------------

    /// What the interviewer said in the mock interview #96 came from (session 24), after the
    /// candidate's answer ended at about 197 s, as `(start ms, end ms, text)`. The question is
    /// the end of the last line, and the detector made no candidate from any of them.
    const SESSION_24: [(u64, u64, &str); 5] = [
        (
            197_600,
            200_700,
            "Great, let me take a quick look and respond.",
        ),
        (
            202_100,
            206_300,
            "Good answer. You covered the core piece as well. Four areas I'd probe.",
        ),
        (
            206_800,
            210_300,
            "Quota reconciliation since output tokens aren't known up front.",
        ),
        (
            210_900,
            220_000,
            "Tool call like dempotency and safety, especially with retries and side effects. \
             Streaming failure modes like client disconnects or provider stalls and...",
        ),
        (
            220_500,
            232_500,
            "Control plane versus data plane separation for prompts, policies, and secrets. Let's \
             zoom into the quota problem. How would you reserve and then reconcile usage \
             correctly under heavy concurrent traffic across regions?",
        ),
    ];

    /// The question in [`SESSION_24`]'s last line, as the fast phase extracts it.
    const RESERVE: &str = "How would you reserve and then reconcile usage correctly under heavy \
                           concurrent traffic across regions?";

    /// A fast-phase reply that shows `question` with a three-line Approach.
    fn reply(question: &str) -> serde_json::Value {
        serde_json::json!({
            "is_question": true,
            "kind": "system_design",
            "question": question,
            "focus": "reservation, reconciliation, consistency across regions",
            "approach": [
                "Reserve an estimate against the tenant's regional quota up front.",
                "Reconcile to the real usage when the stream ends.",
                "Expire reservations that never settle."
            ]
        })
    }

    /// Assistance on over `hub`, answered by `mock` under `dmn run`'s policy: a newer unpinned
    /// question supersedes the one in flight.
    fn asking(
        hub: &Arc<SessionHub>,
        mock: &dmn_llm::MockProvider,
        source: QuestionSource,
    ) -> Assistance {
        let launch = {
            let (hub, mock) = (Arc::clone(hub), mock.clone());
            Box::new(move || Suggester::start_with(mock, Policy::Supersede, hub)) as Launch
        };
        Assistance::new(
            true,
            Arc::clone(hub),
            AudioClock::new(Instant::now(), 1.0),
            detecting_for(source),
            Some(launch),
        )
    }

    /// `seg` into the session and the log with no candidate made from it. Whether the detector
    /// would have found the line is its own tests' business (#93); the key is for when it did
    /// not.
    fn hear(hub: &SessionHub, log: &mut TranscriptLog, seg: Segment) {
        hub.dispatch(wire::segment(&seg));
        log.apply(seg);
    }

    fn hear_session_24(hub: &SessionHub, log: &mut TranscriptLog) {
        for (start, end, text) in SESSION_24 {
            hear(hub, log, final_at(Source::Interviewer, start, end, text));
        }
    }

    fn log_of(finals: &[Segment]) -> TranscriptLog {
        let mut log = TranscriptLog::new();
        for seg in finals {
            log.apply(seg.clone());
        }
        log
    }

    /// Wait, up to 10 s, until the session satisfies `done`.
    fn wait_until(hub: &SessionHub, what: &str, done: impl Fn(&Session) -> bool) -> Session {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let s = hub.snapshot();
            if done(&s) {
                return s;
            }
            assert!(Instant::now() < until, "never: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The live question's fast phase has finished.
    fn answered(s: &Session) -> bool {
        s.live
            .as_ref()
            .is_some_and(|q| q.progress == dmn_session::Progress::Done)
    }

    /// The TUI's screen for `session` at 100 × 30.
    fn screen(session: &Session) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| crate::tui::draw(f, session)).unwrap();
        let buf = t.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// The fast-phase requests `mock` received, as their user messages.
    fn asked_of(mock: &dmn_llm::MockProvider) -> Vec<String> {
        mock.requests()
            .iter()
            .map(|r| r.messages[1].content.clone())
            .collect()
    }

    const THREE: [(u64, u64, &str); 3] = [
        (5_000, 9_000, "Okay, that covers the happy path."),
        (9_600, 14_000, "Now say one region loses its Redis."),
        (
            14_500,
            18_000,
            "What does the limiter do for those callers?",
        ),
    ];

    fn three_interviewer_finals() -> Vec<Segment> {
        let mut finals = vec![final_at(
            Source::You,
            0,
            4_000,
            "I'd start with a token bucket.",
        )];
        finals.extend(
            THREE.map(|(start, end, text)| final_at(Source::Interviewer, start, end, text)),
        );
        finals
    }

    #[test]
    fn a_turn_of_three_finals_is_one_candidate_in_spoken_order() {
        let log = log_of(&three_interviewer_finals());
        let turn = last_turn(log.finals(), QuestionSource::Interviewer).unwrap();
        assert_eq!(
            turn.text,
            "Okay, that covers the happy path. Now say one region loses its Redis. What does \
             the limiter do for those callers?"
        );
        assert_eq!(turn.line, turn.text);
        assert_eq!(turn.source, Source::Interviewer);
        assert_eq!(
            (turn.start, turn.end),
            (Duration::from_millis(5_000), Duration::from_millis(18_000))
        );
        assert_eq!(
            (turn.score, turn.cues.len()),
            (0, 0),
            "the detector never scored it"
        );

        // What the candidate says after the turn is not between its lines.
        let mut answering = three_interviewer_finals();
        answering.push(final_at(Source::You, 19_000, 24_000, "Good question."));
        let log = log_of(&answering);
        let later = last_turn(log.finals(), QuestionSource::Interviewer).unwrap();
        assert_eq!(later.text, turn.text);
    }

    #[test]
    fn a_you_final_between_the_lines_cuts_the_turn_there() {
        let mut finals = three_interviewer_finals();
        finals.push(final_at(Source::You, 9_100, 9_500, "Right."));
        let log = log_of(&finals);
        let turn = last_turn(log.finals(), QuestionSource::Interviewer).unwrap();
        assert_eq!(
            turn.text,
            "Now say one region loses its Redis. What does the limiter do for those callers?"
        );
        assert_eq!(turn.start, Duration::from_millis(9_600));

        // A "mm-hm" while the interviewer is still speaking is inside a line, not between two.
        let mut finals = three_interviewer_finals();
        finals.push(final_at(Source::You, 10_000, 10_400, "Mm-hm."));
        let log = log_of(&finals);
        let turn = last_turn(log.finals(), QuestionSource::Interviewer).unwrap();
        assert_eq!(turn.start, Duration::from_millis(5_000), "{}", turn.text);
    }

    #[test]
    fn an_empty_final_neither_cuts_the_turn_nor_is_part_of_it() {
        // #63's rule for a split question, here: an utterance whisper heard nothing in is no
        // speech. The transcript log keeps no empty final, so the slice is handed over as is.
        let whole = "Okay, that covers the happy path. Now say one region loses its Redis. What \
                     does the limiter do for those callers?";
        for nothing in ["", "  "] {
            let mut finals = three_interviewer_finals();
            // In the gap between the first and the second line, where "Right." cuts the turn.
            finals.insert(2, final_at(Source::You, 9_100, 9_500, nothing));
            let turn = last_turn(&finals, QuestionSource::Interviewer).unwrap();
            assert_eq!(turn.text, whole, "{nothing:?}");
            assert_eq!(turn.start, Duration::from_millis(5_000));

            // On the asking lane it is not the turn's last line, nor a line of it.
            finals.push(final_at(Source::Interviewer, 18_400, 18_900, nothing));
            let turn = last_turn(&finals, QuestionSource::Interviewer).unwrap();
            assert_eq!(turn.text, whole, "{nothing:?}");
            assert_eq!(turn.end, Duration::from_millis(18_000));
            // Alone, it is not a turn at all: that lane has said nothing.
            assert!(last_turn(&finals[2..3], QuestionSource::You).is_none());
        }
        // The same place with something said in it still cuts the turn.
        let mut finals = three_interviewer_finals();
        finals.insert(2, final_at(Source::You, 9_100, 9_500, "Right."));
        let turn = last_turn(&finals, QuestionSource::Interviewer).unwrap();
        assert_eq!(turn.start, Duration::from_millis(9_600), "{}", turn.text);

        // And the log is where such a final stops: it never holds one.
        let mut log = log_of(&three_interviewer_finals());
        log.apply(final_at(Source::You, 9_100, 9_500, ""));
        assert_eq!(log.finals().len(), 4);
    }

    #[test]
    fn a_turn_goes_back_at_most_30_s() {
        let finals: Vec<Segment> = SESSION_24
            .iter()
            .map(|&(start, end, text)| final_at(Source::Interviewer, start, end, text))
            .collect();
        let log = log_of(&finals);
        let turn = last_turn(log.finals(), QuestionSource::Interviewer).unwrap();
        // 232.5 s back to 206.8 s is 25.7 s; the line before starts 30.4 s back.
        assert_eq!(turn.start, Duration::from_millis(206_800));
        assert_eq!(
            turn.text,
            [SESSION_24[2].2, SESSION_24[3].2, SESSION_24[4].2].join(" ")
        );
        assert!(turn.end - turn.start <= CONTINUE_SPAN);
    }

    #[test]
    fn the_turn_is_on_the_lane_the_question_source_selects() {
        let finals = [
            final_at(Source::Interviewer, 0, 5_000, FIRST),
            final_at(Source::You, 6_000, 9_000, MINE),
            final_at(Source::You, 9_400, 12_000, MINE_REST),
            final_at(Source::Interviewer, 13_000, 13_600, "Mm-hm."),
        ];
        let log = log_of(&finals);
        let turn = |asks| last_turn(log.finals(), asks).map(|c| (c.source, c.text));
        assert_eq!(
            turn(QuestionSource::Interviewer),
            Some((Source::Interviewer, "Mm-hm.".into())),
            "your lines in between cut the interviewer's turn"
        );
        assert_eq!(
            turn(QuestionSource::You),
            Some((Source::You, format!("{MINE} {MINE_REST}"))),
            "the mic lane's last turn, though the interviewer spoke after it"
        );
        assert_eq!(
            turn(QuestionSource::Both),
            Some((Source::Interviewer, "Mm-hm.".into())),
            "both: whichever lane spoke last"
        );
        let log = log_of(&finals[..3]);
        assert_eq!(
            last_turn(log.finals(), QuestionSource::Both).map(|c| c.source),
            Some(Source::You)
        );

        // Until the asking lane has said something there is no turn.
        assert!(last_turn(&[], QuestionSource::Both).is_none());
        let log = log_of(&finals[..1]);
        assert!(last_turn(log.finals(), QuestionSource::You).is_none());
    }

    #[test]
    fn controls_ask_goes_to_the_pipeline_thread() {
        let (controls, rx) = Controls::for_test();
        controls.ask();
        assert_eq!(rx.try_recv(), Ok(Command::Ask));
        assert_eq!(
            controls.hub().snapshot(),
            Session::default(),
            "the session changes only once the pipeline has a turn to send"
        );
    }

    #[test]
    fn enter_asks_the_last_thing_the_interviewer_said_when_the_detector_made_no_candidate() {
        // The whole path, store included: the session-24 lines are the transcript, nothing was
        // detected in them, and one key press puts the question and its Approach on screen.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let recorder = Recorder::start(Some(open_store(&paths).unwrap()), Some(&paths.db_file));
        let hub = recorder.hub();
        hub.dispatch(Action::Started {
            label: "session 24".into(),
            at: 1_790_000_000,
        });
        let mock = dmn_llm::MockProvider::json(&reply(RESERVE));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();
        hear(
            &hub,
            &mut log,
            final_at(
                Source::You,
                185_000,
                197_000,
                "So the gateway meters every request per tenant before it reaches a provider.",
            ),
        );
        hear_session_24(&hub, &mut log);
        let before = hub.snapshot();
        assert_eq!(before.counts.candidates, 0, "no candidate from them");
        assert!(screen(&before).contains("LISTENING"));

        a.command(Command::Ask, &log);
        let s = wait_until(&hub, "the question and its Approach", answered);
        let live = s.live.clone().unwrap();
        assert_eq!(live.text, RESERVE);
        let shown = screen(&s);
        assert!(shown.contains("LIVE QUESTION"), "{shown}");
        for part in [
            "How would you reserve and then reconcile usage correctly",
            "1  Reserve an estimate against the tenant's regional quota up front.",
            "3  Expire reservations that never settle.",
        ] {
            assert!(shown.contains(part), "{part}:\n{shown}");
        }
        assert!(s.asked.is_empty() && s.not_a_question.is_none());

        // What the model was asked: the turn as one candidate, and the 60 s window around it.
        let asked = asked_of(&mock);
        assert_eq!(asked.len(), 1);
        let turn = [SESSION_24[2].2, SESSION_24[3].2, SESSION_24[4].2].join(" ");
        assert!(
            asked[0].contains(&format!("## Interviewer just said\n{turn}")),
            "{}",
            asked[0]
        );
        assert!(
            asked[0].contains("You: So the gateway meters"),
            "{}",
            asked[0]
        );

        let (d, tally, _) = a.finish(false);
        assert!(d.found().is_empty(), "the detector made none");
        assert_eq!(tally.map(|t| t.shown), Some(1));
        drop(hub);
        let report = recorder.finish(1_790_000_300).unwrap().unwrap();
        assert_eq!((report.questions, report.errors), (1, 0), "{report:?}");
        let store = dmn_store::Store::open(&paths.db_file).unwrap();
        let questions = store.questions(report.session_id.unwrap()).unwrap();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].text.as_deref(), Some(RESERVE));
        assert_eq!(questions[0].kind.as_deref(), Some("system_design"));
        let suggestions = store.suggestions_for_question(questions[0].id).unwrap();
        assert_eq!(suggestions.len(), 1);
        assert!(
            suggestions[0]
                .payload
                .contains("Reconcile to the real usage when the stream ends."),
            "{}",
            suggestions[0].payload
        );
    }

    #[test]
    fn enter_while_pinned_queues_the_question_and_the_badge_counts_it() {
        let hub = Arc::new(SessionHub::default());
        let mock = dmn_llm::MockProvider::json(&reply(FIRST)).then_json(&reply(RESERVE));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        wait_until(&hub, "the detected question", answered);
        hub.dispatch(Action::Pin);
        hear_session_24(&hub, &mut log);

        a.command(Command::Ask, &log);
        let s = wait_until(&hub, "the asked question, queued", |s| {
            s.queued
                .first()
                .is_some_and(|q| q.progress == dmn_session::Progress::Done)
        });
        assert!(s.pinned);
        assert_eq!(s.live.as_ref().map(|q| q.text.as_str()), Some(FIRST));
        assert_eq!(s.queued.len(), 1);
        assert_eq!(s.queued[0].text, RESERVE);
        let shown = screen(&s);
        assert!(shown.contains("PINNED QUESTION"), "{shown}");
        assert!(shown.contains("1 new"), "the badge counts it:\n{shown}");
        assert!(!shown.contains("reserve and then reconcile"), "{shown}");

        // The queued question is that turn's: Enter again asks nothing.
        a.command(Command::Ask, &log);
        a.finish(false);
        assert_eq!(asked_of(&mock).len(), 2, "the detector's and the key's");
        assert_eq!(hub.snapshot().counts.candidates, 2);
    }

    #[test]
    fn enter_twice_on_the_same_turn_sends_one_request() {
        let hub = Arc::new(SessionHub::default());
        // Streams for about a second, so the second press comes while the first is in flight.
        let mock =
            dmn_llm::MockProvider::json(&reply(RESERVE)).with_delay(Duration::from_millis(20));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();
        hear_session_24(&hub, &mut log);
        a.command(Command::Ask, &log);
        a.command(Command::Ask, &log);
        assert_eq!(hub.snapshot().counts.candidates, 1);
        wait_until(&hub, "the asked question", answered);
        // On screen now: asking that one again is `r`.
        a.command(Command::Ask, &log);
        assert_eq!(hub.snapshot().counts.candidates, 1);

        // A new line from the interviewer is a new turn.
        hear(
            &hub,
            &mut log,
            final_at(
                Source::Interviewer,
                233_400,
                236_000,
                "And how would you test that under load?",
            ),
        );
        a.command(Command::Ask, &log);
        a.finish(false);
        assert_eq!(hub.snapshot().counts.candidates, 2);
        assert_eq!(asked_of(&mock).len(), 2);
    }

    #[test]
    fn enter_again_after_a_failed_request_asks_about_the_same_turn_again() {
        let hub = Arc::new(SessionHub::default());
        // The first request fails; the second is answered, streaming for about a second.
        let mock = dmn_llm::MockProvider::new(vec![dmn_llm::mock::Step::Error(
            "upstream timed out".into(),
        )])
        .then_json(&reply(RESERVE))
        .with_delay(Duration::from_millis(20));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();
        hear_session_24(&hub, &mut log);
        a.command(Command::Ask, &log);
        let s = wait_until(&hub, "the request to fail", |s| s.counts.failed == 1);
        assert_eq!(s.ask_failed, Some(1));
        assert!(s.live.is_none() && s.asked.is_empty());

        // Nothing new was said, and the key is not dead: the same turn goes out again.
        a.command(Command::Ask, &log);
        // That one is in flight: a double press is still one request.
        a.command(Command::Ask, &log);
        let s = hub.snapshot();
        assert_eq!((s.counts.candidates, s.ask_failed), (2, None));
        let s = wait_until(&hub, "the retry's answer", answered);
        assert_eq!(s.live.as_ref().map(|q| q.text.as_str()), Some(RESERVE));
        // And once it is on screen, asking again is `r`.
        a.command(Command::Ask, &log);
        a.finish(false);
        assert_eq!(hub.snapshot().counts.candidates, 2);
        let asked = asked_of(&mock);
        assert_eq!(asked.len(), 2, "the failed request and its retry");
        let turn = [SESSION_24[2].2, SESSION_24[3].2, SESSION_24[4].2].join(" ");
        for user in &asked {
            assert!(
                user.contains(&format!("## Interviewer just said\n{turn}")),
                "{user}"
            );
        }
    }

    #[test]
    fn enter_on_a_turn_the_detector_already_made_the_question_asks_nothing() {
        let hub = Arc::new(SessionHub::default());
        let mock = dmn_llm::MockProvider::json(&reply(FIRST)).with_delay(Duration::from_millis(20));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        // Its reply is on its way: an impatient Enter must not cancel it and ask again.
        a.command(Command::Ask, &log);
        wait_until(&hub, "the detected question", answered);
        // And once it is the live question, asking it again is `r`.
        a.command(Command::Ask, &log);
        a.finish(false);
        assert_eq!(hub.snapshot().counts.candidates, 1);
        assert_eq!(asked_of(&mock).len(), 1);
    }

    #[test]
    fn enter_sends_nothing_with_assistance_off_or_before_anyone_has_spoken() {
        let (mut a, mock, _) = assistance(true);
        let hub = Arc::clone(&a.hub);
        let mut log = TranscriptLog::new();
        a.command(Command::Ask, &log);
        // Your own greeting is not the asking lane speaking.
        hear(
            &hub,
            &mut log,
            final_at(Source::You, 1_000, 3_000, "Hi, can you hear me?"),
        );
        a.command(Command::Ask, &log);
        assert_eq!(hub.snapshot().counts.candidates, 0, "nobody to ask about");

        a.command(Command::Assist { on: false }, &log);
        hear_session_24(&hub, &mut log);
        a.command(Command::Ask, &log);
        assert_eq!(hub.snapshot().counts.candidates, 0, "assistance is off");
        let (_, tally, _) = a.finish(false);
        assert_eq!(tally.map(|t| t.shown + t.ignored + t.failed), Some(0));
        assert!(mock.requests().is_empty(), "no provider request");

        // Off from the start: the key does not start the suggester either.
        let (mut a, mock, launched) = assistance(false);
        a.command(Command::Ask, &log);
        assert!(!launched.load(Ordering::Acquire));
        a.finish(false);
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn a_declined_key_press_says_not_a_question_for_5_s_and_a_declined_detector_candidate_nothing()
    {
        let hub = Arc::new(SessionHub::default());
        let mock = dmn_llm::MockProvider::json(&serde_json::json!({
            "is_question": false,
            "kind": "small_talk"
        }));
        let mut a = asking(&hub, &mock, QuestionSource::Interviewer);
        let mut log = TranscriptLog::new();

        // The detector's candidate, declined: nothing is said, as before #96.
        publish(
            &mut a,
            &mut log,
            final_at(Source::Interviewer, 20_000, 26_000, FIRST),
        );
        let s = wait_until(&hub, "the detector's candidate declined", |s| {
            s.counts.ignored == 1
        });
        assert_eq!(s.not_a_question, None);
        assert!(!screen(&s).contains("not a question"));
        let now = Instant::now();
        a.expire_notice(now);
        a.expire_notice(now + NOT_A_QUESTION_FOR);
        assert_eq!(hub.snapshot(), s, "no notice, so nothing to take down");

        // The same reply to a key press: the status line answers it.
        hear(
            &hub,
            &mut log,
            final_at(Source::You, 27_000, 30_000, "A token bucket per API key."),
        );
        hear(
            &hub,
            &mut log,
            final_at(
                Source::Interviewer,
                31_000,
                33_000,
                "Okay, that makes sense.",
            ),
        );
        a.command(Command::Ask, &log);
        let s = wait_until(&hub, "the key's candidate declined", |s| {
            s.counts.ignored == 2
        });
        assert_eq!(s.not_a_question, Some(2));
        assert!(s.live.is_none(), "the screen keeps what it had");
        let shown = screen(&s);
        assert!(shown.contains("LISTENING"), "{shown}");
        assert!(
            shown.contains("not a question · m mute · q quit · ⏎ ask"),
            "{shown}"
        );

        // Up for 5 s from when the pipeline first sees it, then taken down.
        let seen = Instant::now();
        a.expire_notice(seen);
        a.expire_notice(seen + NOT_A_QUESTION_FOR - Duration::from_millis(1));
        assert_eq!(hub.snapshot().not_a_question, Some(2), "not yet");
        a.expire_notice(seen + NOT_A_QUESTION_FOR);
        let s = hub.snapshot();
        assert_eq!(s.not_a_question, None);
        assert!(!screen(&s).contains("not a question"));

        // The model has answered for that turn, and at temperature 0 would say the same again:
        // Enter asks nothing, unlike after a request that failed.
        assert_eq!(s.ask_failed, None);
        a.command(Command::Ask, &log);
        a.finish(false);
        assert_eq!(asked_of(&mock).len(), 2);
    }

    #[test]
    fn a_failed_web_lookup_is_said_for_5_s_beside_the_other_notice() {
        // #102: the suggester says `lookup failed`; this thread takes it down after 5 s, on its
        // own clock, whatever `not a question` is doing.
        let (mut a, _, _) = assistance(true);
        let hub = Arc::clone(&a.hub);
        hub.dispatch(Action::LookupFailed { candidate: 3 });
        let seen = Instant::now();
        a.expire_notice(seen);
        a.expire_notice(seen + LOOKUP_FAILED_FOR - Duration::from_millis(1));
        assert_eq!(hub.snapshot().lookup_failed, Some(3), "not yet");
        assert!(screen(&hub.snapshot()).contains("lookup failed"));
        a.expire_notice(seen + LOOKUP_FAILED_FOR);
        let s = hub.snapshot();
        assert_eq!(s.lookup_failed, None);
        assert!(!screen(&s).contains("lookup failed"));
    }

    #[test]
    fn under_you_enter_sends_the_mic_lanes_last_turn() {
        let hub = Arc::new(SessionHub::default());
        let mock = dmn_llm::MockProvider::json(&reply(MINE));
        let mut a = asking(&hub, &mock, QuestionSource::You);
        let mut log = TranscriptLog::new();
        for seg in [
            final_at(Source::Interviewer, 0, 5_000, FIRST),
            final_at(
                Source::You,
                6_000,
                9_000,
                "So for the limiter I keep going back and forth.",
            ),
            final_at(
                Source::You,
                9_400,
                13_000,
                "Token bucket or sliding window for a public API.",
            ),
            final_at(Source::Interviewer, 14_000, 14_600, "Mm-hm."),
        ] {
            hear(&hub, &mut log, seg);
        }
        a.command(Command::Ask, &log);
        wait_until(&hub, "the answer to the mic lane's turn", answered);
        a.finish(false);
        let asked = asked_of(&mock);
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains(
                "## You just said\nSo for the limiter I keep going back and forth. Token bucket \
                 or sliding window for a public API."
            ),
            "{}",
            asked[0]
        );
        assert!(
            !asked[0].contains("## Interviewer just said"),
            "{}",
            asked[0]
        );
    }

    // --- #113: translation-only: a translator of its own, the answer model left out ---------

    /// `[speak]` on with a translator of its own, against a stand-in Ollama.
    fn translator_cfg(url: &str) -> Config {
        let mut cfg = Config::default();
        cfg.llm.base_url = url.to_owned();
        cfg.llm.model = "answer-35b".into();
        cfg.speak.enabled = true;
        cfg.speak.target = "de".into();
        cfg.speak.model = Some("translator-8b".into());
        cfg
    }

    /// Assistance off at start with `[speak].model` set: the answer model is only asked to
    /// leave, never loaded; the translator is loaded and every translation names it; and
    /// Ctrl+Shift+A loads the answer model, as #79 does.
    #[test]
    fn with_assistance_off_and_a_translator_only_the_translator_is_loaded() {
        let ollama =
            crate::testollama::FakeOllama::start("Ich würde mit einem Token-Bucket anfangen.");
        let cfg = translator_cfg(&ollama.url);

        // Start-up, whisper on the GPU: the answer model leaves, the translator loads first.
        prepare_local_models(&cfg, false, true);
        let seen = ollama.seen();
        assert!(
            seen.iter()
                .any(|r| r.model() == "answer-35b" && r.is_unload()),
            "#79's unload is back: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|r| r.model() == "translator-8b" && r.is_warm_up()),
            "{seen:?}"
        );

        // `t`: the speaker warms and translates on the translator.
        let hub = Arc::new(SessionHub::default());
        let provider = wire::provider(&cfg.translator_llm()).unwrap();
        let (setup, played) =
            crate::speaker::setup_on(provider, dmn_tts::MockTtsProvider::new(), true);
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 1 });
        assert!(speaker.speak(1, SAID.into()));
        let after = crate::speaker::tests::settled(&hub);
        assert_eq!(after.speak_failed, None, "{:?}", after.speak_failed);
        assert!(
            !played.lock().unwrap().is_empty(),
            "the translation was spoken"
        );
        speaker.stop();
        let chats: Vec<_> = ollama.seen().into_iter().filter(|r| r.is_chat()).collect();
        assert_eq!(chats.len(), 1, "one translation: {chats:?}");
        assert_eq!(chats[0].model(), "translator-8b");
        assert_eq!(chats[0].body["think"], false);
        assert_eq!(chats[0].body["messages"][1]["content"], SAID);
        assert!(
            ollama
                .seen()
                .iter()
                .all(|r| r.model() != "answer-35b" || r.is_unload()),
            "nothing loads the answer model while assistance is off: {:?}",
            ollama.seen()
        );

        // Ctrl+Shift+A: the suggester starts and loads the answer model.
        let hub = Arc::new(SessionHub::default());
        let launch = {
            let (llm, hub) = (cfg.llm.clone(), Arc::clone(&hub));
            Box::new(move || {
                Suggester::start(
                    &llm,
                    &dmn_core::config::Lookup::default(),
                    Output::Quiet,
                    Policy::AnswerAll,
                    hub,
                    wire::SessionClock::start(1.0),
                    AudioClock::new(Instant::now(), 1.0),
                    ContextSource::Off,
                    None,
                )
            }) as Launch
        };
        let mut a = Assistance::new(
            false,
            hub,
            AudioClock::new(Instant::now(), 1.0),
            detecting_for(QuestionSource::Interviewer),
            Some(launch),
        );
        a.command(Command::Assist { on: true }, &TranscriptLog::new());
        assert!(
            ollama.wait_for(|r| r.model() == "answer-35b" && r.is_warm_up()),
            "Ctrl+Shift+A loads [llm].model: {:?}",
            ollama.seen()
        );
        let _ = a.finish(true);
    }

    /// With assistance on both models are wanted; with `[speak].model` left out `t` translates
    /// on the answer model, which stays loaded with assistance off (#109, unchanged).
    #[test]
    fn with_assistance_on_or_no_translator_the_answer_model_is_kept() {
        let ollama = crate::testollama::FakeOllama::start("");
        let cfg = translator_cfg(&ollama.url);
        prepare_local_models(&cfg, true, true);
        let seen = ollama.seen();
        assert!(seen.iter().all(|r| !r.is_unload()), "{seen:?}");
        for model in ["answer-35b", "translator-8b"] {
            assert!(
                seen.iter().any(|r| r.model() == model && r.is_warm_up()),
                "{model}: {seen:?}"
            );
        }

        let ollama = crate::testollama::FakeOllama::start("");
        let mut cfg = translator_cfg(&ollama.url);
        cfg.speak.model = None;
        prepare_local_models(&cfg, false, true);
        let seen = ollama.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].model() == "answer-35b" && seen[0].is_warm_up());
        // Whisper on the CPU: nothing is loaded before it; the speaker and the suggester warm
        // their own.
        let ollama = crate::testollama::FakeOllama::start("");
        let cfg = translator_cfg(&ollama.url);
        prepare_local_models(&cfg, false, false);
        let seen = ollama.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].model() == "answer-35b" && seen[0].is_unload());
    }

    // --- #109: `t` speaks your last turn in the interview's language ---------------------------

    const SAID: &str = "I'd start with a token bucket per API key.";
    const SAID_REST: &str = "And I'd keep the counters in Redis.";
    const SPANISH: &str = "Empezaría con un token bucket por clave de API y guardaría los \
                           contadores en Redis.";

    fn spanish() -> dmn_llm::MockProvider {
        dmn_llm::MockProvider::new(vec![
            dmn_llm::mock::Step::Text(SPANISH.into()),
            dmn_llm::mock::Step::Done,
        ])
    }

    /// `t` on `hub` with a speaker over `llm` and `tts`, and what reached its sink.
    fn speaking(
        hub: &Arc<SessionHub>,
        llm: &dmn_llm::MockProvider,
        tts: &dmn_tts::MockTtsProvider,
    ) -> (Speaking, crate::speaker::Fed) {
        let (s, played, _) = speaking_in(hub, llm, tts, VoiceMode::Passthrough);
        (s, played)
    }

    /// [`speaking`] starting in `mode` (#114), with the sink's microphone gate too.
    fn speaking_in(
        hub: &Arc<SessionHub>,
        llm: &dmn_llm::MockProvider,
        tts: &dmn_tts::MockTtsProvider,
        mode: VoiceMode,
    ) -> (Speaking, crate::speaker::Fed, Arc<AtomicBool>) {
        let (s, played, gate, _) = speaking_saved_to(hub, llm, tts, mode, None);
        (s, played, gate)
    }

    /// [`speaking_in`], with the picker's choice saved to `config_file` (#115) and the
    /// languages the test speaker lists.
    fn speaking_saved_to(
        hub: &Arc<SessionHub>,
        llm: &dmn_llm::MockProvider,
        tts: &dmn_tts::MockTtsProvider,
        mode: VoiceMode,
        config_file: Option<PathBuf>,
    ) -> (
        Speaking,
        crate::speaker::Fed,
        Arc<AtomicBool>,
        Vec<Language>,
    ) {
        let (setup, played, gate) =
            crate::speaker::setup_sink(Arc::new(llm.clone()), tts.clone(), true);
        let languages = setup.languages.clone();
        let mut s = Speaking::new(Arc::clone(hub));
        s.attach(
            Speaker::start(setup, Arc::clone(hub)).unwrap(),
            mode,
            "es".into(),
            Some("en".into()),
            languages.clone(),
            config_file,
        );
        (s, played, gate, languages)
    }

    /// The interviewer's question, then your answer in two finals.
    fn your_answer(hub: &SessionHub) -> TranscriptLog {
        let mut log = TranscriptLog::new();
        for seg in [
            final_at(Source::Interviewer, 0, 5_000, FIRST),
            final_at(Source::You, 6_000, 9_000, SAID),
            final_at(Source::You, 9_400, 12_000, SAID_REST),
        ] {
            hear(hub, &mut log, seg);
        }
        log
    }

    #[test]
    fn t_after_a_you_final_translates_that_turn_then_synthesizes_the_translation_then_plays_it() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, played) = speaking(&hub, &llm, &tts);
        let log = your_answer(&hub);
        s.press(&log);
        let pressed = hub.snapshot().speech.expect("a press in flight at once");
        assert_eq!(pressed.stage, dmn_session::SpeakStage::Translating);
        let after = wait_until(&hub, "the press played", |s| {
            s.speech.is_none() && !tts.requests().is_empty()
        });
        s.finish();
        assert_eq!(after.speak_failed, None, "{:?}", after.speak_failed);

        let asked = llm.requests();
        assert_eq!(asked.len(), 1, "exactly one translate request");
        assert_eq!(
            asked[0].messages[1].content,
            format!("{SAID} {SAID_REST}"),
            "its body is your last turn, nothing else"
        );
        let synthesized = tts.requests();
        assert_eq!(synthesized.len(), 1, "exactly one synthesize request");
        assert_eq!(synthesized[0].text, SPANISH, "of the translation");
        let fed: usize = played.lock().unwrap().iter().map(|(n, ..)| n).sum();
        assert_eq!(fed, 12_000, "then playback: the speech reached dmn-voice");
    }

    #[test]
    fn t_before_you_have_said_anything_says_nothing_to_speak_and_asks_nothing() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, played) = speaking(&hub, &llm, &tts);
        let mut log = TranscriptLog::new();
        hear(
            &hub,
            &mut log,
            final_at(Source::Interviewer, 0, 5_000, FIRST),
        );
        s.press(&log);
        let snap = hub.snapshot();
        assert_eq!(snap.speech, None);
        assert_eq!(
            snap.speak_failed.as_ref().map(|n| n.reason.as_str()),
            Some(NOTHING_TO_SPEAK)
        );
        assert!(screen(&snap).contains("nothing to speak"));
        // Up for 5 s on this thread's clock, then gone.
        let mut echo = EchoGuard::new(true);
        let clock = AudioClock::new(Instant::now(), 1.0);
        let seen = Instant::now();
        s.tick(seen, &mut echo, &clock);
        s.tick(
            seen + SPEAK_FAILED_FOR - Duration::from_millis(1),
            &mut echo,
            &clock,
        );
        assert!(hub.snapshot().speak_failed.is_some(), "not yet");
        s.tick(seen + SPEAK_FAILED_FOR, &mut echo, &clock);
        assert_eq!(hub.snapshot().speak_failed, None);
        s.finish();
        assert!(llm.requests().is_empty(), "no LLM request");
        assert!(tts.requests().is_empty(), "no TTS request");
        assert!(played.lock().unwrap().is_empty());
    }

    /// #113: the start-up notice that nothing is linked to `dmn-voice` is on the status line
    /// for 5 s, like a press's, and a press meanwhile is not disturbed by it.
    #[test]
    fn the_nothing_linked_notice_shows_for_five_seconds_and_a_press_still_speaks() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _) = speaking(&hub, &llm, &tts);
        hub.dispatch(Action::SpeakFailed {
            id: crate::speaker::STARTUP_NOTICE,
            reason: crate::speaker::NOT_LINKED.to_owned(),
        });
        assert!(
            screen(&hub.snapshot()).contains("Discord: re-select dmn-voice"),
            "{}",
            screen(&hub.snapshot())
        );
        let mut echo = EchoGuard::new(true);
        let clock = AudioClock::new(Instant::now(), 1.0);
        let seen = Instant::now();
        s.tick(seen, &mut echo, &clock);
        s.tick(seen + SPEAK_FAILED_FOR, &mut echo, &clock);
        assert_eq!(hub.snapshot().speak_failed, None);

        // A notice up while a press is in flight leaves the press alone.
        let mut log = TranscriptLog::new();
        hear(&hub, &mut log, final_at(Source::You, 0, 3_000, SAID));
        s.press(&log);
        hub.dispatch(Action::SpeakFailed {
            id: crate::speaker::STARTUP_NOTICE,
            reason: crate::speaker::NOT_LINKED.to_owned(),
        });
        let after = crate::speaker::tests::settled(&hub);
        assert_eq!(tts.requests().len(), 1, "the press was spoken");
        assert_eq!(
            after.speak_failed.map(|n| n.id),
            Some(crate::speaker::STARTUP_NOTICE),
            "only the notice"
        );
        s.finish();
    }

    #[test]
    fn t_does_nothing_while_speak_is_off() {
        let hub = Arc::new(SessionHub::default());
        let mut s = Speaking::new(Arc::clone(&hub));
        let log = your_answer(&hub);
        let before = hub.snapshot();
        s.press(&log);
        s.tick(
            Instant::now(),
            &mut EchoGuard::new(true),
            &AudioClock::new(Instant::now(), 1.0),
        );
        let after = hub.snapshot();
        assert_eq!(after, before, "no action at all");
        assert!(!after.speak && after.speech.is_none() && after.speak_failed.is_none());
        assert!(!screen(&after).contains("t speak"));
        s.finish();
    }

    /// `t` is a speech feature, not an answer feature: with assistance off it speaks, while the
    /// layer that answers questions ignores it and never starts its model.
    #[test]
    fn t_works_with_assistance_off() {
        let (mut a, mock, launched) = assistance(false);
        let hub = Arc::clone(&a.hub);
        hub.dispatch(Action::Assist { enabled: false });
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _) = speaking(&hub, &llm, &tts);
        let log = your_answer(&hub);
        a.command(Command::Speak, &log);
        s.press(&log);
        wait_until(&hub, "the press played", |s| {
            s.speech.is_none() && !tts.requests().is_empty()
        });
        s.finish();
        assert_eq!(llm.requests().len(), 1);
        assert_eq!(tts.requests().len(), 1);
        assert!(!launched.load(Ordering::Acquire), "no suggester");
        assert!(
            mock.requests().is_empty(),
            "the answering model was not asked"
        );
        assert!(screen(&hub.snapshot()).contains("assist off"));
    }

    #[test]
    fn a_second_t_while_one_is_in_flight_does_nothing() {
        let hub = Arc::new(SessionHub::default());
        // A slow model keeps the first press translating while the second comes.
        let llm = spanish().with_delay(Duration::from_millis(100));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, _) = speaking(&hub, &llm, &tts);
        let log = your_answer(&hub);
        s.press(&log);
        s.press(&log);
        s.press(&log);
        assert_eq!(s.presses, 1, "only the first press counts");
        wait_until(&hub, "the press played", |s| {
            s.speech.is_none() && !tts.requests().is_empty()
        });
        // Once it has played, the next press speaks again.
        s.press(&log);
        assert_eq!(s.presses, 2);
        wait_until(&hub, "the second press played", |s| {
            s.speech.is_none() && tts.requests().len() == 2
        });
        s.finish();
        assert_eq!(llm.requests().len(), 2);
    }

    /// The status line's seconds come from this thread's clock, one action per new second; a
    /// press that fails is said for 5 s like any notice.
    #[test]
    fn the_seconds_since_the_press_count_up_on_the_status_line() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _) = speaking(&hub, &llm, &tts);
        let mut echo = EchoGuard::new(true);
        let clock = AudioClock::new(Instant::now(), 1.0);
        // A press as the reducer sees it, with no job behind it.
        let t0 = Instant::now();
        hub.dispatch(Action::SpeakStarted { id: 9 });
        s.pressed = Some((9, t0));
        s.tick(t0 + Duration::from_millis(900), &mut echo, &clock);
        assert_eq!(hub.snapshot().speech.unwrap().secs, 0);
        s.tick(t0 + Duration::from_millis(3_200), &mut echo, &clock);
        let snap = hub.snapshot();
        assert_eq!(snap.speech.as_ref().unwrap().secs, 3);
        assert!(
            screen(&snap).contains("translating… 3s"),
            "{}",
            screen(&snap)
        );
        hub.dispatch(Action::SpeakTranslated {
            id: 9,
            text: SPANISH.into(),
        });
        s.tick(t0 + Duration::from_millis(11_000), &mut echo, &clock);
        let snap = hub.snapshot();
        assert!(screen(&snap).contains("speaking… 11s"), "{}", screen(&snap));
        assert!(screen(&snap).contains("» Empezaría con un token bucket"));
        hub.dispatch(Action::SpeakDone { id: 9 });
        let snap = hub.snapshot();
        assert!(!screen(&snap).contains("speaking…"), "the tag clears");
        assert!(!screen(&snap).contains("» "), "and the line");
        s.finish();
    }

    /// The echo window (no PipeWire): what either lane heard while the speech played is dropped
    /// by the guard; what came after is kept.
    #[test]
    fn segments_heard_while_t_plays_are_dropped_and_later_ones_kept() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _) = speaking(&hub, &llm, &tts);
        let mut echo = EchoGuard::new(true);
        let clock = AudioClock::new(Instant::now(), 1.0);
        let log = your_answer(&hub);
        let pressed_at = Duration::from_secs_f64(clock.now());
        s.press(&log);
        wait_until(&hub, "the press played", |s| {
            s.speech.is_none() && !tts.requests().is_empty()
        });
        s.tick(Instant::now(), &mut echo, &clock);
        let done_at = Duration::from_secs_f64(clock.now());
        s.finish();
        let ms = |d: Duration| d.as_millis() as u64;
        let during = final_at(
            Source::Interviewer,
            ms(pressed_at),
            ms(done_at),
            "Empezaría con un token bucket.",
        );
        let verdicts = echo.observe(during.clone(), Duration::from_secs(100));
        assert_eq!(verdicts, [Verdict::Drop(during)]);
        let later = final_at(
            Source::Interviewer,
            ms(done_at) + 5_000,
            ms(done_at) + 8_000,
            LATER,
        );
        let verdicts = echo.observe(later.clone(), Duration::from_secs(100));
        assert_eq!(verdicts, [Verdict::Publish(later)]);
    }

    /// #109 with #77: with `[speak]` on the You lane keeps your own words; the Interviewer lane
    /// translates as configured. With `[speak]` off, or English, nothing changes.
    #[test]
    fn with_speak_on_the_you_lane_transcribes_without_whisper_translate() {
        use dmn_stt::Language;
        let auto = Language::Auto;
        let es = Language::Fixed("es".into());
        assert_eq!(lane_language(Source::You, &auto, true), Language::Detect);
        assert_eq!(lane_language(Source::You, &es, true), Language::Detect);
        assert_eq!(lane_language(Source::Interviewer, &auto, true), auto);
        assert_eq!(lane_language(Source::Interviewer, &es, true), es);
        for lang in [Language::English, auto.clone(), es.clone()] {
            for source in [Source::You, Source::Interviewer] {
                assert_eq!(lane_language(source, &lang, false), lang, "speak off");
            }
        }
        assert_eq!(
            lane_language(Source::You, &Language::English, true),
            Language::English,
            "English needs no translation to keep"
        );
    }

    // --- #114: your own voice through dmn-voice by default; `l` makes every turn go out translated

    /// Wait for the speaker's thread to set the sink's microphone gate to `want`.
    fn gate_settles(gate: &AtomicBool, want: bool) {
        let t = Instant::now();
        while gate.load(Ordering::Acquire) != want {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "the gate never went {want}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Drive `s` (ticks on this thread, as the loop does) until `done`, or 10 s.
    fn drive(
        s: &mut Speaking,
        hub: &SessionHub,
        what: &str,
        mut done: impl FnMut(&Session) -> bool,
    ) -> Session {
        let mut echo = EchoGuard::new(true);
        let clock = AudioClock::new(Instant::now(), 1.0);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            s.tick(Instant::now(), &mut echo, &clock);
            let snap = hub.snapshot();
            if done(&snap) {
                return snap;
            }
            assert!(Instant::now() < until, "never: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn mode_of(hub: &SessionHub) -> Option<VoiceMode> {
        hub.snapshot().voice.map(|v| v.mode)
    }

    #[test]
    fn l_switches_passthrough_and_translate_and_the_microphones_gate_follows() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _, gate) = speaking_in(&hub, &llm, &tts, VoiceMode::Passthrough);
        let snap = hub.snapshot();
        assert!(snap.speak);
        assert_eq!(
            snap.voice,
            Some(dmn_session::Voice {
                mode: VoiceMode::Passthrough,
                target: "es".into(),
                own: Some("en".into()),
            })
        );
        let shown = screen(&snap);
        assert!(
            shown.contains("voice: you (en)") && shown.contains("l translate"),
            "{shown}"
        );
        assert!(
            snap.transcript.is_empty(),
            "the start-up mode writes no line"
        );
        gate_settles(&gate, true);

        s.toggle();
        assert_eq!(mode_of(&hub), Some(VoiceMode::Translate));
        let snap = hub.snapshot();
        let shown = screen(&snap);
        assert!(
            shown.contains("voice: → es") && shown.contains("l own voice"),
            "{shown}"
        );
        assert_eq!(snap.transcript.len(), 1);
        assert!(snap.transcript[0].note);
        assert_eq!(snap.transcript[0].text, "— translate mode on: → es —");
        gate_settles(&gate, false);

        s.toggle();
        assert_eq!(mode_of(&hub), Some(VoiceMode::Passthrough));
        let snap = hub.snapshot();
        assert_eq!(snap.transcript.len(), 2);
        assert_eq!(
            snap.transcript[1].text,
            "— translate mode off: your own voice —"
        );
        assert!(screen(&snap).contains("voice: you (en)"));
        gate_settles(&gate, true);
        s.finish();
        assert!(
            llm.requests().is_empty() && tts.requests().is_empty(),
            "no key spoke"
        );
    }

    /// The issue's criterion: two finalized You turns are two translate requests and two
    /// synthesize requests, in order, the second starting only when the first has played; the
    /// microphone is shut while either plays; both lines are marked for what they went out as.
    #[test]
    fn in_translate_mode_each_finalized_you_turn_is_spoken_in_order_the_second_waiting_for_the_first()
     {
        let hub = Arc::new(SessionHub::default());
        // A slow model keeps the first turn translating while the second is finalized.
        let llm = spanish().with_delay(Duration::from_millis(300));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, played, gate) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        gate_settles(&gate, false);
        let mut log = TranscriptLog::new();
        hear(
            &hub,
            &mut log,
            final_at(Source::Interviewer, 0, 5_000, FIRST),
        );
        let first = final_at(Source::You, 6_000, 9_000, SAID);
        hear(&hub, &mut log, first.clone());
        s.heard(&first);
        let snap = hub.snapshot();
        let sp = snap
            .speech
            .clone()
            .expect("the turn is in flight at once, no key");
        assert_eq!(
            (sp.id, sp.stage, sp.span),
            (1, dmn_session::SpeakStage::Translating, Some((6.0, 9.0)))
        );
        assert!(screen(&snap).contains("voice: → es"));

        let second = final_at(Source::You, 12_000, 15_000, SAID_REST);
        hear(&hub, &mut log, second.clone());
        s.heard(&second);
        assert_eq!(s.queue.len(), 1, "the second waits");
        assert_eq!(llm.requests().len(), 1, "and is not translated yet");

        // When the second starts, the first has been synthesized and played.
        let mut second_started_after = None;
        let after = drive(&mut s, &hub, "both turns played", |snap| {
            if snap.speech.as_ref().is_some_and(|sp| sp.id == 2) && second_started_after.is_none() {
                second_started_after = Some(tts.requests().len());
            }
            snap.speech.is_none() && tts.requests().len() == 2
        });
        assert_eq!(
            second_started_after,
            Some(1),
            "the second started after the first played"
        );
        assert_eq!(s.queue.len(), 0);
        let asked: Vec<String> = llm
            .requests()
            .iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(asked, [SAID, SAID_REST], "two translate requests, in order");
        let spoken: Vec<String> = tts.requests().iter().map(|r| r.text.clone()).collect();
        assert_eq!(
            spoken,
            [SPANISH, SPANISH],
            "two synthesize requests, in order"
        );
        let fed = played.lock().unwrap();
        assert_eq!(
            fed.iter().map(|(n, ..)| n).sum::<usize>(),
            24_000,
            "both utterances reached the sink: {fed:?}"
        );
        assert!(
            fed.iter().all(|(_, _, mic)| !mic),
            "the microphone's gate was shut while either played: {fed:?}"
        );
        drop(fed);
        let yours: Vec<(Option<String>, Option<String>)> = after
            .transcript
            .iter()
            .filter(|l| l.speaker == dmn_session::Speaker::You)
            .map(|l| (l.spoken_as.clone(), l.translation.clone()))
            .collect();
        assert_eq!(
            yours,
            [
                (Some("es".into()), Some(SPANISH.into())),
                (Some("es".into()), Some(SPANISH.into())),
            ],
            "both lines went out translated"
        );
        assert!(screen(&after).contains("You → es"), "{}", screen(&after));
        assert_eq!(after.speak_failed, None, "{:?}", after.speak_failed);
        s.finish();
    }

    /// #114 (review of PR #119): while a turn waits, a You final joins it only until the
    /// interviewer speaks; what the user says after that is a turn of its own, as `last_turn`
    /// groups them. Here the first turn is in flight, the second waits, the interviewer answers,
    /// and the third and fourth finals arrive: three translate requests, the third and fourth
    /// finals joined into the third.
    #[test]
    fn in_translate_mode_the_interviewer_speaking_ends_the_waiting_turn() {
        let hub = Arc::new(SessionHub::default());
        let llm = spanish().with_delay(Duration::from_millis(300));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, _played, gate) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        gate_settles(&gate, false);
        let mut log = TranscriptLog::new();
        let first = final_at(Source::You, 6_000, 9_000, SAID);
        hear(&hub, &mut log, first.clone());
        s.heard(&first);
        let second = final_at(Source::You, 12_000, 15_000, SAID_REST);
        hear(&hub, &mut log, second.clone());
        s.heard(&second);
        assert_eq!(s.queue.len(), 1, "the second waits");
        let asked = final_at(Source::Interviewer, 16_000, 18_000, FIRST);
        hear(&hub, &mut log, asked.clone());
        s.heard(&asked);
        assert_eq!(
            s.queue.len(),
            1,
            "the interviewer's line is not a turn to speak"
        );
        // Long enough that the mock's Spanish passes #117's length rule for the joined turn.
        let third = final_at(Source::You, 19_000, 21_000, SAID);
        hear(&hub, &mut log, third.clone());
        s.heard(&third);
        assert_eq!(
            s.queue.len(),
            2,
            "after the interviewer spoke, a turn of its own"
        );
        let fourth = final_at(Source::You, 21_500, 23_000, SAID_REST);
        hear(&hub, &mut log, fourth.clone());
        s.heard(&fourth);
        assert_eq!(s.queue.len(), 2, "nothing in between: joined to the third");
        let joined = format!("{SAID} {SAID_REST}");
        assert_eq!(
            s.queue.back().map(|u| u.text.as_str()),
            Some(joined.as_str())
        );
        let _ = drive(&mut s, &hub, "all turns played", |snap| {
            snap.speech.is_none() && tts.requests().len() == 3
        });
        let asked: Vec<String> = llm
            .requests()
            .iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(asked, [SAID, SAID_REST, joined.as_str()]);
        s.finish();
    }

    #[test]
    fn leaving_translate_mode_drops_the_waiting_turn_not_the_one_speaking_and_opens_the_gate() {
        let hub = Arc::new(SessionHub::default());
        let llm = spanish().with_delay(Duration::from_millis(300));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, _, gate) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        let mut log = TranscriptLog::new();
        let first = final_at(Source::You, 6_000, 9_000, SAID);
        hear(&hub, &mut log, first.clone());
        s.heard(&first);
        let second = final_at(Source::You, 12_000, 15_000, SAID_REST);
        hear(&hub, &mut log, second.clone());
        s.heard(&second);
        assert_eq!(s.queue.len(), 1);

        s.toggle();
        assert_eq!(s.queue.len(), 0, "the waiting turn is dropped");
        assert_eq!(mode_of(&hub), Some(VoiceMode::Passthrough));
        assert!(hub.snapshot().speech.is_some(), "the one speaking goes on");
        let after = drive(&mut s, &hub, "the first played", |snap| {
            snap.speech.is_none()
        });
        // A few more loops: nothing else starts.
        for _ in 0..5 {
            s.tick(
                Instant::now(),
                &mut EchoGuard::new(true),
                &AudioClock::new(Instant::now(), 1.0),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(llm.requests().len(), 1, "only the first was translated");
        assert_eq!(tts.requests().len(), 1);
        gate_settles(&gate, true);
        let marked: Vec<bool> = after
            .transcript
            .iter()
            .filter(|l| !l.note)
            .map(|l| l.spoken_as.is_some())
            .collect();
        assert_eq!(marked, [true, false]);

        // Back in pass-through mode `t` is #109's: the last turn, once, over the microphone.
        s.press(&log);
        let after = drive(&mut s, &hub, "the press played", |snap| {
            snap.speech.is_none() && tts.requests().len() == 2
        });
        assert_eq!(
            llm.requests()[1].messages[1].content,
            format!("{SAID} {SAID_REST}")
        );
        assert_eq!(after.speak_failed, None);
        s.finish();
    }

    #[test]
    fn t_in_translate_mode_speaks_the_last_turn_again_behind_whatever_is_speaking() {
        let hub = Arc::new(SessionHub::default());
        let llm = spanish().with_delay(Duration::from_millis(200));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, _, _) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        let mut log = TranscriptLog::new();
        // Nothing said yet: as in pass-through mode.
        s.press(&log);
        assert_eq!(
            hub.snapshot().speak_failed.map(|n| n.reason).as_deref(),
            Some(NOTHING_TO_SPEAK)
        );
        let first = final_at(Source::You, 6_000, 9_000, SAID);
        hear(&hub, &mut log, first.clone());
        s.heard(&first);
        // `t` while the automatic one is in flight is not refused: it queues the turn again.
        s.press(&log);
        assert_eq!(s.queue.len(), 1);
        let after = drive(&mut s, &hub, "both played", |snap| {
            snap.speech.is_none() && tts.requests().len() == 2
        });
        let asked: Vec<String> = llm
            .requests()
            .iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(asked, [SAID, SAID], "the same turn, twice");
        assert_eq!(after.speak_failed, None);
        // And once everything has played, `t` speaks it a third time, at once.
        s.press(&log);
        assert!(hub.snapshot().speech.is_some());
        drive(&mut s, &hub, "the third played", |snap| {
            snap.speech.is_none() && tts.requests().len() == 3
        });
        s.finish();
    }

    /// The `last_turn` grouping on the queue: finals that arrive while a turn is already
    /// waiting join it, so a sentence the VAD split in two goes out as one utterance.
    #[test]
    fn you_finals_that_arrive_while_a_turn_waits_are_one_utterance() {
        let hub = Arc::new(SessionHub::default());
        let llm = spanish().with_delay(Duration::from_millis(300));
        let tts = dmn_tts::MockTtsProvider::new();
        let (mut s, _, _) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        let mut log = TranscriptLog::new();
        for seg in [
            final_at(Source::You, 6_000, 9_000, SAID),
            final_at(Source::You, 10_000, 11_000, "And I'd keep"),
            final_at(Source::You, 11_200, 12_000, "the counters in Redis."),
            // Not turns: a partial, the interviewer, an echo retraction.
            Segment {
                is_final: false,
                ..final_at(Source::You, 13_000, 13_500, "Also")
            },
            final_at(Source::Interviewer, 13_000, 14_000, "Go on."),
            final_at(Source::You, 14_000, 14_500, ""),
        ] {
            hear(&hub, &mut log, seg.clone());
            s.heard(&seg);
        }
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.queue[0].text, "And I'd keep the counters in Redis.");
        assert_eq!(
            (s.queue[0].from, s.queue[0].to),
            (Duration::from_millis(10_000), Duration::from_millis(12_000))
        );
        let after = drive(&mut s, &hub, "both played", |snap| {
            snap.speech.is_none() && tts.requests().len() == 2
        });
        let asked: Vec<String> = llm
            .requests()
            .iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(asked, [SAID, "And I'd keep the counters in Redis."]);
        let yours: Vec<(bool, bool)> = after
            .transcript
            .iter()
            .filter(|l| l.speaker == dmn_session::Speaker::You)
            .map(|l| (l.spoken_as.is_some(), l.translation.is_some()))
            .collect();
        assert_eq!(
            yours,
            [(true, true), (true, false), (true, true)],
            "the split sentence's two lines are one turn: the translation under its last"
        );
        s.finish();
    }

    /// `[speak].mode = "translate"`: the session starts in translate mode, the gate shut, and
    /// nothing is written to the transcript for it.
    #[test]
    fn the_start_up_mode_can_be_translate() {
        let hub = Arc::new(SessionHub::default());
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (mut s, _, gate) = speaking_in(&hub, &llm, &tts, VoiceMode::Translate);
        let snap = hub.snapshot();
        assert_eq!(mode_of(&hub), Some(VoiceMode::Translate));
        assert!(snap.transcript.is_empty());
        assert!(screen(&snap).contains("voice: → es"), "{}", screen(&snap));
        gate_settles(&gate, false);
        // Pass-through mode ignores finals: nothing is spoken without `t`.
        s.toggle();
        let first = final_at(Source::You, 6_000, 9_000, SAID);
        s.heard(&first);
        assert!(hub.snapshot().speech.is_none() && s.queue.is_empty());
        s.finish();
        assert!(llm.requests().is_empty());
    }

    /// What one live `t` came to (#109, #113).
    struct LiveT {
        text: String,
        translated: Duration,
        first_audio: Option<Duration>,
        done: Duration,
        /// Seconds of speech `pw-record` heard.
        heard: f64,
    }

    /// A live `t`: `voice` enrolled in `target` from `secs` seconds of `fixture` (from
    /// `from`), `cfg` with `[speak]` on that voice and target (and `edit`'s changes), the real
    /// speaker, `pw-record` on `dmn-voice` on a pipe, then each of `turns` said on the You
    /// lane and spoken with `t`, one after the other. The voice is deleted afterwards and
    /// everything else lives in a tempdir. `None` when a server is not answering or the
    /// fixture is missing.
    fn live_t(
        voice: &str,
        target: &str,
        fixture: &str,
        from: usize,
        edit: impl FnOnce(&mut Config),
        turns: &[&str],
    ) -> Option<Vec<LiveT>> {
        use std::io::Read as _;
        use std::process::{Command as Proc, Stdio};
        const TTS: &str = "http://localhost:4123";
        // Each run opens its own `dmn-voice`, and `pw-record --target dmn-voice` would pick
        // either: one live `t` at a time.
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let up = |url: &str| {
            Proc::new("curl")
                .args(["-sf", "-m", "2", url])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !up(&format!("{TTS}/health")) || !up("http://localhost:11434/api/version") {
            eprintln!("skipped: the TTS server or Ollama is not answering");
            return None;
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(fixture)
            .join("candidate.wav");
        if !fixture.exists() {
            eprintln!("skipped: {} is missing", fixture.display());
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let clip = dir.path().join(format!("{voice}.wav"));
        {
            let mut r = hound::WavReader::open(&fixture).unwrap();
            let spec = r.spec();
            let rate = spec.sample_rate as usize;
            let samples: Vec<i16> = r
                .samples::<i16>()
                .skip(from * rate)
                .take(15 * rate)
                .map(|s| s.unwrap())
                .collect();
            let mut w = hound::WavWriter::create(&clip, spec).unwrap();
            for s in samples {
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }
        struct DeleteVoice(String);
        impl Drop for DeleteVoice {
            fn drop(&mut self) {
                let _ = std::process::Command::new("curl")
                    .args(["-s", "-X", "DELETE", &format!("{TTS}/v1/voices/{}", self.0)])
                    .stdout(std::process::Stdio::null())
                    .status();
            }
        }
        let _cleanup = DeleteVoice(voice.to_owned());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        use dmn_tts::TtsProvider as _;
        rt.block_on(dmn_tts::ChatterboxProvider::new(TTS).enroll(voice, &clip, Some(target)))
            .unwrap();

        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.target = target.into();
        cfg.speak.tts_url = TTS.into();
        cfg.speak.voice_ref = clip.display().to_string();
        cfg.llm.num_ctx = 8192;
        edit(&mut cfg);
        // No pass-through: a test never captures the real microphone.
        let setup = crate::speaker::setup(&cfg, &paths, None).unwrap();
        let languages = setup.languages.clone();
        let hub = Arc::new(SessionHub::default());
        let mut s = Speaking::new(Arc::clone(&hub));
        s.attach(
            Speaker::start(setup, Arc::clone(&hub)).unwrap(),
            VoiceMode::Passthrough,
            target.into(),
            Some("en".into()),
            languages,
            None,
        );

        // The node is there before any press, for the call (here pw-record) to select.
        let node = Instant::now();
        while !String::from_utf8_lossy(&Proc::new("pw-dump").output().unwrap().stdout)
            .contains("\"node.name\": \"dmn-voice\"")
        {
            assert!(node.elapsed() < Duration::from_secs(5), "no dmn-voice");
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut rec = Proc::new("pw-record")
            .args(["--target", "dmn-voice", "--raw", "--rate", "48000"])
            .args(["--channels", "1", "--format", "f32", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = rec.stdout.take().unwrap();
        // Each loud read's time, and how many loud samples it had.
        let reader = std::thread::spawn(move || {
            let (mut loud, mut buf) = (Vec::new(), [0u8; 4096]);
            while let Ok(n) = out.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let hits = buf[..n]
                    .chunks_exact(4)
                    .filter(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs() > 0.005)
                    .count();
                if hits > 0 {
                    loud.push((Instant::now(), hits));
                }
            }
            loud
        });
        // The model and the TTS server warm up at start (#113); a press is measured warm.
        std::thread::sleep(Duration::from_secs(4));

        let mut log = TranscriptLog::new();
        let mut presses = Vec::new();
        for (n, turn) in turns.iter().enumerate() {
            let at = 6_000 + 20_000 * n as u64;
            if n > 0 {
                // The interviewer between two turns, so each press speaks only its own.
                hear(
                    &hub,
                    &mut log,
                    final_at(Source::Interviewer, at - 3_000, at - 1_000, "Go on."),
                );
            }
            hear(&hub, &mut log, final_at(Source::You, at, at + 5_000, turn));
            let pressed = Instant::now();
            s.press(&log);
            let mut translated = None;
            let done = loop {
                let snap = hub.snapshot();
                match &snap.speech {
                    Some(sp) if sp.stage == dmn_session::SpeakStage::Speaking => {
                        translated.get_or_insert_with(|| (pressed.elapsed(), sp.text.clone()));
                    }
                    Some(_) => {}
                    None => break (pressed.elapsed(), snap),
                }
                assert!(pressed.elapsed() < Duration::from_secs(120), "never done");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(done.1.speak_failed, None, "{:?}", done.1.speak_failed);
            presses.push((pressed, translated.expect("a translation"), done.0));
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = rec.kill();
        let _ = rec.wait();
        let loud = reader.join().unwrap();
        s.finish();
        let results: Vec<LiveT> = presses
            .into_iter()
            .map(|(pressed, (translated, text), done)| {
                let mine = loud
                    .iter()
                    .filter(|(t, _)| *t >= pressed && *t <= pressed + done);
                let first_audio = mine
                    .clone()
                    .next()
                    .map(|(t, _)| t.saturating_duration_since(pressed));
                let heard = mine.map(|(_, n)| n).sum::<usize>() as f64 / 48_000.0;
                LiveT {
                    text,
                    translated,
                    first_audio,
                    done,
                    heard,
                }
            })
            .collect();
        for r in &results {
            eprintln!("translation: {}", r.text);
            eprintln!(
                "key → translation {:.2} s, key → first audio {}, key → done {:.2} s, {:.1} s of speech heard by pw-record",
                r.translated.as_secs_f64(),
                r.first_audio
                    .map_or("never".into(), |d| format!("{:.2} s", d.as_secs_f64())),
                r.done.as_secs_f64(),
                r.heard
            );
        }
        Some(results)
    }

    /// #109 live, the issue's measurement: an English You line, `t`, the real model (Ollama,
    /// `qwen3.6:35b-a3b`, `think: false`) translating it into Spanish, the real TTS server
    /// (chatterbox-tts-api on :4123) speaking it in a test voice, and `pw-record` on `dmn-voice`
    /// hearing it, on a pipe. Prints key → translation, key → first audio, key → done. The test
    /// voice `dmn-test-109` is enrolled from 15 s of the Spanish fixture with `--language es`
    /// (so the start-up check passes) and deleted afterwards; everything else lives in a
    /// tempdir. Skips when either server is not answering.
    #[test]
    #[ignore = "needs PipeWire, Ollama with qwen3.6:35b-a3b, the TTS server on :4123 and the es fixture"]
    fn live_t_speaks_your_english_line_in_spanish_into_dmn_voice() {
        let Some(results) = live_t(
            "dmn-test-109",
            "es",
            "es",
            47,
            |_| {},
            &["I would start with a token bucket for each API key and keep the counters in Redis."],
        ) else {
            return;
        };
        assert!(
            results[0].first_audio.is_some(),
            "pw-record heard nothing from dmn-voice"
        );
    }

    /// #113 live, translation only: `[speak].model = "llama3.1:8b"` translates into German
    /// (the answer model is never asked for), the TTS server speaks it (on the GPU on the dev
    /// box), one sentence and then a three-sentence turn. Prints the issue's measurements:
    /// key → translation, key → first audio, key → done. The test voice `dmn-test-113` is
    /// enrolled from 15 s of the sysdesign candidate with `--language de` and deleted
    /// afterwards. Skips when either server is not answering.
    #[test]
    #[ignore = "needs PipeWire, Ollama with llama3.1:8b, the TTS server on :4123 and the sysdesign fixture"]
    fn live_t_translation_only_speaks_german_with_a_small_translator() {
        let Some(results) = live_t(
            "dmn-test-113",
            "de",
            "sysdesign",
            31,
            |cfg| cfg.speak.model = Some("llama3.1:8b".into()),
            &[
                "I would start with a token bucket for each API key and keep the counters in Redis.",
                "First I'd clarify the requirements. Then I'd put a token bucket in front of each API key. If Redis goes down, we let the requests through.",
            ],
        ) else {
            return;
        };
        for r in &results {
            assert!(r.first_audio.is_some(), "pw-record heard nothing");
            assert!(r.heard > 1.0, "{} s heard", r.heard);
        }
    }

    /// The issue's first criterion end to end on a fixture: the real pipeline replays the
    /// English sysdesign interview (whisper and all), `t` is pressed once a You final is in the
    /// transcript, and the mock model and mock TTS server see exactly one request each. Run
    /// with `just probe` on a dev machine.
    #[test]
    #[ignore = "needs the fixture WAVs (just fixtures-fetch) and the whisper model (dmn doctor --fetch small.en)"]
    fn replaying_an_english_fixture_t_speaks_the_candidates_last_turn() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sysdesign");
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            models_dir: Paths::from_xdg().unwrap().models_dir,
            ..Paths::under(dir.path())
        };
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.target = "es".into();
        // Nothing listens here: a request that slipped past the mocks would fail loudly.
        cfg.llm.base_url = "http://127.0.0.1:9".into();
        let mut pipeline = Pipeline::start(
            &cfg,
            &paths,
            &Options {
                input: Input::Files {
                    interviewer: fixture.join("interviewer.wav"),
                    candidate: fixture.join("candidate.wav"),
                },
                speed: 8.0,
                model: None,
                threads: None,
                vad_close_ms: None,
                vad: None,
                device: Some(dmn_core::config::SttDevice::Cpu),
                language: None,
                partials: Partials::Off,
                spec_literal_detector: false,
                question_source: None,
                assist: Some(false),
                no_llm: true,
                no_store: true,
                output: Output::Quiet,
                policy: Policy::AnswerAll,
                coding_solutions: None,
            },
        )
        .unwrap();
        let hub = pipeline.hub();
        let controls = pipeline.controls();
        let (llm, tts) = (spanish(), dmn_tts::MockTtsProvider::new());
        let (setup, played) = crate::speaker::setup_with(llm.clone(), tts.clone(), true);
        let languages = setup.languages.clone();
        pipeline.attach_speaker(
            Speaker::start(setup, Arc::clone(&hub)).unwrap(),
            VoiceMode::Passthrough,
            "es".into(),
            languages,
            None,
        );
        let worker = std::thread::spawn(move || {
            pipeline.run(&mut Quiet, true);
            pipeline.finish(&mut Quiet).unwrap()
        });
        // The pipeline keeps serving keys until quit: a failed assertion must still stop it, or
        // the test binary never exits.
        struct QuitOnDrop(Controls);
        impl Drop for QuitOnDrop {
            fn drop(&mut self) {
                self.0.quit();
            }
        }
        let _quit = QuitOnDrop(controls.clone());
        // Generous waits: `just probe` runs this beside other whisper-heavy tests.
        let wait = |what: &str, done: &dyn Fn(&Session) -> bool| -> Session {
            let until = Instant::now() + Duration::from_secs(120);
            loop {
                let s = hub.snapshot();
                if done(&s) {
                    return s;
                }
                assert!(Instant::now() < until, "never: {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait("a You final", &|s| {
            s.transcript
                .iter()
                .any(|l| l.speaker == dmn_session::Speaker::You && !l.text.is_empty())
        });
        controls.speak();
        let after = wait("the press played", &|s| {
            s.speech.is_none() && (!tts.requests().is_empty() || s.speak_failed.is_some())
        });
        controls.quit();
        let report = worker.join().unwrap();
        assert_eq!(after.speak_failed, None, "{:?}", after.speak_failed);
        let asked = llm.requests();
        assert_eq!(asked.len(), 1, "one translate request");
        let body = &asked[0].messages[1].content;
        assert!(!body.is_empty());
        // The session keeps 60 s of transcript; the log keeps the whole run.
        let yours: Vec<&str> = report
            .log
            .finals()
            .iter()
            .filter(|s| s.source == Source::You)
            .map(|s| s.text.trim())
            .collect();
        assert!(
            yours.join(" ").contains(body.as_str()),
            "the body is the candidate's last turn: {body:?} in {yours:?}"
        );
        let synthesized = tts.requests();
        assert_eq!(synthesized.len(), 1, "one synthesize request");
        assert_eq!(synthesized[0].text, SPANISH);
        assert!(!played.lock().unwrap().is_empty(), "playback started");
    }
}
