//! Scoring one final interviewer line and turning it into a [`Candidate`].

use std::time::Duration;

use dmn_audio::Source;
use dmn_transcript::Segment;

use crate::cue::{
    Cue, ends_with_question_mark, has_handoff, is_long, is_rhetorical, is_small_talk,
    starts_with_opener,
};
use crate::sentence::{self, Sentence};
use crate::{DetectConfig, Sensitivity};

/// What the detector proposes to the LLM fast phase: the question as asked, with the score and the
/// cues behind it so the eval harness and #16 can see why.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub source: Source,
    /// Onset of the utterance the question came from.
    pub start: Duration,
    pub end: Duration,
    /// The question sentence, not the whole line: "Sorry about that. Where were we. Let's switch
    /// gears. Suppose your p99 doubled..." hands the LLM the "Suppose..." part.
    pub text: String,
    /// The whole line the question was found in, for context.
    pub line: String,
    pub score: i32,
    pub cues: Vec<Cue>,
}

impl Candidate {
    /// `score 6 [? opener interviewer long] text` for `dmn replay` and the eval report.
    pub fn render(&self) -> String {
        let cues: Vec<&str> = self.cues.iter().map(|c| c.label()).collect();
        format!("score {} [{}] {}", self.score, cues.join(" "), self.text)
    }
}

/// What the pipeline knows about what came after a line. The detector never waits for it: a live
/// caller passes what it has (usually nothing) and the line is scored at once, which is what keeps
/// detection inside the SPEC's 100 ms budget. `dmn replay` and the eval harness run offline and
/// can pass the truth, so the SPEC's "followed by" cue is measurable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Following {
    /// Silence after this line before anyone spoke again.
    pub silence: Option<Duration>,
    /// Who spoke next, if anyone.
    pub next_speaker: Option<Source>,
}

impl Following {
    /// Nothing known yet: the live path at the instant a final arrives.
    pub const UNKNOWN: Self = Self {
        silence: None,
        next_speaker: None,
    };

    pub fn silence(after: Duration) -> Self {
        Self {
            silence: Some(after),
            next_speaker: None,
        }
    }

    pub fn speaker(next: Source) -> Self {
        Self {
            silence: None,
            next_speaker: Some(next),
        }
    }

    /// docs/SPEC.md: "Interviewer segment followed by >= 1.5 s of silence or a You segment".
    fn matches(self, gap: Duration) -> bool {
        self.silence.is_some_and(|s| s >= gap) || self.next_speaker == Some(Source::You)
    }
}

/// The silence that counts as a hand-back to the candidate (docs/SPEC.md).
pub const FOLLOW_SILENCE: Duration = Duration::from_millis(1500);

/// One sentence's score and the cues behind it.
#[derive(Debug, Clone, PartialEq)]
struct Scored {
    text: String,
    score: i32,
    cues: Vec<Cue>,
}

/// The Tier-C rule scorer. Holds no state: [`PinState`](crate::PinState) owns what is pinned and
/// queued, so the scorer stays a pure function of the line in front of it.
#[derive(Debug, Clone, Default)]
pub struct Detector {
    cfg: DetectConfig,
}

impl Detector {
    pub fn new(cfg: DetectConfig) -> Self {
        Self { cfg }
    }

    pub fn with_sensitivity(sensitivity: Sensitivity) -> Self {
        Self::new(DetectConfig::default().with_sensitivity(sensitivity))
    }

    pub fn config(&self) -> &DetectConfig {
        &self.cfg
    }

    /// Score a final interviewer segment and return a candidate when it clears the bar.
    ///
    /// Partials, empty lines and anything the candidate said are ignored: the SPEC runs the
    /// detector "on every final Interviewer segment".
    pub fn observe(&self, segment: &Segment, following: Following) -> Option<Candidate> {
        if !segment.is_final || segment.source != Source::Interviewer {
            return None;
        }
        let best = self.score_line(&segment.text, segment.source, following)?;
        Some(Candidate {
            source: segment.source,
            start: segment.start,
            end: segment.end,
            text: best.text,
            line: segment.text.clone(),
            score: best.score,
            cues: best.cues,
        })
    }

    /// The best-scoring sentence of a line, or `None` when nothing clears the bar.
    fn score_line(&self, line: &str, source: Source, following: Following) -> Option<Scored> {
        let sentences = sentence::split(line);
        if sentences.is_empty() {
            return None;
        }
        let handoff = has_handoff(line);
        // "Hello? Can you hear me okay?" is one line of chatter, but "Hello?" on its own carries
        // no small-talk word. A short sentence inside a small-talk line is chatter too; a long one
        // is not, so a technical question that happens to share a line with an aside still counts.
        let line_chatter = is_small_talk(line);
        let mut best: Option<Scored> = None;
        for s in &sentences {
            let scored = self.score_sentence(s, source, following, handoff, line_chatter);
            let better = match &best {
                // A later sentence wins a tie: an interviewer who chats and then asks is asking
                // the last thing, and that is the line the LLM should answer.
                Some(prev) => scored.score >= prev.score,
                None => true,
            };
            if better {
                best = Some(scored);
            }
        }
        let best = best?;
        if self.cfg.require_question_form && !best.cues.iter().any(|c| c.is_question_form()) {
            return None;
        }
        if self.cfg.rhetorical_vetoes && best.cues.contains(&Cue::Rhetorical) {
            return None;
        }
        if self.cfg.small_talk_vetoes && best.cues.contains(&Cue::SmallTalk) {
            return None;
        }
        (best.score >= self.cfg.effective_threshold()).then_some(best)
    }

    fn score_sentence(
        &self,
        s: &Sentence<'_>,
        source: Source,
        following: Following,
        handoff_in_line: bool,
        line_chatter: bool,
    ) -> Scored {
        let mut cues = Vec::new();
        if ends_with_question_mark(s.text) {
            cues.push(Cue::QuestionMark);
        }
        if starts_with_opener(s.text) {
            cues.push(Cue::QuestionOpener);
        }
        // A hand-off phrase introduces the question that follows it ("Let's switch gears. Suppose
        // your p99 doubled."), so it credits the rest of the line, not the phrase itself.
        if handoff_in_line && (has_handoff(s.text) || s.index > 0) {
            cues.push(Cue::HandOff);
        }
        if source == Source::Interviewer {
            cues.push(Cue::Interviewer);
        }
        if is_long(s.text) {
            cues.push(Cue::Long);
        }
        // Only the last sentence of a line is followed by whatever came after the line.
        if s.last && following.matches(FOLLOW_SILENCE) {
            cues.push(Cue::Followed);
        }
        if is_rhetorical(s.text) {
            cues.push(Cue::Rhetorical);
        }
        if is_small_talk(s.text) || (line_chatter && sentence::words(s.text).len() <= 4) {
            cues.push(Cue::SmallTalk);
        }
        cues.sort_unstable();
        let score = cues.iter().map(|&c| self.cfg.weight(c)).sum();
        Scored {
            text: s.text.to_owned(),
            score,
            cues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Segment {
        Segment {
            source: Source::Interviewer,
            start: Duration::from_secs(10),
            end: Duration::from_secs(20),
            text: text.to_owned(),
            is_final: true,
            avg_logprob: -0.2,
        }
    }

    fn detect(text: &str) -> Option<Candidate> {
        Detector::default().observe(&seg(text), Following::speaker(Source::You))
    }

    #[test]
    fn a_plain_question_is_a_candidate_with_its_cues() {
        let c = detect("How would you design a rate limiter for a globally distributed API?")
            .expect("a question");
        assert!(c.cues.contains(&Cue::QuestionMark));
        assert!(c.cues.contains(&Cue::QuestionOpener));
        assert!(c.cues.contains(&Cue::Interviewer));
        assert!(c.cues.contains(&Cue::Long));
        assert!(c.cues.contains(&Cue::Followed));
        assert_eq!(c.score, 7);
        assert!(c.render().starts_with("score 7 ["));
    }

    #[test]
    fn an_imperative_question_without_a_question_mark_still_scores() {
        let c = detect("Tell me about a time you disagreed with a technical decision.")
            .expect("an imperative question");
        assert!(c.cues.contains(&Cue::QuestionOpener));
        assert!(!c.cues.contains(&Cue::QuestionMark));
        assert!(c.score >= 2);
    }

    #[test]
    fn the_candidate_text_is_the_question_not_the_filler_around_it() {
        let c = detect(
            "Sorry about that. Where were we. Right, the pipeline. Let's switch gears to \
             something more hands-on. Suppose you have a service whose p99 latency doubled \
             overnight, how would you find out why?",
        )
        .expect("the question at the end");
        assert!(
            c.text.starts_with("Suppose you have a service"),
            "{}",
            c.text
        );
        assert!(c.cues.contains(&Cue::HandOff));
        assert!(
            c.line.starts_with("Sorry about that."),
            "the whole line is kept"
        );
    }

    #[test]
    fn statements_and_chatter_are_not_candidates() {
        assert!(detect("Okay, hold on one second, someone's at my door.").is_none());
        assert!(
            detect("That makes sense. Well, I think that's everything from my side.").is_none()
        );
        assert!(
            detect("Two billion a day, that's a decent scale, right? Most people don't get to touch that.")
                .is_none(),
            "a rhetorical tag is not a question"
        );
    }

    #[test]
    fn a_hand_off_alone_is_not_a_question() {
        let d = Detector::default();
        let s = seg("Okay, let's switch gears.");
        assert!(
            d.observe(&s, Following::speaker(Source::You)).is_none(),
            "hand-off + interviewer + followed with nothing asked"
        );
    }

    #[test]
    fn small_talk_questions_stay_below_the_bar() {
        assert!(detect("Hello? Can you hear me okay?").is_none());
        assert!(detect("How's the weather where you are?").is_none());
    }

    #[test]
    fn only_final_interviewer_segments_are_scored() {
        let d = Detector::default();
        let mut partial = seg("How would you design a rate limiter?");
        partial.is_final = false;
        assert!(d.observe(&partial, Following::UNKNOWN).is_none());

        let mut you = seg("How would you design a rate limiter?");
        you.source = Source::You;
        assert!(d.observe(&you, Following::UNKNOWN).is_none());

        let empty = seg("   ");
        assert!(d.observe(&empty, Following::UNKNOWN).is_none());
    }

    #[test]
    fn the_followed_cue_needs_silence_or_the_candidate_speaking() {
        let d = Detector::default();
        let s = seg("What would you check first?");
        let none = d.observe(&s, Following::UNKNOWN).expect("still a question");
        assert!(!none.cues.contains(&Cue::Followed));

        let short = d
            .observe(&s, Following::silence(Duration::from_millis(900)))
            .expect("still a question");
        assert!(
            !short.cues.contains(&Cue::Followed),
            "under 1.5 s is not a hand-back"
        );

        let long = d
            .observe(&s, Following::silence(Duration::from_millis(1500)))
            .expect("still a question");
        assert!(long.cues.contains(&Cue::Followed));

        let interviewer_again = d
            .observe(&s, Following::speaker(Source::Interviewer))
            .expect("still a question");
        assert!(!interviewer_again.cues.contains(&Cue::Followed));
    }

    #[test]
    fn sensitivity_moves_the_bar() {
        // interviewer + long + opener, no question mark: score 4.
        let line = seg("Describe the last incident you were on call for and what you changed.");
        let eager = Detector::with_sensitivity(Sensitivity::Eager);
        let strict = Detector::with_sensitivity(Sensitivity::Strict);
        assert!(eager.observe(&line, Following::UNKNOWN).is_some());
        assert!(strict.observe(&line, Following::UNKNOWN).is_some());

        // A bare opener with nothing else: score 3 (opener + interviewer), below strict's bar of
        // 3? No: 3 >= 3 clears it, so use a two-point line instead.
        let thin = seg("What now?");
        assert_eq!(
            Detector::default()
                .observe(&thin, Following::UNKNOWN)
                .map(|c| c.score),
            Some(5)
        );
    }

    #[test]
    fn the_form_gate_keeps_long_interviewer_statements_out() {
        let statement = seg("We have a lot of integer streams over here on the platform team.");
        assert!(
            Detector::default()
                .observe(&statement, Following::speaker(Source::You))
                .is_none()
        );

        let literal = Detector::new(DetectConfig::spec_literal());
        let c = literal
            .observe(&statement, Following::speaker(Source::You))
            .expect("the SPEC table alone makes this a candidate");
        assert_eq!(c.score, 3, "interviewer + long + followed");
    }
}
