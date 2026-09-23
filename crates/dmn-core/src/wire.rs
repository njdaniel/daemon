//! Pipeline values → session [`Action`]s. The only place that knows both sides: `dmn-session`
//! depends on no pipeline crate, and no pipeline crate depends on `dmn-session`.

use std::time::{SystemTime, UNIX_EPOCH};

use dmn_detect::Candidate;
use dmn_llm::{FastEvent, FastOutcome, LlmError};
use dmn_session::{Action, IgnoreReason, Speaker, StarCards};
use dmn_transcript::Segment;

/// Unix epoch seconds now. Actions carry their times, so this is called by whoever dispatches,
/// never by the reducer.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn speaker(source: dmn_audio::Source) -> Speaker {
    match source {
        dmn_audio::Source::Interviewer => Speaker::Interviewer,
        dmn_audio::Source::You => Speaker::You,
        dmn_audio::Source::Unknown => Speaker::Unknown,
    }
}

/// A segment from the transcript bus, partial or final.
pub fn segment(seg: &Segment) -> Action {
    Action::Segment {
        speaker: speaker(seg.source),
        start: seg.start.as_secs_f64(),
        end: seg.end.as_secs_f64(),
        text: seg.text.clone(),
        is_final: seg.is_final,
        avg_logprob: f64::from(seg.avg_logprob),
    }
}

/// A detector candidate. `id` is the pipeline's running candidate number; the candidate's
/// `start` is the start of the final segment it was found in, which is how the store writer
/// links the question row to its segment row.
pub fn candidate(id: u64, c: &Candidate) -> Action {
    Action::Candidate {
        id,
        speaker: speaker(c.source),
        start: c.start.as_secs_f64(),
        end: c.end.as_secs_f64(),
        text: c.text.clone(),
        score: c.score,
    }
}

/// One published field of the fast phase for candidate `id`.
pub fn fast_event(id: u64, ev: &FastEvent) -> Action {
    match ev {
        FastEvent::Question { question, kind } => Action::Question {
            candidate: id,
            text: question.clone(),
            kind: kind.as_str().to_owned(),
        },
        FastEvent::Clarification { text } => Action::Clarification {
            candidate: id,
            text: text.clone(),
        },
        FastEvent::Focus { text } => Action::Focus {
            candidate: id,
            text: text.clone(),
        },
        FastEvent::ApproachLine { index, text } => Action::ApproachLine {
            candidate: id,
            index: *index,
            text: text.clone(),
        },
        FastEvent::Star(star) => Action::Star {
            candidate: id,
            cards: StarCards {
                situation: star.situation.clone(),
                task: star.task.clone(),
                action: star.action.clone(),
                result: star.result.clone(),
                story: star.story.clone(),
            },
        },
    }
}

/// How the fast phase for candidate `id` ended. `at` is unix epoch seconds.
pub fn outcome(id: u64, result: &Result<FastOutcome, LlmError>, at: i64) -> Action {
    match result {
        Ok(FastOutcome::Shown {
            suggestion,
            clarification,
            ..
        }) => Action::Answered {
            candidate: id,
            clarification: *clarification,
            // Serialising a plain struct of strings cannot fail; an empty object is the
            // harmless fallback if it ever did.
            payload: serde_json::to_string(suggestion).unwrap_or_else(|_| "{}".into()),
            at,
        },
        Ok(FastOutcome::Ignored { reason, score }) => Action::Ignored {
            candidate: id,
            reason: match reason {
                dmn_llm::IgnoreReason::NotAQuestion => IgnoreReason::NotAQuestion,
                dmn_llm::IgnoreReason::SmallTalk => IgnoreReason::SmallTalk,
            },
            score: *score,
        },
        Err(e) => Action::Failed {
            candidate: id,
            error: e.to_string(),
        },
    }
}
