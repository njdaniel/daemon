//! `dmn replay`: the whole capture → VAD → whisper → transcript pipeline, fed from two WAVs
//! instead of PipeWire (docs/SPEC.md § Architecture: "every stage can be fed from a WAV file").

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dmn_audio::{AudioSource, FRAME_MS, Source, Speed, WavSource, frame_channel};
use dmn_core::{Config, Paths};
use dmn_stt::{Model, SttConfig, SttStats, SttWorker, SttWorkerConfig, Transcriber, model_path};
use dmn_transcript::{Bus, DEFAULT_WINDOW, Segment, TranscriptLog, timestamp};
use dmn_vad::{VadConfig, VadWorker, WorkerStats, event_channel};

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
    loop {
        match segments.recv_timeout(Duration::from_millis(50)) {
            Ok(seg) => {
                // An empty final is dmn-stt saying "this utterance was dropped"; it closes the
                // partial in the log and has nothing to print.
                if !seg.text.is_empty() {
                    println!("{}", seg.render());
                }
                log.apply(seg);
            }
            Err(_) if done.load(Ordering::Acquire) && segments.is_empty() => break,
            Err(_) => {}
        }
    }
    let (stats, failures) = finisher.join().map_err(|_| "replay thread panicked")?;
    let wall = started.elapsed();

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
    if !failures.is_empty() {
        return Err(failures.join("; ").into());
    }
    Ok(())
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
