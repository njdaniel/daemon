//! Pipeline values → session [`Action`]s. The only place that knows both sides: `dmn-session`
//! depends on no pipeline crate, and no pipeline crate depends on `dmn-session`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dmn_detect::Candidate;
use dmn_llm::{
    AnthropicProvider, FastEvent, FastOutcome, LlmError, LookupError, OllamaProvider, OllamaSearch,
    OllamaSearchSettings, OllamaSettings, OpenAiProvider, Provider, SearxngSearch, SearxngSettings,
    WebSearch,
};
use dmn_session::{
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, IgnoreReason, ProviderLabel, SlowEnd,
    SlowField, Speaker, StarCards, TradeoffItem,
};

use crate::config::{self, Llm, Lookup};
use dmn_transcript::Segment;

/// Unix epoch seconds now. Actions carry their times, so this is called by whoever dispatches,
/// never by the reducer.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Session time in unix epoch seconds, advancing at the pace the audio is delivered.
///
/// A live call runs at 1×, so this is the wall clock. `dmn replay --speed N` delivers N seconds
/// of audio per second, so its clock runs N× from the session start: a suggestion stamped by it
/// lands just after its question (whose `created_at` is the session start plus the question's
/// offset into the audio), and the session ends at start + audio length, as the call did.
#[derive(Debug, Clone, Copy)]
pub struct SessionClock {
    started_at: i64,
    started: Instant,
    speed: f64,
}

impl SessionClock {
    /// Start now, at `speed` × wall clock (1.0 for a live call).
    pub fn start(speed: f64) -> Self {
        Self {
            started_at: unix_now(),
            started: Instant::now(),
            speed: if speed.is_finite() && speed > 0.0 {
                speed
            } else {
                1.0
            },
        }
    }

    /// The session start, for [`Action::Started`].
    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// Session time now.
    pub fn now(&self) -> i64 {
        self.started_at + (self.started.elapsed().as_secs_f64() * self.speed).floor() as i64
    }
}

/// Audio time now: seconds on the clock segment times use, for [`Action::Timing`] and
/// [`Action::Cpu`] (#16).
///
/// Segment times count the frames a lane's VAD has consumed (20 ms each), which is not the wall
/// clock: a live stream starts delivering some time after capture starts, and a stalled stream
/// (a suspended sink) delivers nothing for a while. The clock is therefore an anchor, "at wall
/// instant `at` the Them lane had reached audio time `audio`", that the pipeline moves
/// forward whenever that lane's frame count advances, and between anchors it runs at `speed` ×
/// the wall clock. A WAV replay anchors once, at the instant its files start. Cheap to clone; the
/// anchor is shared.
#[derive(Debug, Clone)]
pub struct AudioClock {
    anchor: Arc<Mutex<(Instant, f64)>>,
    speed: f64,
}

impl AudioClock {
    /// Audio time 0 at `zero`, advancing at `speed` × the wall clock (1.0 for a live call).
    pub fn new(zero: Instant, speed: f64) -> Self {
        Self {
            anchor: Arc::new(Mutex::new((zero, 0.0))),
            speed: if speed.is_finite() && speed > 0.0 {
                speed
            } else {
                1.0
            },
        }
    }

    /// At wall instant `at` the audio had reached `audio` seconds.
    pub fn anchor(&self, at: Instant, audio: f64) {
        let mut a = self.anchor.lock().unwrap_or_else(PoisonError::into_inner);
        *a = (at, audio);
    }

    /// Audio seconds at wall instant `at`.
    pub fn at(&self, at: Instant) -> f64 {
        let (anchored, audio) = *self.anchor.lock().unwrap_or_else(PoisonError::into_inner);
        let since = if at >= anchored {
            at.duration_since(anchored).as_secs_f64()
        } else {
            -anchored.duration_since(at).as_secs_f64()
        };
        audio + since * self.speed
    }

    /// Audio seconds now.
    pub fn now(&self) -> f64 {
        self.at(Instant::now())
    }
}

/// CPU seconds this process has used (user + system, every thread), from `/proc/self/stat`.
/// `None` where that file cannot be read (not Linux). The kernel reports it in `USER_HZ` ticks,
/// which the proc ABI fixes at 100 per second.
pub fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    cpu_seconds_from_stat(&stat)
}

/// `utime + stime` of a `/proc/<pid>/stat` line, in seconds. The command name (field 2) is in
/// parentheses and may itself hold spaces or parentheses, so fields are counted from the last
/// `)`: utime and stime are fields 14 and 15, the 12th and 13th after it.
pub fn cpu_seconds_from_stat(stat: &str) -> Option<f64> {
    const USER_HZ: f64 = 100.0;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some((utime + stime) as f64 / USER_HZ)
}

pub fn speaker(source: dmn_audio::Source) -> Speaker {
    match source {
        dmn_audio::Source::Them => Speaker::Them,
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
        language: seg.language.clone(),
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
        FastEvent::Lookup { query } => Action::Lookup {
            candidate: id,
            query: query.clone(),
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

/// The session's coding-solutions mode as the slow phase takes it (#50, #55).
pub fn coding_mode(mode: CodingSolutions) -> dmn_llm::CodingMode {
    match mode {
        CodingSolutions::Hints => dmn_llm::CodingMode::Hints,
        CodingSolutions::Full => dmn_llm::CodingMode::Full,
    }
}

fn code_block(e: &dmn_llm::schema::Example) -> CodeBlock {
    CodeBlock {
        language: e.language.clone(),
        code: e.code.clone(),
    }
}

/// One slow-phase field for candidate `id` (#55), or `None` for the summary and promise (#57):
/// those are about the previous question and become one [`Action::Summary`] when the phase ends.
pub fn slow_field(id: u64, field: &dmn_llm::SlowField) -> Option<Action> {
    use dmn_llm::SlowField as L;
    let field = match field {
        L::Deeper(items) => SlowField::Deeper {
            items: items
                .iter()
                .map(|d| DeeperItem {
                    label: d.label.clone(),
                    text: d.text.clone(),
                })
                .collect(),
        },
        L::Followups(items) => SlowField::Followups {
            items: items.clone(),
        },
        L::Tradeoffs(items) => SlowField::Tradeoffs {
            items: items
                .iter()
                .map(|t| TradeoffItem {
                    option: t.option.clone(),
                    pro: t.pro.clone(),
                    con: t.con.clone(),
                })
                .collect(),
        },
        L::Example(code) => SlowField::Example {
            code: code.as_ref().map(code_block),
        },
        L::Mention(items) => SlowField::Mention {
            items: items.clone(),
        },
        L::ResumeContext(items) => SlowField::ResumeContext {
            items: items
                .iter()
                .map(|r| Citation {
                    source: r.source.clone(),
                    text: r.text.clone(),
                })
                .collect(),
        },
        L::Solution(code) => SlowField::Solution {
            code: code_block(code),
        },
        L::Summary(_) | L::Promise(_) => return None,
    };
    Some(Action::Slow {
        candidate: id,
        field,
    })
}

/// How candidate `id`'s slow phase ended, with the fields that arrived (for the store). `stopped`
/// says the run itself was stopped (`q`), which is not a failure. `at` is unix epoch seconds.
pub fn slow_ended(
    id: u64,
    result: &Result<dmn_llm::SlowOutcome, LlmError>,
    fields: &dmn_llm::SlowFields,
    stopped: bool,
    at: i64,
) -> Action {
    let end = match result {
        Ok(_) => SlowEnd::Done,
        Err(LlmError::Timeout(_)) => SlowEnd::TimedOut,
        Err(LlmError::Cancelled) if stopped => SlowEnd::Stopped,
        Err(e) => SlowEnd::Failed(e.to_string()),
    };
    Action::SlowEnded {
        candidate: id,
        end,
        payload: fields.to_json().to_string(),
        at,
    }
}

/// The provider `[llm]` names (#59), with the API key read now from the env var
/// `api_key_env` names. The key goes into the provider and nowhere else: not logged, not in any
/// `Debug` output, not in the label.
pub fn provider(llm: &Llm) -> Result<Arc<dyn Provider>, LlmError> {
    let key = || llm.api_key().map(|k| k.expose().to_owned());
    Ok(match llm.provider {
        config::Provider::Ollama => Arc::new(OllamaProvider::from_settings(&OllamaSettings {
            base_url: llm.endpoint(),
            model: llm.model.clone(),
            // #61: a context window smaller than the server's frees VRAM for GPU whisper.
            num_ctx: (llm.num_ctx > 0).then_some(llm.num_ctx),
            ..OllamaSettings::default()
        })?),
        config::Provider::OpenAI => Arc::new(OpenAiProvider::new(
            llm.endpoint(),
            llm.model.clone(),
            key(),
        )?),
        config::Provider::Anthropic => Arc::new(AnthropicProvider::new(
            llm.endpoint(),
            llm.model.clone(),
            key(),
        )?),
    })
}

/// The web search `[lookup]` names (#102), or `None` while it is off. The key is read now from
/// the variable `api_key_env` names and goes into the client and nowhere else; unset, the client
/// is still built, and every lookup fails at once with [`LookupError::NoKey`] without sending
/// anything, which the status line reports as `lookup failed`. `searxng` needs no key.
pub fn web_search(lookup: &Lookup) -> Result<Option<Arc<dyn WebSearch>>, LookupError> {
    if !lookup.enabled {
        return Ok(None);
    }
    let max_results = usize::from(lookup.max_results);
    let timeout = lookup.timeout();
    let search: Arc<dyn WebSearch> = match lookup.provider {
        config::LookupProvider::Ollama => Arc::new(OllamaSearch::new(&OllamaSearchSettings {
            base_url: lookup.endpoint(),
            api_key: lookup.api_key().map(|k| k.expose().to_owned()),
            key_env: lookup.api_key_env.clone(),
            max_results,
            timeout,
        })?),
        // On PR #104: the user's own instance; no key, so `api_key_env` is not read.
        config::LookupProvider::Searxng => Arc::new(SearxngSearch::new(&SearxngSettings {
            base_url: lookup.endpoint(),
            max_results,
            timeout,
        })?),
    };
    Ok(Some(search))
}

/// What the model dot shows for `llm` (#59).
pub fn provider_label(llm: &Llm) -> ProviderLabel {
    ProviderLabel {
        name: llm.provider.as_str().to_owned(),
        model: llm.model.clone(),
        remote: llm.leaves_machine(),
    }
}

/// The `[llm]` fields that decide which provider answers: a change to any of them swaps the
/// provider on reload; a change elsewhere in the file swaps nothing.
pub fn same_provider(a: &Llm, b: &Llm) -> bool {
    a.provider == b.provider
        && a.endpoint() == b.endpoint()
        && a.model == b.model
        && a.api_key_env == b.api_key_env
        && a.num_ctx == b.num_ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_fields_map_onto_session_actions() {
        let example = dmn_llm::schema::Example {
            language: "python".into(),
            code: "x = 1".into(),
        };
        let a = slow_field(3, &dmn_llm::SlowField::Solution(example.clone()));
        assert_eq!(
            a,
            Some(Action::Slow {
                candidate: 3,
                field: SlowField::Solution {
                    code: CodeBlock {
                        language: "python".into(),
                        code: "x = 1".into(),
                    }
                }
            })
        );
        assert_eq!(
            slow_field(3, &dmn_llm::SlowField::Summary("x".into())),
            None,
            "a summary is about the previous question, not a section of this one"
        );
        let mut fields = dmn_llm::SlowFields::default();
        fields.apply(&dmn_llm::SlowField::Example(Some(example)));
        let timed_out = slow_ended(
            3,
            &Err(LlmError::Timeout(std::time::Duration::from_secs(20))),
            &fields,
            false,
            100,
        );
        let Action::SlowEnded { end, payload, .. } = timed_out else {
            panic!("{timed_out:?}");
        };
        assert_eq!(end, SlowEnd::TimedOut);
        assert!(payload.contains("\"example\""), "{payload}");
        let stopped = slow_ended(3, &Err(LlmError::Cancelled), &fields, true, 100);
        assert!(matches!(
            stopped,
            Action::SlowEnded {
                end: SlowEnd::Stopped,
                ..
            }
        ));
        assert_eq!(
            coding_mode(CodingSolutions::Full),
            dmn_llm::CodingMode::Full
        );
    }

    #[test]
    fn the_session_clock_runs_at_the_replay_speed() {
        let clock = SessionClock::start(3600.0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let ahead = clock.now() - clock.started_at();
        // 20 ms at 3600× is 72 s of session time; allow for a slow scheduler.
        assert!((72..=3600).contains(&ahead), "{ahead}");

        let live = SessionClock::start(1.0);
        assert!((live.now() - unix_now()).abs() <= 1, "1× is the wall clock");
        assert_eq!(SessionClock::start(f64::NAN).speed, 1.0);
    }

    #[test]
    fn the_audio_clock_runs_from_its_latest_anchor() {
        use std::time::Duration;
        let zero = Instant::now();
        let clock = AudioClock::new(zero, 2.0);
        let t = |ms| zero + Duration::from_millis(ms);
        assert!((clock.at(t(500)) - 1.0).abs() < 1e-9, "2× from zero");
        // The stream stalled: at 1.5 s wall only 1.2 s of audio had arrived.
        clock.anchor(t(1500), 1.2);
        assert!((clock.at(t(1500)) - 1.2).abs() < 1e-9);
        assert!((clock.at(t(2000)) - 2.2).abs() < 1e-9, "then 2× again");
        assert!((clock.at(t(1000)) - 0.2).abs() < 1e-9, "before the anchor");
        let shared = clock.clone();
        shared.anchor(t(3000), 3.0);
        assert!(
            (clock.at(t(3000)) - 3.0).abs() < 1e-9,
            "clones share the anchor"
        );
        assert!((AudioClock::new(zero, f64::NAN).at(t(1000)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cpu_seconds_come_from_utime_and_stime() {
        // A command name with spaces and a parenthesis, as `/proc` allows.
        let stat = "4242 (dmn (x) y) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0";
        assert_eq!(cpu_seconds_from_stat(stat), Some(3.0));
        assert_eq!(cpu_seconds_from_stat("garbage"), None);
        assert!(process_cpu_seconds().is_some_and(|s| s >= 0.0));
    }
}
