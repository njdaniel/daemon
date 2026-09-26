//! Prompt assembly (SPEC § Prompt contract). The system prompt is fixed per [`Kind`]: a shared
//! base (role, the five-line rule, the no-paragraphs rule) plus that kind's section from
//! `prompts/`. A fresh candidate's kind is not known until the fast phase answers, so its system
//! prompt carries every kind's section; a regenerate knows the kind and carries only its own.
//! The JSON schema travels in the request (`format` for Ollama), not in the prompt text.

use crate::provider::Message;
use crate::schema::Kind;

const BASE: &str = include_str!("../prompts/base.md");

fn section(kind: Kind) -> &'static str {
    match kind {
        Kind::SystemDesign => include_str!("../prompts/system_design.md"),
        Kind::Coding => include_str!("../prompts/coding.md"),
        Kind::Behavioral => include_str!("../prompts/behavioral.md"),
        Kind::Knowledge => include_str!("../prompts/knowledge.md"),
        Kind::Clarification => include_str!("../prompts/clarification.md"),
        Kind::SmallTalk => include_str!("../prompts/small_talk.md"),
    }
}

/// The system prompt for `kind`, or for a candidate whose kind is not known yet (`None`).
pub fn system_prompt(kind: Option<Kind>) -> String {
    let mut out = String::from(BASE.trim_end());
    out.push_str("\n\nPer kind:\n");
    let kinds: &[Kind] = match kind {
        Some(ref k) => std::slice::from_ref(k),
        None => &Kind::ALL,
    };
    for k in kinds {
        out.push_str("- ");
        out.push_str(section(*k).trim());
        out.push('\n');
    }
    out
}

/// The question on screen now, which a clarification attaches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentQuestion {
    pub question: String,
    pub kind: Kind,
}

/// A story from the candidate's own material (#58), matched to a behavioral question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryText {
    /// `stories/<file>`: what the STAR header names. Set on `star.story` in Rust, never asked of
    /// the model, so the named story is always the one the prompt carried.
    pub label: String,
    pub text: String,
}

/// Everything the fast phase is asked about. Built by the wiring crate from the detector's
/// candidate, the transcript and (from M2) the context assembler; this crate only defines it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FastRequest {
    /// The Interviewer text the detector proposed.
    pub candidate: String,
    /// The detector's score for `candidate`, logged when the model says it is not a question.
    pub score: i32,
    /// The last 60 s of transcript, one `Speaker: text` line per segment.
    pub transcript: String,
    /// Job-description requirement bullets (≤ 8), when a job description is loaded.
    pub job_requirements: Vec<String>,
    /// Retrieved source chunks, already capped by the context assembler.
    pub chunks: Vec<String>,
    /// The last session-summary lines.
    pub summary: Vec<String>,
    pub current: Option<CurrentQuestion>,
    /// The user pressed regenerate: higher temperature and a note asking for another angle.
    pub regenerate: bool,
    /// The story matched to a behavioral question (#58): STAR is built from it.
    pub story: Option<StoryText>,
    /// Swap (#58): answer the question on screen again from `story`, the next-ranked one.
    pub swap: bool,
}

impl FastRequest {
    pub fn new(candidate: impl Into<String>, score: i32) -> Self {
        Self {
            candidate: candidate.into(),
            score,
            ..Self::default()
        }
    }
}

/// The user message: job requirements, retrieved chunks, session summary, recent transcript,
/// then the candidate. Empty sections are left out.
pub fn user_message(req: &FastRequest) -> String {
    let mut out = String::new();
    if !req.job_requirements.is_empty() {
        out.push_str("## Job requirements\n");
        for r in &req.job_requirements {
            out.push_str("- ");
            out.push_str(r.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    if !req.chunks.is_empty() {
        out.push_str("## Candidate's material\n");
        for (i, c) in req.chunks.iter().enumerate() {
            out.push_str(&format!("[{}] {}\n", i + 1, c.trim()));
        }
        out.push('\n');
    }
    if let Some(story) = &req.story {
        out.push_str(&format!(
            "## Matched story ({})\n{}\n\n",
            story.label,
            story.text.trim()
        ));
    }
    if !req.summary.is_empty() {
        out.push_str("## Session so far\n");
        for s in &req.summary {
            out.push_str(s.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(cur) = &req.current {
        out.push_str(&format!(
            "## Current question ({})\n{}\n\n",
            cur.kind.as_str(),
            cur.question.trim()
        ));
    }
    if !req.transcript.trim().is_empty() {
        out.push_str("## Recent transcript\n");
        out.push_str(req.transcript.trim());
        out.push_str("\n\n");
    }
    out.push_str("## Interviewer just said\n");
    out.push_str(req.candidate.trim());
    out.push('\n');
    if req.swap {
        out.push_str(
            "\nThe user asked for another of their stories: build the STAR cards and the \
             Approach from the matched story above.\n",
        );
    } else if req.regenerate {
        out.push_str("\nThe user asked for a different angle.\n");
    }
    out
}

/// System + user messages for the fast phase.
pub fn fast_messages(req: &FastRequest) -> Vec<Message> {
    let kind = if req.regenerate || req.swap {
        req.current.as_ref().map(|c| c.kind)
    } else {
        None
    };
    vec![
        Message::system(system_prompt(kind)),
        Message::user(user_message(req)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kind_prompt_carries_every_kind_and_the_rules() {
        let p = system_prompt(None);
        for k in Kind::ALL {
            assert!(p.contains(&format!("- {}:", k.as_str())), "missing {k:?}");
        }
        assert!(p.contains("three to five lines"));
        assert!(p.contains("No numbering, no paragraphs"));
    }

    #[test]
    fn known_kind_prompt_carries_only_that_kind() {
        let p = system_prompt(Some(Kind::Coding));
        assert!(p.contains("- coding:"));
        assert!(!p.contains("- behavioral:"));
    }

    #[test]
    fn user_message_orders_sections_and_skips_empty_ones() {
        let req = FastRequest {
            candidate: "How would you design a rate limiter?".into(),
            score: 5,
            transcript: "Interviewer: Let's switch gears.\nYou: Sure.".into(),
            job_requirements: vec!["Distributed systems".into()],
            current: Some(CurrentQuestion {
                question: "Tell me about yourself".into(),
                kind: Kind::Behavioral,
            }),
            ..FastRequest::default()
        };
        let m = user_message(&req);
        let job = m.find("## Job requirements").unwrap();
        let cur = m.find("## Current question (behavioral)").unwrap();
        let tr = m.find("## Recent transcript").unwrap();
        let cand = m.find("## Interviewer just said").unwrap();
        assert!(job < cur && cur < tr && tr < cand, "{m}");
        assert!(!m.contains("## Candidate's material"));
        assert!(!m.contains("## Session so far"));
        assert!(!m.contains("different angle"));
    }

    #[test]
    fn regenerate_uses_the_current_kind_and_asks_for_another_angle() {
        let req = FastRequest {
            candidate: "q".into(),
            current: Some(CurrentQuestion {
                question: "q".into(),
                kind: Kind::Knowledge,
            }),
            regenerate: true,
            ..FastRequest::default()
        };
        let msgs = fast_messages(&req);
        assert!(msgs[0].content.contains("- knowledge:"));
        assert!(!msgs[0].content.contains("- coding:"));
        assert!(msgs[1].content.contains("different angle"));
    }

    fn story() -> StoryText {
        StoryText {
            label: "stories/broker.md".into(),
            text: "Situation: we chose a broker.".into(),
        }
    }

    #[test]
    fn a_matched_story_goes_in_before_the_transcript_and_without_one_nothing_changes() {
        let plain = FastRequest {
            transcript: "Interviewer: hi".into(),
            ..FastRequest::new("Tell me about a time you disagreed.", 5)
        };
        let with = FastRequest {
            story: Some(story()),
            ..plain.clone()
        };
        let m = user_message(&with);
        let s = m.find("## Matched story (stories/broker.md)\n").expect(&m);
        assert!(m.contains("Situation: we chose a broker."));
        assert!(s < m.find("## Recent transcript").unwrap(), "{m}");
        assert_eq!(
            user_message(&FastRequest {
                story: None,
                ..with
            }),
            user_message(&plain),
            "no story, the M1 prompt"
        );
        assert!(!user_message(&plain).contains("Matched story"));
    }

    #[test]
    fn swap_uses_the_current_kind_and_asks_for_the_story_at_temperature_0() {
        let req = FastRequest {
            candidate: "Tell me about a time you disagreed.".into(),
            current: Some(CurrentQuestion {
                question: "Tell me about a time you disagreed.".into(),
                kind: Kind::Behavioral,
            }),
            story: Some(story()),
            swap: true,
            ..FastRequest::default()
        };
        let msgs = fast_messages(&req);
        assert!(msgs[0].content.contains("- behavioral:"));
        assert!(!msgs[0].content.contains("- coding:"));
        assert!(msgs[1].content.contains("another of their stories"));
        assert!(!msgs[1].content.contains("different angle"));
        assert_eq!(crate::fast::fast_chat_request(&req).temperature, 0.0);
    }
}
