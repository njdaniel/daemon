//! `dmn replay`: the whole capture → VAD → whisper → transcript pipeline, fed from two WAVs
//! instead of PipeWire (docs/SPEC.md § Architecture: "every stage can be fed from a WAV file"),
//! printing every segment, candidate and fast-phase result as it happens.
//!
//! The pipeline itself is [`crate::pipeline`], shared with `dmn run --replay`; this module is
//! what replay prints.

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use dmn_audio::{Source, Speed};
use dmn_core::{Config, Paths};
use dmn_stt::SttStats;
use dmn_transcript::{DEFAULT_WINDOW, Segment, timestamp};
use dmn_vad::WorkerStats;

use crate::pipeline::{Found, Input, Observer, Options, Partials, Pipeline, Report};
use crate::suggest::{Output, Policy};

pub struct Args {
    pub interviewer: PathBuf,
    pub candidate: PathBuf,
    /// Paces `WavSource` delivery at this many multiples of wall-clock time; 1.0 is a live call.
    pub speed: f32,
    pub model: Option<String>,
    pub threads: Option<usize>,
    pub vad_close_ms: Option<u16>,
    /// `--vad`, over `[stt].vad` (#62).
    pub vad: Option<dmn_core::config::Vad>,
    /// `--device` (#61), over `[stt].device`.
    pub device: Option<dmn_core::config::SttDevice>,
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
    /// Keep the session in memory only: write nothing to `dmn.sqlite` (#13). The eval harness
    /// uses it so measuring the detector does not add sessions to the user's history.
    pub no_store: bool,
}

/// Prints each segment and candidate as the pipeline produces them.
struct Printer {
    json: bool,
}

impl Observer for Printer {
    fn segment(&mut self, seg: &Segment) {
        // An empty final is dmn-stt saying "this utterance was dropped"; it closes the partial
        // in the log and has nothing to print.
        if seg.text.is_empty() {
            return;
        }
        if self.json {
            print_segment_json(seg);
        } else {
            println!("{}", seg.render());
        }
    }

    fn candidate(&mut self, _id: u64, found: &Found) {
        print_candidate(found, self.json);
    }
}

pub fn run(cfg: &Config, paths: &Paths, args: &Args) -> Result<(), Box<dyn Error>> {
    let mut pipeline = Pipeline::start(
        cfg,
        paths,
        &Options {
            input: Input::Files {
                interviewer: args.interviewer.clone(),
                candidate: args.candidate.clone(),
            },
            speed: args.speed,
            model: args.model.clone(),
            threads: args.threads,
            vad_close_ms: args.vad_close_ms,
            vad: args.vad,
            device: args.device,
            partials: args.partials,
            spec_literal_detector: args.spec_literal_detector,
            no_llm: args.no_llm,
            no_store: args.no_store,
            output: Output::Print { json: args.json },
            policy: Policy::AnswerAll,
            coding_solutions: None,
        },
    )?;
    if !args.json {
        let info = pipeline.info();
        println!(
            "replay: {} + {} ({} of audio) through {} on {} threads ({}), {} VAD, {}, partials {}\n",
            args.interviewer.display(),
            args.candidate.display(),
            pretty(info.audio),
            info.model,
            info.threads,
            info.stt_device,
            info.vad,
            match info.speed {
                Speed::RealTime => "1× (wall clock)".to_string(),
                Speed::Factor(f) => format!("{f}× (wall clock)"),
                Speed::Fast => "unpaced".to_string(),
            },
            if info.partials { "on" } else { "off" },
        );
    }
    let audio = pipeline.info().audio;
    let mut printer = Printer { json: args.json };
    pipeline.run(&mut printer, false);
    let report = pipeline.finish(&mut printer)?;

    if args.json {
        print_summary_json(&report, audio);
        if !report.failures.is_empty() {
            return Err(report.failures.join("; ").into());
        }
        return Ok(());
    }
    if args.transcript {
        println!("\n--- transcript (finals, splits merged) ---");
        for seg in report.log.window(Duration::MAX) {
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
            report.log.render_window(DEFAULT_WINDOW)
        );
    }
    print_summary(&report, audio);
    print_downstream(&report, paths);
    if !report.failures.is_empty() {
        return Err(report.failures.join("; ").into());
    }
    Ok(())
}

/// The detector, fast-phase and session-log lines of the readable summary; `dmn run` prints
/// the same lines after the TUI closes.
pub fn print_downstream(report: &Report, paths: &Paths) {
    let d = &report.detecting;
    println!(
        "detector     {} candidate(s), {} on arrival (slowest {:?}, budget 100 ms), {} once followed{}",
        d.found().len(),
        d.found().len() - d.late(),
        d.slowest(),
        d.late(),
        if report.dropped_candidates > 0 {
            format!(", {} dropped by a full channel", report.dropped_candidates)
        } else {
            String::new()
        }
    );
    match report.tally {
        Some(t) => {
            println!(
                "fast phase   {} shown ({} regenerated), {} clarification(s), {} not shown, {} failed, {} skipped, {} superseded",
                t.shown,
                t.regenerated,
                t.clarifications,
                t.ignored,
                t.failed,
                t.skipped,
                t.superseded
            );
            // #54: what went into the prompts, and what retrieval cost against its 50 ms.
            println!(
                "context      {} source(s), {} passage(s); job requirements {} ({}); retrieval {}×, slowest {:.2} ms, mean {:.2} ms (budget 50 ms)",
                t.context.sources,
                t.context.passages,
                t.context.requirements,
                t.context.from.label(),
                t.retrievals,
                t.retrieval_slowest.as_secs_f64() * 1000.0,
                t.retrieval_mean().as_secs_f64() * 1000.0
            );
            // #55: the six sections, filled after each fast phase and never ahead of one.
            println!(
                "slow phase   {} filled, {} slow (deadline), {} failed, {} restart(s) for a newer question, {} not run; slowest {:.2} s (budget 20 s)",
                t.slow_done,
                t.slow_timed_out,
                t.slow_failed,
                t.slow_restarts,
                t.slow_dropped,
                t.slow_slowest.as_secs_f64()
            );
            // #57: one summary line per answered question.
            println!(
                "memory       {} summary line(s), {} from the model, {} fallback(s)",
                t.summaries,
                t.summaries - t.summary_fallbacks,
                t.summary_fallbacks
            );
        }
        None => println!("fast phase   off (--no-llm)"),
    }
    match report.session {
        Some(r) => println!(
            "session log  #{} in {}: {} segment(s), {} question(s), {} suggestion(s){}",
            r.session_id
                .map_or_else(|| "?".to_string(), |id| id.to_string()),
            paths.db_file.display(),
            r.segments,
            r.questions,
            r.suggestions,
            if r.errors + r.lagged > 0 {
                format!(", {} not stored, {} lost", r.errors, r.lagged)
            } else {
                String::new()
            }
        ),
        None => println!("session log  off (--no-store)"),
    }
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

fn print_summary_json(report: &Report, audio: Duration) {
    let lanes: Vec<serde_json::Value> = report
        .stats
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
    let d = &report.detecting;
    println!(
        "{}",
        serde_json::json!({
            "type": "summary",
            "lanes": lanes,
            "lines": report.log.finals().len(),
            "candidates": d.found().len(),
            "detect_slowest_us": d.slowest().as_micros() as u64,
            "candidates_late": d.late(),
            "audio_s": audio.as_secs_f64(),
            "wall_s": report.wall.as_secs_f64(),
            "bus_dropped": report.bus_dropped,
            "suggestions": report.tally.map(|t| serde_json::json!({
                "shown": t.shown,
                "clarifications": t.clarifications,
                "ignored": t.ignored,
                "failed": t.failed,
                "skipped": t.skipped,
                "context": {
                    "sources": t.context.sources,
                    "passages": t.context.passages,
                    "requirements": t.context.requirements,
                    "requirements_from": t.context.from.label(),
                },
                "retrievals": t.retrievals,
                "retrieval_slowest_us": t.retrieval_slowest.as_micros() as u64,
                "retrieval_mean_us": t.retrieval_mean().as_micros() as u64,
                "slow": {
                    "done": t.slow_done,
                    "timed_out": t.slow_timed_out,
                    "failed": t.slow_failed,
                    "restarts": t.slow_restarts,
                    "not_run": t.slow_dropped,
                    "slowest_ms": t.slow_slowest.as_millis() as u64,
                },
                "summaries": {
                    "lines": t.summaries,
                    "fallbacks": t.summary_fallbacks,
                },
            })),
            "session": report.session.map(|r| serde_json::json!({
                "id": r.session_id,
                "segments": r.segments,
                "questions": r.questions,
                "suggestions": r.suggestions,
                "errors": r.errors,
                "lagged": r.lagged,
            })),
        })
    );
}

/// The per-lane transcription summary.
pub fn print_summary(report: &Report, audio: Duration) {
    println!("\n--- summary ---");
    let mut totals = SttStats::default();
    let stats: &[(Source, WorkerStats, SttStats)] = &report.stats;
    for (source, vad, stt) in stats {
        println!(
            "{:<12} {:>3} finals, {:>4} partials ({} stale skipped), {} dropped (empty {}, low confidence {}, phrase {}), \
             whisper {} total / {} slowest, VAD {} frames, {} utterances, {} overruns, \
             {} events dropped",
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
            vad.frames,
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
    let rtf = report.wall.as_secs_f64() / audio.as_secs_f64().max(f64::EPSILON);
    println!(
        "total        {} finals, {} partials, {} dropped, {} errors; transcript holds {} lines",
        totals.finals,
        totals.partials,
        totals.dropped(),
        totals.errors,
        report.log.finals().len(),
    );
    if report.bus_dropped > 0 {
        println!(
            "bus          {} segments dropped by a slow subscriber",
            report.bus_dropped
        );
    }
    println!(
        "wall {} for {} of audio: {:.2}× real time (slowest single inference {})",
        pretty(report.wall),
        pretty(audio),
        1.0 / rtf.max(f64::EPSILON),
        pretty(Duration::from_millis(totals.slowest_ms)),
    );
}

/// `1m 23.4s` / `0.77s`.
pub fn pretty(d: Duration) -> String {
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
    fn durations_render_compactly() {
        assert_eq!(pretty(Duration::from_millis(770)), "0.77s");
        assert_eq!(pretty(Duration::from_secs(59)), "59.00s");
        assert_eq!(pretty(Duration::from_millis(83_400)), "1m 23.4s");
        assert_eq!(pretty(Duration::from_secs(725)), "12m 05.0s");
    }
}
