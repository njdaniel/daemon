//! Weights, threshold and sensitivity. Every number the detector uses lives here so #16 can
//! retune on a real call without touching a matcher.

use dmn_audio::Source;

use crate::cue::Cue;

/// Whose questions the detector scores (`[detect].question_source` in config.toml, #90). It
/// selects eligible lanes, not voice identities: the mic is always labelled You and the call's
/// output always Them, whoever is actually speaking on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuestionSource {
    /// The call's output lane only: the behaviour before #90.
    #[default]
    Them,
    /// The mic lane only. Lets the user ask Daemon directly, and lets a mock interview played
    /// from a phone into the microphone drive a whole session.
    You,
    /// Either lane.
    Both,
}

impl QuestionSource {
    /// Whether a final from `source` is scored at all.
    pub fn allows(self, source: Source) -> bool {
        match self {
            QuestionSource::Them => source == Source::Them,
            QuestionSource::You => source == Source::You,
            QuestionSource::Both => matches!(source, Source::Them | Source::You),
        }
    }

    /// Parse `[detect].question_source`. As with [`Sensitivity::from_name`], `dmn-core` owns the
    /// config type and this keeps the crates apart.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            // `interviewer` is the name before #128.
            "them" | "interviewer" => Some(QuestionSource::Them),
            "you" => Some(QuestionSource::You),
            "both" => Some(QuestionSource::Both),
            _ => None,
        }
    }

    /// The config-file spelling, for the status line and the replay header.
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionSource::Them => "them",
            QuestionSource::You => "you",
            QuestionSource::Both => "both",
        }
    }
}

/// How eager the detector is (`[detect].sensitivity` in config.toml). The sensitivity shifts the
/// threshold, not the weights: the cues mean the same thing at every setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sensitivity {
    /// Threshold one point lower: more candidates, more false positives.
    Eager,
    #[default]
    Balanced,
    /// Threshold one point higher: only well-corroborated questions.
    Strict,
}

impl Sensitivity {
    /// Points added to the base threshold.
    pub fn offset(self) -> i32 {
        match self {
            Sensitivity::Eager => -1,
            Sensitivity::Balanced => 0,
            Sensitivity::Strict => 1,
        }
    }

    /// Parse `[detect].sensitivity`. `dmn-core` owns the config type; this keeps the crates apart
    /// (docs/SPEC.md § Architecture: `dmn-core` depends on the stages, never the other way).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "eager" => Some(Sensitivity::Eager),
            "balanced" => Some(Sensitivity::Balanced),
            "strict" => Some(Sensitivity::Strict),
            _ => None,
        }
    }
}

/// Points per cue and the bar a line must clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectConfig {
    pub question_mark: i32,
    pub question_opener: i32,
    pub hand_off: i32,
    pub them: i32,
    pub long: i32,
    pub followed: i32,
    pub rhetorical: i32,
    pub small_talk: i32,
    /// A line is a candidate at or above this score.
    pub threshold: i32,
    pub sensitivity: Sensitivity,
    /// Whose finals are scored (#90). Default [`QuestionSource::Them`], the SPEC's
    /// "every final Them segment".
    pub question_source: QuestionSource,
    /// Require at least one question-form cue (`?`, an opener, or a hand-off phrase) before a line
    /// can be a candidate, whatever its score.
    ///
    /// Without this, the corroborating cues alone reach the threshold: any Them line over
    /// six words that the candidate answers scores them + long + followed = 3, so "Okay,
    /// hold on one second, someone's at my door." is a candidate. See docs/SPEC.md § Question
    /// detector and the amendment this behaviour is recorded under.
    pub require_question_form: bool,
    /// Treat a rhetorical tag as a disqualifier rather than a discount.
    ///
    /// At the SPEC's -2 a rhetorical question still clears the bar: "that's a decent scale,
    /// right?" scores `?` + them + long - rhetorical = 2. It is not a question at all, so
    /// no amount of corroboration should make it one.
    pub rhetorical_vetoes: bool,
    /// Treat small-talk vocabulary as a disqualifier rather than a discount.
    ///
    /// At the SPEC's -2, "Can you hear me okay?" still scores 4: `?` + opener + them +
    /// followed - small talk. The SPEC's own Tier-A step throws small talk away again
    /// (§ Extraction: `kind=small_talk` -> nothing changes on screen), so vetoing it here costs
    /// nothing and saves the LLM call.
    pub small_talk_vetoes: bool,
}

impl Default for DetectConfig {
    /// The cue table of docs/SPEC.md § Question detector.
    fn default() -> Self {
        Self {
            question_mark: 2,
            question_opener: 2,
            hand_off: 1,
            them: 1,
            long: 1,
            followed: 1,
            rhetorical: -2,
            small_talk: -2,
            threshold: 2,
            sensitivity: Sensitivity::Balanced,
            question_source: QuestionSource::Them,
            require_question_form: true,
            rhetorical_vetoes: true,
            small_talk_vetoes: true,
        }
    }
}

impl DetectConfig {
    /// The SPEC's table exactly as written: weights summed, threshold 2, no form gate and no
    /// vetoes. The eval harness reports this alongside the default so the amendment's effect on
    /// recall and precision is visible rather than asserted.
    pub fn spec_literal() -> Self {
        Self {
            require_question_form: false,
            rhetorical_vetoes: false,
            small_talk_vetoes: false,
            ..Self::default()
        }
    }

    pub fn weight(&self, cue: Cue) -> i32 {
        match cue {
            Cue::QuestionMark => self.question_mark,
            Cue::QuestionOpener => self.question_opener,
            Cue::HandOff => self.hand_off,
            Cue::Them => self.them,
            Cue::Long => self.long,
            Cue::Followed => self.followed,
            Cue::Rhetorical => self.rhetorical,
            Cue::SmallTalk => self.small_talk,
        }
    }

    /// The bar after the sensitivity shift.
    pub fn effective_threshold(&self) -> i32 {
        self.threshold + self.sensitivity.offset()
    }

    pub fn with_sensitivity(mut self, sensitivity: Sensitivity) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    pub fn with_question_source(mut self, source: QuestionSource) -> Self {
        self.question_source = source;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_weights_are_the_spec_table() {
        let c = DetectConfig::default();
        assert_eq!(c.weight(Cue::QuestionMark), 2);
        assert_eq!(c.weight(Cue::QuestionOpener), 2);
        assert_eq!(c.weight(Cue::HandOff), 1);
        assert_eq!(c.weight(Cue::Them), 1);
        assert_eq!(c.weight(Cue::Long), 1);
        assert_eq!(c.weight(Cue::Followed), 1);
        assert_eq!(c.weight(Cue::Rhetorical), -2);
        assert_eq!(c.weight(Cue::SmallTalk), -2);
        assert_eq!(c.threshold, 2);
    }

    #[test]
    fn sensitivity_shifts_the_threshold_only() {
        let base = DetectConfig::default();
        assert_eq!(base.effective_threshold(), 2);
        assert_eq!(
            base.clone()
                .with_sensitivity(Sensitivity::Eager)
                .effective_threshold(),
            1
        );
        assert_eq!(
            base.clone()
                .with_sensitivity(Sensitivity::Strict)
                .effective_threshold(),
            3
        );
        let strict = base.with_sensitivity(Sensitivity::Strict);
        assert_eq!(strict.weight(Cue::QuestionMark), 2, "weights do not move");
    }

    #[test]
    fn sensitivity_parses_the_config_names() {
        assert_eq!(Sensitivity::from_name("eager"), Some(Sensitivity::Eager));
        assert_eq!(
            Sensitivity::from_name(" Balanced "),
            Some(Sensitivity::Balanced)
        );
        assert_eq!(Sensitivity::from_name("strict"), Some(Sensitivity::Strict));
        assert_eq!(Sensitivity::from_name("aggressive"), None);
    }

    #[test]
    fn the_default_question_source_is_the_them_lane_alone() {
        let c = DetectConfig::default();
        assert_eq!(c.question_source, QuestionSource::Them);
        assert!(c.question_source.allows(Source::Them));
        assert!(!c.question_source.allows(Source::You));
        assert!(!c.question_source.allows(Source::Unknown));
    }

    #[test]
    fn question_source_selects_lanes_and_parses_its_three_names() {
        assert!(QuestionSource::You.allows(Source::You));
        assert!(!QuestionSource::You.allows(Source::Them));
        assert!(QuestionSource::Both.allows(Source::You));
        assert!(QuestionSource::Both.allows(Source::Them));
        // A single-stream fallback has no lane to select, so it is never scored.
        assert!(!QuestionSource::Both.allows(Source::Unknown));

        assert_eq!(
            QuestionSource::from_name(" Them "),
            Some(QuestionSource::Them)
        );
        assert_eq!(QuestionSource::from_name("you"), Some(QuestionSource::You));
        assert_eq!(
            QuestionSource::from_name("both"),
            Some(QuestionSource::Both)
        );
        assert_eq!(QuestionSource::from_name("everyone"), None);
        for s in [
            QuestionSource::Them,
            QuestionSource::You,
            QuestionSource::Both,
        ] {
            assert_eq!(QuestionSource::from_name(s.as_str()), Some(s));
        }
    }
}
