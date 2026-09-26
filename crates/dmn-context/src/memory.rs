//! Session memory (#57, SPEC § Session memory): the pieces of a summary line that do not come
//! from the model.
//!
//! A question is summarised after the candidate has answered it: the slow phase of the next
//! question reads that answer and writes one line (`dmn-llm`'s `summary` and `promise`). The line
//! starts with [`clock`], when the question was asked. When no model line comes (the slow phase
//! is off, timed out, or never ran because the session ended first), [`fallback`] stands in, so
//! every answered question still has its one line. The session (`dmn-session`) keeps the lines
//! in question order and hands the last 8 to every prompt.

/// Session time of a question, as a summary line starts: `m:ss` under an hour, `h:mm:ss` after.
pub fn clock(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// The summary text when the model gave none: the question and what it probed. Says nothing
/// about the answer, because nothing read it.
pub fn fallback(question: &str, focus: &str) -> String {
    let question = question.trim().trim_end_matches(['?', '.']);
    let focus = focus.trim();
    if focus.is_empty() {
        format!("{question} — asked")
    } else {
        format!("{question} — asked about {focus}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_session_time() {
        assert_eq!(clock(0.0), "00:00");
        assert_eq!(clock(192.7), "03:12");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(clock(-1.0), "00:00");
        assert_eq!(clock(f64::NAN), "00:00");
    }

    #[test]
    fn the_fallback_names_the_question_and_its_focus() {
        assert_eq!(
            fallback(
                "How would you design a rate limiter?",
                "consistency, failure"
            ),
            "How would you design a rate limiter — asked about consistency, failure"
        );
        assert_eq!(fallback("Two sum.", " "), "Two sum — asked");
    }
}
