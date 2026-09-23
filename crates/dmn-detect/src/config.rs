//! Weights, threshold and sensitivity. Every number the detector uses lives here so #16 can
//! retune on a real call without touching a matcher.

use crate::cue::Cue;

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
    pub interviewer: i32,
    pub long: i32,
    pub followed: i32,
    pub rhetorical: i32,
    pub small_talk: i32,
    /// A line is a candidate at or above this score.
    pub threshold: i32,
    pub sensitivity: Sensitivity,
    /// Require at least one question-form cue (`?`, an opener, or a hand-off phrase) before a line
    /// can be a candidate, whatever its score.
    ///
    /// Without this, the corroborating cues alone reach the threshold: any interviewer line over
    /// six words that the candidate answers scores interviewer + long + followed = 3, so "Okay,
    /// hold on one second, someone's at my door." is a candidate. See docs/SPEC.md § Question
    /// detector and the amendment this behaviour is recorded under.
    pub require_question_form: bool,
    /// Treat a rhetorical tag as a disqualifier rather than a discount.
    ///
    /// At the SPEC's -2 a rhetorical question still clears the bar: "that's a decent scale,
    /// right?" scores `?` + interviewer + long - rhetorical = 2. It is not a question at all, so
    /// no amount of corroboration should make it one.
    pub rhetorical_vetoes: bool,
    /// Treat small-talk vocabulary as a disqualifier rather than a discount.
    ///
    /// At the SPEC's -2, "Can you hear me okay?" still scores 4: `?` + opener + interviewer +
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
            interviewer: 1,
            long: 1,
            followed: 1,
            rhetorical: -2,
            small_talk: -2,
            threshold: 2,
            sensitivity: Sensitivity::Balanced,
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
            Cue::Interviewer => self.interviewer,
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
        assert_eq!(c.weight(Cue::Interviewer), 1);
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
}
