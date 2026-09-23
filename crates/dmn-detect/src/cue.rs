//! The cues of docs/SPEC.md § Question detector, and the word lists they match.
//!
//! Each cue is a yes/no test; [`DetectConfig`](crate::DetectConfig) holds its points, so #16 can
//! retune weights on a real call without touching the matchers.

use crate::sentence::words;

/// One matched cue. The detector keeps the list on every [`Candidate`](crate::Candidate) so the
/// eval harness can show why a line scored what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cue {
    /// Ends with `?` from whisper punctuation.
    QuestionMark,
    /// Starts with a wh-word or an auxiliary verb.
    QuestionOpener,
    /// Contains a hand-off phrase ("let's switch gears", "next question").
    HandOff,
    /// The speaker is the interviewer.
    Interviewer,
    /// Longer than six words.
    Long,
    /// Followed by >= 1.5 s of silence or by the candidate speaking.
    Followed,
    /// Ends with a rhetorical tag ("..., right?", "you know?").
    Rhetorical,
    /// Mostly small-talk vocabulary.
    SmallTalk,
}

impl Cue {
    /// Short label for `dmn replay` and the eval report.
    pub fn label(self) -> &'static str {
        match self {
            Cue::QuestionMark => "?",
            Cue::QuestionOpener => "opener",
            Cue::HandOff => "handoff",
            Cue::Interviewer => "interviewer",
            Cue::Long => "long",
            Cue::Followed => "followed",
            Cue::Rhetorical => "rhetorical",
            Cue::SmallTalk => "smalltalk",
        }
    }

    /// True for the cues that make a line read as a question at all. The others corroborate:
    /// being the interviewer, or speaking for more than six words, says nothing about whether a
    /// question was asked.
    pub fn is_question_form(self) -> bool {
        matches!(self, Cue::QuestionMark | Cue::QuestionOpener | Cue::HandOff)
    }
}

/// Wh-words and auxiliary verbs that open a question, plus the imperative openers the SPEC lists
/// ("tell me", "walk me through"), which ask for an answer without a question mark.
///
/// Kept to the SPEC's class: wh-words and auxiliaries. Openers like "give me" or "have a" are
/// deliberately absent — they open requests and pleasantries ("give me a second", "have a good
/// afternoon"), not interview questions.
const OPENERS: [&str; 27] = [
    // wh-words
    "how", "what", "why", "when", "which", "who", "whom", "whose", "where",
    // auxiliaries
    "can", "could", "would", "will", "should", "do", "does", "did", "is", "are", "was", "were",
    // imperative openers from the SPEC's list
    "describe", "explain", "imagine", "suppose", "tell", "walk",
];

/// Multi-word openers checked before the single-word list, so "tell me about a time" matches as an
/// opener but "tell" alone in "I will tell you why" does not open the sentence.
const OPENER_PHRASES: [&str; 6] = [
    "tell me",
    "walk me through",
    "talk me through",
    "let's say",
    "let us say",
    "given a",
];

/// Hand-off phrases: the interviewer signalling that a question is coming.
const HANDOFF_PHRASES: [&str; 8] = [
    "let's switch gears",
    "let us switch gears",
    "next question",
    "i'd like to hear",
    "i would like to hear",
    "talk me through",
    "let me ask",
    "moving on to",
];

/// Rhetorical tags: a question mark that is not a question.
const RHETORICAL_TAGS: [&str; 6] = [
    "right?",
    "you know?",
    "isn't it?",
    "wasn't it?",
    "don't you think?",
    "yeah?",
];

/// Small-talk vocabulary (docs/SPEC.md names weather, weekend, coffee, "can you hear me"). The
/// rest is the same register: the call itself, the calendar, the room, the pleasantries at each
/// end. It is a vocabulary test, not a topic model; the LLM fast phase is what finally decides a
/// candidate is small talk (SPEC § Extraction).
const SMALL_TALK: [&str; 34] = [
    "weather",
    "weekend",
    "coffee",
    "hear me",
    "hear you",
    "you there",
    "audio",
    "mic",
    "microphone",
    "headset",
    "connection",
    "cutting out",
    "frozen",
    "calendar",
    "invite",
    "meeting link",
    "reschedule",
    "running late",
    "double-booked",
    "double booked",
    "traffic",
    "raining",
    "pouring",
    "snow",
    "sunny",
    "holiday",
    "vacation",
    "lunch",
    "door",
    "one second",
    "hold on",
    "nice to meet",
    "good to meet",
    "how are you",
];

/// Ends with `?` (whisper's own punctuation).
pub fn ends_with_question_mark(sentence: &str) -> bool {
    sentence
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '”' | '’'))
        .ends_with('?')
}

/// Opens with a wh-word, an auxiliary verb, or one of the SPEC's imperative openers.
pub fn starts_with_opener(sentence: &str) -> bool {
    let w = words(sentence);
    let Some(first) = w.first() else {
        return false;
    };
    // Skip a leading discourse marker: "So, how would you..." opens the same way as "How would
    // you...". Only one, and only from a closed list, so "Okay, hold on one second" does not
    // become an opener.
    let skipped = if matches!(
        first.as_str(),
        "so" | "okay" | "ok" | "alright" | "right" | "well" | "now" | "and" | "but" | "great"
    ) && w.len() > 1
    {
        &w[1..]
    } else {
        &w[..]
    };
    let Some(first) = skipped.first() else {
        return false;
    };
    let joined = skipped.join(" ");
    if OPENERS.contains(&first.as_str()) {
        return true;
    }
    // Spoken questions put framing in front of the opener: "Okay, a coding one, given a stream of
    // integers, describe how you would...". Whisper often drops those commas, leaving one
    // sentence, so a multi-word opener counts anywhere in the first few words rather than only at
    // the very start. The window is short so that "we talked about how you would scale it" (an
    // opener deep inside a statement) still does not match.
    let window: String = skipped
        .iter()
        .take(OPENER_WINDOW)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    OPENER_PHRASES.iter().any(|p| window.contains(p))
        || OPENER_PHRASES.iter().any(|p| joined.starts_with(p))
}

/// How many words into a sentence a multi-word opener still counts as opening it.
const OPENER_WINDOW: usize = 8;

/// Contains a hand-off phrase anywhere in the line.
pub fn has_handoff(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    HANDOFF_PHRASES.iter().any(|p| lowered.contains(p))
}

/// Longer than six words.
pub fn is_long(sentence: &str) -> bool {
    words(sentence).len() > 6
}

/// Ends with a rhetorical tag.
///
/// A tag question is punctuated as one: "that's a decent scale, right?". The comma is what makes
/// it a tag rather than the last word of the question, so it is required unless the tag is the
/// whole utterance ("Right?"). Without that, whisper turning "make a notification write" into
/// "make a notification right?" would read as rhetoric and a real question would be thrown away.
pub fn is_rhetorical(sentence: &str) -> bool {
    let lowered = sentence.trim().to_ascii_lowercase();
    let lowered = lowered.trim_end_matches(['"', '\'', '”', '’']);
    RHETORICAL_TAGS.iter().any(|tag| {
        if !lowered.ends_with(tag) {
            return false;
        }
        let before = &lowered[..lowered.len() - tag.len()];
        let before = before.trim_end();
        before.is_empty() || before.ends_with(',')
    })
}

/// Mostly small-talk vocabulary: a small-talk phrase appears and the sentence is not carrying a
/// technical question around it. Matching is on phrases, so "link" in "the link changed at the
/// last minute" counts while "linked list" does not.
pub fn is_small_talk(sentence: &str) -> bool {
    let lowered = format!(" {} ", sentence.to_ascii_lowercase());
    SMALL_TALK.iter().any(|phrase| {
        if phrase.contains(' ') {
            lowered.contains(phrase)
        } else {
            lowered.contains(&format!(" {phrase} "))
                || lowered.contains(&format!(" {phrase},"))
                || lowered.contains(&format!(" {phrase}?"))
                || lowered.contains(&format!(" {phrase}."))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_mark_cue_needs_a_trailing_question_mark() {
        assert!(ends_with_question_mark("How would you design it?"));
        assert!(ends_with_question_mark("Is that clear?\""));
        assert!(!ends_with_question_mark(
            "Tell me about a time you shipped late."
        ));
    }

    #[test]
    fn opener_cue_matches_wh_aux_and_imperatives_but_not_statements() {
        assert!(starts_with_opener("How would you design a rate limiter?"));
        assert!(starts_with_opener("Could you walk through the tradeoffs"));
        assert!(starts_with_opener("Tell me about a disagreement"));
        assert!(starts_with_opener("Walk me through your last incident"));
        assert!(starts_with_opener("Suppose the cache is cold"));
        assert!(
            starts_with_opener("So how would you shard it?"),
            "one marker is skipped"
        );
        assert!(!starts_with_opener("I shipped it on Friday"));
        assert!(!starts_with_opener("Give me a second, I'm writing a note."));
        assert!(!starts_with_opener("Have a good rest of your afternoon."));
        assert!(!starts_with_opener(""));
    }

    #[test]
    fn handoff_cue_matches_the_phrase_anywhere_in_the_line() {
        assert!(has_handoff(
            "Okay. Let's switch gears to something hands-on."
        ));
        assert!(has_handoff(
            "Anyway. Next question. I'd like to hear about a failure."
        ));
        assert!(!has_handoff("We shipped the gearbox service last quarter."));
    }

    #[test]
    fn long_cue_counts_more_than_six_words() {
        assert!(is_long(
            "How would you design a globally distributed rate limiter"
        ));
        assert!(!is_long("What about Redis?"));
        assert!(
            !is_long("One two three four five six"),
            "six is not more than six"
        );
    }

    #[test]
    fn rhetorical_cue_matches_trailing_tags_only() {
        assert!(is_rhetorical("That's a decent scale, right?"));
        assert!(is_rhetorical("You've seen this before, you know?"));
        assert!(is_rhetorical("Right?"), "a bare tag is the whole utterance");
        assert!(!is_rhetorical("Right, so how would you scale it?"));
        assert!(!is_rhetorical("What would you do differently?"));
    }

    #[test]
    fn a_tag_needs_its_comma_so_a_mistranscribed_question_survives() {
        // whisper writes "write" as "right" often enough that this matters: without the comma
        // rule, a real question about idempotent writes reads as rhetoric and is thrown away.
        assert!(!is_rhetorical(
            "How exactly do you make a notification right?"
        ));
        assert!(is_rhetorical("You would make it idempotent, right?"));
    }

    #[test]
    fn a_multi_word_opener_counts_just_past_the_start_but_not_deep_inside() {
        assert!(
            starts_with_opener("Okay, a coding one given a stream of integers describe the median"),
            "spoken framing comes before the opener"
        );
        assert!(!starts_with_opener(
            "We spent the whole quarter arguing about how you would shard the write path"
        ));
    }

    #[test]
    fn generic_technical_words_are_not_call_chatter() {
        assert!(
            !is_small_talk("Suppose one link goes viral and gets a hundred thousand redirects"),
            "a short link is the system under discussion, not the calendar"
        );
        assert!(is_small_talk("Did you get the meeting link okay?"));
    }

    #[test]
    fn small_talk_cue_matches_call_chatter_not_technical_lines() {
        assert!(is_small_talk("Can you hear me okay?"));
        assert!(is_small_talk("How's the weather where you are?"));
        assert!(is_small_talk("Did you find the calendar invite okay?"));
        assert!(is_small_talk(
            "Okay, hold on one second, someone's at my door."
        ));
        assert!(!is_small_talk("How would you design a rate limiter?"));
        assert!(!is_small_talk("Walk me through a linked list deletion."));
    }

    #[test]
    fn form_cues_are_the_ones_that_read_as_a_question() {
        assert!(Cue::QuestionMark.is_question_form());
        assert!(Cue::QuestionOpener.is_question_form());
        assert!(Cue::HandOff.is_question_form());
        assert!(!Cue::Interviewer.is_question_form());
        assert!(!Cue::Long.is_question_form());
        assert!(!Cue::Followed.is_question_form());
    }
}
