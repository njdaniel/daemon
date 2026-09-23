//! The suggestion contract (SPEC § Prompt contract): the [`Suggestion`] type every phase fills,
//! the question [`Kind`], and the JSON schema each phase sends to the provider.
//!
//! Property order in [`fast_schema`] is load-bearing: Ollama turns the schema into a grammar
//! that emits properties in the order given, so `is_question` and `kind` come first (they decide
//! whether anything is shown), then `question`, then `focus`, then `approach`. That order is what
//! lets the question reach the session before the approach has finished streaming. serde_json's
//! `preserve_order` feature keeps it through serialisation.

use serde::{Deserialize, Serialize};

/// What sort of question the fast phase decided this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    SystemDesign,
    Coding,
    Behavioral,
    Knowledge,
    /// A follow-up that only refines the current question; shown under it, never replacing it.
    Clarification,
    /// Not shown; logged with the detector score for the eval harness.
    SmallTalk,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::SystemDesign,
        Kind::Coding,
        Kind::Behavioral,
        Kind::Knowledge,
        Kind::Clarification,
        Kind::SmallTalk,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SystemDesign => "system_design",
            Kind::Coding => "coding",
            Kind::Behavioral => "behavioral",
            Kind::Knowledge => "knowledge",
            Kind::Clarification => "clarification",
            Kind::SmallTalk => "small_talk",
        }
    }

    /// Whether this kind puts an Approach block on screen (small talk shows nothing).
    pub fn shows_approach(self) -> bool {
        !matches!(self, Kind::SmallTalk)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deeper {
    pub label: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tradeoff {
    pub option: String,
    pub pro: String,
    pub con: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Example {
    pub language: String,
    pub code: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeContext {
    pub source: String,
    pub text: String,
}

/// Behavioral questions replace the Approach block with STAR cards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Star {
    pub situation: String,
    pub task: String,
    pub action: String,
    pub result: String,
    /// Path of the matched story file, when one was retrieved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub story: Option<String>,
}

/// One suggestion: the fast phase fills `question`, `kind`, `focus`, `approach` (and `star` for
/// behavioral); the slow phase (M2) fills the rest. The UI renders these fields and nothing else.
/// Every field defaults, so a phase's partial object parses; limits are applied by
/// [`crate::limits::enforce`], never by the prompt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Suggestion {
    pub question: String,
    pub kind: Option<Kind>,
    pub focus: String,
    pub approach: Vec<String>,
    pub deeper: Vec<Deeper>,
    pub followups: Vec<String>,
    pub tradeoffs: Vec<Tradeoff>,
    pub example: Option<Example>,
    pub mention: Vec<String>,
    pub resume_context: Vec<ResumeContext>,
    pub star: Option<Star>,
}

/// The fast phase's reply as the model sends it: [`Suggestion`]'s fast fields plus `is_question`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct FastReply {
    pub is_question: bool,
    pub kind: Option<Kind>,
    pub question: String,
    pub focus: String,
    pub approach: Vec<String>,
    pub star: Option<Star>,
}

impl FastReply {
    pub fn into_suggestion(self) -> Suggestion {
        Suggestion {
            question: self.question,
            kind: self.kind,
            focus: self.focus,
            approach: self.approach,
            star: self.star,
            ..Suggestion::default()
        }
    }
}

/// JSON schema for the fast phase. See the module doc for why the property order matters.
pub fn fast_schema() -> serde_json::Value {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "is_question": { "type": "boolean" },
            "kind": { "type": "string", "enum": kinds },
            "question": { "type": "string" },
            "focus": { "type": "string" },
            "approach": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": crate::limits::APPROACH_MIN,
                "maxItems": crate::limits::APPROACH_MAX
            },
            "star": {
                "type": "object",
                "properties": {
                    "situation": { "type": "string" },
                    "task": { "type": "string" },
                    "action": { "type": "string" },
                    "result": { "type": "string" }
                },
                "required": ["situation", "task", "action", "result"]
            }
        },
        "required": ["is_question", "kind", "question", "focus", "approach"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_schema_orders_gate_fields_then_question_then_approach() {
        let schema = fast_schema();
        let keys: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "is_question",
                "kind",
                "question",
                "focus",
                "approach",
                "star"
            ]
        );
        // And the order survives serialisation, which is what the provider actually receives.
        let text = schema.to_string();
        let q = text.find("\"question\"").unwrap();
        let a = text.find("\"approach\"").unwrap();
        assert!(q < a, "{text}");
    }

    #[test]
    fn kind_round_trips_as_snake_case() {
        for k in Kind::ALL {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
            assert_eq!(serde_json::from_str::<Kind>(&json).unwrap(), k);
        }
    }

    #[test]
    fn spec_example_parses_into_suggestion() {
        let json = r#"{
          "question": "How would you design a rate limiter for a globally distributed API?",
          "kind": "system_design",
          "focus": "architecture, consistency tradeoffs, Redis/distributed state, failure handling",
          "approach": ["Clarify requirements and rate-limit scope.", "b", "c", "d", "e"],
          "deeper": [{"label": "Algorithm", "text": "..."}],
          "followups": ["..."],
          "tradeoffs": [{"option": "Centralized Redis", "pro": "exact", "con": "one hop, SPOF"}],
          "example": {"language": "text", "code": "..."},
          "mention": ["..."],
          "resume_context": [{"source": "resume.pdf", "text": "..."}],
          "star": {"situation": "s", "task": "t", "action": "a", "result": "r", "story": "stories/x.md"}
        }"#;
        let s: Suggestion = serde_json::from_str(json).unwrap();
        assert_eq!(s.kind, Some(Kind::SystemDesign));
        assert_eq!(s.approach.len(), 5);
        assert_eq!(s.tradeoffs[0].con, "one hop, SPOF");
        assert_eq!(s.star.unwrap().story.as_deref(), Some("stories/x.md"));
    }
}
