//! `dmn replay`: the whole capture → VAD → whisper → transcript pipeline, fed from two WAVs
//! instead of PipeWire (docs/SPEC.md § Architecture: "every stage can be fed from a WAV file").

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dmn_audio::{AudioSource, FRAME_MS, Source, Speed, WavSource, frame_channel};
use dmn_core::{Config, Paths};
use dmn_detect::{Candidate, DetectConfig, Detector, Following, Sensitivity, candidate_channel};
use dmn_stt::{Model, SttConfig, SttStats, SttWorker, SttWorkerConfig, Transcriber, model_path};
use dmn_transcript::{Bus, DEFAULT_WINDOW, Segment, TranscriptLog, timestamp};
use dmn_vad::{VadConfig, VadWorker, WorkerStats, event_channel};

use crate::suggest::{Suggester, Tally};

/// Bounded queue from each VAD thread to its whisper worker. A live call needs only a few slots,
/// but an unpaced replay hands the VAD a whole interview in seconds, so this holds every
/// utterance of a long fixture rather than dropping them (the SPEC's drop-don't-stall rule is
/// about a live call falling behind, not about replay losing half the interview).
const EVENT_CAPACITY: usize = 512;

/// Ring frames per lane. A paced lane (`RealTime` or `Factor`) delivers frames on a wall-clock
/// schedule like a live call, so the live 2 s ring is enough and the VAD drains it under the
/// same real-time pressure a call would apply. Only a genuinely unpaced `WavSource` — `dmn
/// replay`'s `--speed` no longer produces one, but the type still allows it — needs a ring sized
/// to the file: it delivers a 12 minute file in about a second, so a 2 s ring would drop almost
/// all of it (the first run of this command lost 32 000 frames). Sizing that case's ring to the
/// file instead makes the producer wait-free and lossless, at 64 KB per second of audio: 12
/// minutes costs ~46 MB.
fn ring_frames(audio: Duration, speed: Speed) -> usize {
    match speed {
        Speed::RealTime | Speed::Factor(_) => DEFAULT_CAPACITY_FRAMES,
        Speed::Fast => {
            let frames = audio.as_millis() / u128::from(FRAME_MS);
            (frames as usize).saturating_add(DEFAULT_CAPACITY_FRAMES)
        }
    }
}

/// The live ring depth, 2 s (docs/SPEC.md § Threading rules).
const DEFAULT_CAPACITY_FRAMES: usize = dmn_audio::DEFAULT_CAPACITY_FRAMES;

pub struct Args {
    pub interviewer: PathBuf,
    pub candidate: PathBuf,
    /// Paces `WavSource` delivery at this many multiples of wall-clock time; 1.0 is a live call.
    pub speed: f32,
    pub model: Option<String>,
    pub threads: Option<usize>,
    pub partials: Partials,
    /// Print the merged transcript at the end.
    pub transcript: bool,
    /// Print one JSON object per line (segments, candidates, summary) instead of the readable
    /// transcript, for `eval/run.py`.
    pub json: bool,
    /// Score with the SPEC's cue table exactly as written, with no form gate and no vetoes.
    pub spec_literal_detector: bool,
    /// Skip the LLM fast phase (#12): transcribe and detect only.
    pub no_llm: bool,
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
    fn enabled(self, speed: Speed) -> bool {
        match self {
            Partials::Auto => !matches!(speed, Speed::Fast),
            Partials::On => true,
            Partials::Off => false,
        }
    }
}

struct Lane {
    source: Source,
    wav: WavSource,
    vad: VadWorker,
    stt: SttWorker,
}

/// Runs the detector over the finals as they arrive.
///
/// Each interviewer final is scored the moment it arrives with [`Following::UNKNOWN`], exactly as
/// a live session would, and a line that clears the bar is emitted then; that is the latency the
/// SPEC's 100 ms budget is about. A line that falls short is held until the next final: the
/// "followed by >= 1.5 s of silence or a You segment" cue can only be judged once something
/// follows, and if that point lifts it over the bar it is emitted late. Such a candidate cannot
/// arrive within 100 ms of its final by definition, so it is counted apart.
struct Detecting {
    detector: Detector,
    /// An interviewer final that did not clear the bar on its own, with when it arrived.
    pending: Option<(Segment, Instant)>,
    found: Vec<Found>,
}

/// One emitted candidate and how long after its final it came out.
struct Found {
    candidate: Candidate,
    latency: Duration,
    /// Emitted only once the next final showed the line was followed.
    late: bool,
}

impl Detecting {
    fn new(spec_literal: bool, sensitivity: Sensitivity) -> Self {
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

    /// Feed one segment; returns the candidates that became decidable because of it.
    fn feed(&mut self, seg: &Segment) -> Vec<&Found> {
        if !seg.is_final {
            return Vec::new();
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
        self.found[before..].iter().collect()
    }

    /// Score whatever is still held after the audio ends.
    fn flush(&mut self) -> Vec<&Found> {
        let before = self.found.len();
        if let Some((prev, arrived)) = self.pending.take() {
            self.score(&prev, Following::UNKNOWN, arrived, true);
        }
        self.found[before..].iter().collect()
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

    /// Slowest final-to-candidate time among candidates emitted on arrival, for the check
    /// against the 100 ms budget.
    fn slowest(&self) -> Duration {
        self.found
            .iter()
            .filter(|f| !f.late)
            .map(|f| f.latency)
            .max()
            .unwrap_or(Duration::ZERO)
    }

    fn late(&self) -> usize {
        self.found.iter().filter(|f| f.late).count()
    }
}

/// `[detect].sensitivity` from the config file, in the detector's own type (dmn-detect does not
/// depend on dmn-core).
fn sensitivity(s: dmn_core::config::Sensitivity) -> Sensitivity {
    match s {
        dmn_core::config::Sensitivity::Eager => Sensitivity::Eager,
        dmn_core::config::Sensitivity::Balanced => Sensitivity::Balanced,
        dmn_core::config::Sensitivity::Strict => Sensitivity::Strict,
    }
}

pub fn run(cfg: &Config, paths: &Paths, args: &Args) -> Result<(), Box<dyn Error>> {
    let name = args.model.clone().unwrap_or_else(|| cfg.stt.model.clone());
    let model = model_path(&paths.models_dir, Model::from_name(&name)?);
    if !model.is_file() {
        return Err(format!(
            "whisper model {} is missing; fix: dmn doctor --fetch {name}",
            model.display()
        )
        .into());
    }
    if !args.speed.is_finite() || args.speed <= 0.0 {
        return Err(format!("--speed must be a positive number, not {}", args.speed).into());
    }
    let stt_cfg = SttConfig {
        threads: args.threads.unwrap_or(usize::from(cfg.stt.threads)),
        ..SttConfig::default()
    };
    // #34: `--speed N` paces delivery at N× wall-clock (SPEC § Milestones only requires 1× or
    // faster, but the flag's own name promises real pacing at any N).
    let speed = if (args.speed - 1.0).abs() < f32::EPSILON {
        Speed::RealTime
    } else {
        Speed::Factor(args.speed)
    };

    let partials = args.partials.enabled(speed);
    let bus = Arc::new(Bus::new());
    let segments = bus.subscribe();
    let mut lanes = Vec::new();
    let mut audio = Duration::ZERO;
    for (path, source) in [
        (&args.interviewer, Source::Interviewer),
        (&args.candidate, Source::You),
    ] {
        let lane = start_lane(path, source, speed, &model, &stt_cfg, partials, &bus)?;
        audio = audio.max(lane.wav.duration());
        lanes.push(lane);
    }
    if !args.json {
        println!(
            "replay: {} + {} ({} of audio) through {} on {} threads, {}, partials {}\n",
            args.interviewer.display(),
            args.candidate.display(),
            pretty(audio),
            name,
            stt_cfg.threads,
            match speed {
                Speed::RealTime => "1× (wall clock)".to_string(),
                Speed::Factor(f) => format!("{f}× (wall clock)"),
                Speed::Fast => "unpaced".to_string(),
            },
            if partials { "on" } else { "off" },
        );
    }

    let started = Instant::now();
    let done = Arc::new(AtomicBool::new(false));
    let finisher = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut stats = Vec::new();
            let mut failures = Vec::new();
            for mut lane in lanes {
                // The file is delivered; the VAD then flushes and closes its channel, which ends
                // the whisper worker once it has drained everything. A truncated WAV or a dead
                // source thread must fail the command, not quietly shorten the transcript.
                let label = lane.source.label();
                if let Err(e) = lane.wav.wait() {
                    failures.push(format!("{label} audio: {e}"));
                }
                if let Err(e) = lane.wav.stop() {
                    failures.push(format!("{label} audio: {e}"));
                }
                let vad = match lane.vad.finish() {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!("{label} VAD: {e}"));
                        WorkerStats::default()
                    }
                };
                let stt = lane.stt.finish();
                if stt.errors > 0 {
                    failures.push(format!(
                        "{label} whisper: {} inference(s) failed",
                        stt.errors
                    ));
                }
                stats.push((lane.source, vad, stt));
            }
            done.store(true, Ordering::Release);
            (stats, failures)
        })
    };

    let mut log = TranscriptLog::new();
    let mut detecting = Detecting::new(
        args.spec_literal_detector,
        sensitivity(cfg.detect.sensitivity),
    );
    // The detector publishes on its own bounded channel, the one `dmn-session` will subscribe to
    // in #13; here the same loop drains it to print.
    let (candidates_tx, candidates_rx) = candidate_channel(dmn_detect::DEFAULT_CAPACITY);
    // #12: each candidate goes through the LLM fast phase on its own thread, which prints the
    // question + approach JSON when it is ready.
    let suggester = (!args.no_llm).then(|| Suggester::start(&cfg.llm, args.json));
    let mut dropped_candidates = 0u64;
    loop {
        match segments.recv_timeout(Duration::from_millis(50)) {
            Ok(seg) => {
                // An empty final is dmn-stt saying "this utterance was dropped"; it closes the
                // partial in the log and has nothing to print.
                if !seg.text.is_empty() {
                    if args.json {
                        print_segment_json(&seg);
                    } else {
                        println!("{}", seg.render());
                    }
                }
                for found in detecting.feed(&seg) {
                    if candidates_tx.try_send(found.candidate.clone()).is_err() {
                        dropped_candidates += 1;
                    }
                    print_candidate(found, args.json);
                }
                // The detector's channel feeds the fast phase, with the last 60 s of transcript.
                for candidate in candidates_rx.try_iter() {
                    if let Some(s) = &suggester {
                        s.submit(&candidate, log.render_window(DEFAULT_WINDOW));
                    }
                }
                log.apply(seg);
            }
            Err(_) if done.load(Ordering::Acquire) && segments.is_empty() => break,
            Err(_) => {}
        }
    }
    for found in detecting.flush() {
        print_candidate(found, args.json);
        if let Some(s) = &suggester {
            s.submit(&found.candidate, log.render_window(DEFAULT_WINDOW));
        }
    }
    let (stats, failures) = finisher.join().map_err(|_| "replay thread panicked")?;
    let wall = started.elapsed();
    // Answer whatever is still queued; the transcript is complete by now.
    let tally = suggester.map(Suggester::finish);

    if args.json {
        print_summary_json(&stats, &log, &detecting, audio, wall, bus.dropped(), tally);
        if !failures.is_empty() {
            return Err(failures.join("; ").into());
        }
        return Ok(());
    }
    if args.transcript {
        println!("\n--- transcript (finals, splits merged) ---");
        for seg in log.window(Duration::MAX) {
            println!(
                "[{}] {}: {}",
                timestamp(seg.start),
                seg.source.label(),
                seg.text
            );
        }
        println!(
            "\n--- last {} s ---\n{}",
            DEFAULT_WINDOW.as_secs(),
            log.render_window(DEFAULT_WINDOW)
        );
    }
    print_summary(&stats, &log, audio, wall, bus.dropped());
    println!(
        "detector     {} candidate(s), {} on arrival (slowest {:?}, budget 100 ms), {} once followed{}",
        detecting.found.len(),
        detecting.found.len() - detecting.late(),
        detecting.slowest(),
        detecting.late(),
        if dropped_candidates > 0 {
            format!(", {dropped_candidates} dropped by a full channel")
        } else {
            String::new()
        }
    );
    match tally {
        Some(t) => println!(
            "fast phase   {} shown, {} clarification(s), {} not shown, {} failed, {} skipped (LLM unavailable)",
            t.shown, t.clarifications, t.ignored, t.failed, t.skipped
        ),
        None => println!("fast phase   off (--no-llm)"),
    }
    if !failures.is_empty() {
        return Err(failures.join("; ").into());
    }
    Ok(())
}

/// One NDJSON line per segment, for `eval/run.py`.
fn print_segment_json(seg: &Segment) {
    println!(
        "{}",
        serde_json::json!({
            "type": "segment",
            "source": seg.source.label(),
            "start": seg.start.as_secs_f64(),
            "end": seg.end.as_secs_f64(),
            "text": seg.text,
            "is_final": seg.is_final,
            "avg_logprob": seg.avg_logprob,
        })
    );
}

fn print_candidate(found: &Found, json: bool) {
    let c = &found.candidate;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "candidate",
                "source": c.source.label(),
                "start": c.start.as_secs_f64(),
                "end": c.end.as_secs_f64(),
                "text": c.text,
                "line": c.line,
                "score": c.score,
                "cues": c.cues.iter().map(|q| q.label()).collect::<Vec<_>>(),
                "detect_us": found.latency.as_micros() as u64,
                "late": found.late,
            })
        );
    } else {
        let when = if found.late {
            " (after the next final)"
        } else {
            ""
        };
        println!("  -> candidate {}{when}", c.render());
    }
}

fn print_summary_json(
    stats: &[(Source, WorkerStats, SttStats)],
    log: &TranscriptLog,
    detecting: &Detecting,
    audio: Duration,
    wall: Duration,
    bus_dropped: u64,
    tally: Option<Tally>,
) {
    let lanes: Vec<serde_json::Value> = stats
        .iter()
        .map(|(source, vad, stt)| {
            serde_json::json!({
                "source": source.label(),
                "finals": stt.finals,
                "partials": stt.partials,
                "dropped": stt.dropped(),
                "errors": stt.errors,
                "utterances": vad.utterances,
                "overruns": vad.overruns,
                "dropped_events": vad.dropped_events,
                "inference_ms": stt.inference_ms,
                "slowest_ms": stt.slowest_ms,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "type": "summary",
            "lanes": lanes,
            "lines": log.finals().len(),
            "candidates": detecting.found.len(),
            "detect_slowest_us": detecting.slowest().as_micros() as u64,
            "candidates_late": detecting.late(),
            "audio_s": audio.as_secs_f64(),
            "wall_s": wall.as_secs_f64(),
            "bus_dropped": bus_dropped,
            "suggestions": tally.map(|t| serde_json::json!({
                "shown": t.shown,
                "clarifications": t.clarifications,
                "ignored": t.ignored,
                "failed": t.failed,
                "skipped": t.skipped,
            })),
        })
    );
}

fn start_lane(
    path: &Path,
    source: Source,
    speed: Speed,
    model: &Path,
    stt_cfg: &SttConfig,
    partials: bool,
    bus: &Arc<Bus>,
) -> Result<Lane, Box<dyn Error>> {
    let mut wav = WavSource::open(path, source, speed)?;
    let (sink, frames) = frame_channel(source, ring_frames(wav.duration(), speed));
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
    let vad = VadWorker::spawn(frames, &vad_cfg, tx)?;
    wav.start(sink)?;
    Ok(Lane {
        source,
        wav,
        vad,
        stt,
    })
}

fn print_summary(
    stats: &[(Source, WorkerStats, SttStats)],
    log: &TranscriptLog,
    audio: Duration,
    wall: Duration,
    bus_dropped: u64,
) {
    println!("\n--- summary ---");
    let mut totals = SttStats::default();
    for (source, vad, stt) in stats {
        println!(
            "{:<12} {:>3} finals, {:>4} partials ({} stale skipped), {} dropped (empty {}, low confidence {}, phrase {}), \
             whisper {} total / {} slowest, VAD {} utterances, {} overruns, {} events dropped",
            source.label(),
            stt.finals,
            stt.partials,
            stt.superseded,
            stt.dropped(),
            stt.dropped_empty,
            stt.dropped_low_confidence,
            stt.dropped_phrase,
            pretty(Duration::from_millis(stt.inference_ms)),
            pretty(Duration::from_millis(stt.slowest_ms)),
            vad.utterances,
            vad.overruns,
            vad.dropped_events,
        );
        totals.finals += stt.finals;
        totals.partials += stt.partials;
        totals.superseded += stt.superseded;
        totals.dropped_empty += stt.dropped_empty;
        totals.dropped_low_confidence += stt.dropped_low_confidence;
        totals.dropped_phrase += stt.dropped_phrase;
        totals.errors += stt.errors;
        totals.slowest_ms = totals.slowest_ms.max(stt.slowest_ms);
    }
    let rtf = wall.as_secs_f64() / audio.as_secs_f64().max(f64::EPSILON);
    println!(
        "total        {} finals, {} partials, {} dropped, {} errors; transcript holds {} lines",
        totals.finals,
        totals.partials,
        totals.dropped(),
        totals.errors,
        log.finals().len(),
    );
    if bus_dropped > 0 {
        println!("bus          {bus_dropped} segments dropped by a slow subscriber");
    }
    println!(
        "wall {} for {} of audio: {:.2}× real time (slowest single inference {})",
        pretty(wall),
        pretty(audio),
        1.0 / rtf.max(f64::EPSILON),
        pretty(Duration::from_millis(totals.slowest_ms)),
    );
}

/// `1m 23.4s` / `0.77s`.
fn pretty(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 60.0 {
        format!("{}m {:04.1}s", (s / 60.0) as u64, s % 60.0)
    } else {
        format!("{s:.2}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_uses_the_configured_sensitivity() {
        let threshold = |s| {
            Detecting::new(false, sensitivity(s))
                .detector
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
    fn durations_render_compactly() {
        assert_eq!(pretty(Duration::from_millis(770)), "0.77s");
        assert_eq!(pretty(Duration::from_secs(59)), "59.00s");
        assert_eq!(pretty(Duration::from_millis(83_400)), "1m 23.4s");
        assert_eq!(pretty(Duration::from_secs(725)), "12m 05.0s");
    }
}
