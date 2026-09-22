//! The transcript itself: finals in order, the open partial per source, and the recent window
//! the prompt carries (docs/SPEC.md § Session memory: "the last 60 s of raw transcript").

use std::collections::HashMap;
use std::time::Duration;

use dmn_audio::Source;

use crate::segment::{Segment, merge_adjacent};

/// What [`TranscriptLog::apply`] did with a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// First partial of an utterance.
    OpenedPartial,
    /// Replaced the previous partial of the same utterance.
    UpdatedPartial,
    /// A final that replaced the utterance's partial, if any.
    Finalised { replaced_partial: bool },
}

/// The SPEC's prompt window.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct TranscriptLog {
    finals: Vec<Segment>,
    /// The still-open utterance per source, keyed by source; its `start` matches its finals'.
    partials: HashMap<Source, Segment>,
}

impl TranscriptLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one segment in. A final replaces the partial of the same utterance, so the log never
    /// holds both (docs/SPEC.md § STT worker: "the final pass replaces all partials").
    pub fn apply(&mut self, segment: Segment) -> Applied {
        if !segment.is_final {
            let first = self.partials.insert(segment.source, segment).is_none();
            return if first {
                Applied::OpenedPartial
            } else {
                Applied::UpdatedPartial
            };
        }
        let replaced_partial = match self.partials.get(&segment.source) {
            Some(p) if p.start == segment.start => {
                self.partials.remove(&segment.source);
                true
            }
            // A final for a different utterance means we never saw this one's partial close;
            // the stale partial is dropped either way.
            Some(_) => {
                self.partials.remove(&segment.source);
                false
            }
            None => false,
        };
        let at = self.finals.partition_point(|s| {
            (s.start, s.source.label()) <= (segment.start, segment.source.label())
        });
        self.finals.insert(at, segment);
        Applied::Finalised { replaced_partial }
    }

    /// Every final, oldest first.
    pub fn finals(&self) -> &[Segment] {
        &self.finals
    }

    /// The open partial for each source, if any.
    pub fn open_partials(&self) -> Vec<&Segment> {
        let mut v: Vec<&Segment> = self.partials.values().collect();
        v.sort_by_key(|s| s.start);
        v
    }

    /// Stream time covered so far.
    pub fn position(&self) -> Duration {
        let finals = self.finals.last().map(|s| s.end).unwrap_or_default();
        self.partials
            .values()
            .map(|s| s.end)
            .chain(std::iter::once(finals))
            .max()
            .unwrap_or_default()
    }

    /// Finals from the last `window` of stream time, forced splits merged. This is what the LLM
    /// prompt carries; partials are left out so the model never sees text that is about to
    /// change.
    pub fn window(&self, window: Duration) -> Vec<Segment> {
        let cutoff = self.position().saturating_sub(window);
        let recent: Vec<Segment> = self
            .finals
            .iter()
            .filter(|s| s.end >= cutoff)
            .cloned()
            .collect();
        merge_adjacent(&recent)
    }

    /// The window as labelled lines, oldest first.
    pub fn render_window(&self, window: Duration) -> String {
        self.window(window)
            .iter()
            .map(|s| format!("{}: {}", s.source.label(), s.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(source: Source, start_ms: u64, end_ms: u64, text: &str, is_final: bool) -> Segment {
        Segment {
            source,
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.to_owned(),
            is_final,
            avg_logprob: -0.2,
        }
    }

    #[test]
    fn the_final_replaces_every_partial_of_its_utterance() {
        let mut log = TranscriptLog::new();
        assert_eq!(
            log.apply(seg(Source::Interviewer, 1_000, 2_000, "how would", false)),
            Applied::OpenedPartial
        );
        assert_eq!(
            log.apply(seg(
                Source::Interviewer,
                1_000,
                3_000,
                "how would you design",
                false
            )),
            Applied::UpdatedPartial
        );
        assert_eq!(log.open_partials().len(), 1, "one open utterance, not two");
        assert_eq!(
            log.apply(seg(
                Source::Interviewer,
                1_000,
                4_000,
                "How would you design a rate limiter?",
                true
            )),
            Applied::Finalised {
                replaced_partial: true
            }
        );
        assert!(log.open_partials().is_empty());
        assert_eq!(log.finals().len(), 1, "no duplicate text in the log");
        assert_eq!(log.finals()[0].text, "How would you design a rate limiter?");
    }

    #[test]
    fn each_source_has_its_own_open_partial() {
        let mut log = TranscriptLog::new();
        log.apply(seg(Source::Interviewer, 0, 1_000, "and how", false));
        log.apply(seg(Source::You, 500, 1_500, "so the first", false));
        assert_eq!(log.open_partials().len(), 2);
        log.apply(seg(
            Source::Interviewer,
            0,
            2_000,
            "And how would you test it?",
            true,
        ));
        let open = log.open_partials();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].source, Source::You);
    }

    #[test]
    fn a_stale_partial_is_dropped_when_a_later_utterance_finalises() {
        let mut log = TranscriptLog::new();
        log.apply(seg(Source::You, 0, 1_000, "um", false));
        // Its final never arrived (whisper dropped it as a hallucination); the next one lands.
        assert_eq!(
            log.apply(seg(Source::You, 5_000, 7_000, "Yes, exactly.", true)),
            Applied::Finalised {
                replaced_partial: false
            }
        );
        assert!(log.open_partials().is_empty());
    }

    #[test]
    fn the_window_keeps_the_last_sixty_seconds_and_merges_splits() {
        let mut log = TranscriptLog::new();
        log.apply(seg(Source::Interviewer, 0, 2_000, "Old question?", true));
        log.apply(seg(
            Source::You,
            70_000,
            82_000,
            "so I would start by",
            true,
        ));
        log.apply(seg(
            Source::You,
            82_020,
            90_000,
            "sharding the counters.",
            true,
        ));
        log.apply(seg(
            Source::Interviewer,
            91_000,
            93_000,
            "And if Redis dies?",
            true,
        ));

        let w = log.window(DEFAULT_WINDOW);
        assert_eq!(w.len(), 2, "the 0 s question is outside the window: {w:?}");
        assert_eq!(w[0].text, "so I would start by sharding the counters.");
        assert_eq!(
            log.render_window(DEFAULT_WINDOW),
            "You: so I would start by sharding the counters.\nInterviewer: And if Redis dies?"
        );
    }

    #[test]
    fn position_follows_partials_as_well_as_finals() {
        let mut log = TranscriptLog::new();
        assert_eq!(log.position(), Duration::ZERO);
        log.apply(seg(Source::You, 0, 1_000, "one", true));
        assert_eq!(log.position(), Duration::from_millis(1_000));
        log.apply(seg(Source::You, 2_000, 3_500, "two", false));
        assert_eq!(log.position(), Duration::from_millis(3_500));
    }
}
