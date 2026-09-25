//! The job-description summary (#54): one request, made once per job description when a
//! session loads its context, that turns the job description into ≤ 8 requirement bullets every
//! fast-phase prompt then carries (SPEC § Context and LLM: "Job description is always summarised
//! once at load into ≤ 8 requirement bullets and included in every prompt").
//!
//! It is never on a question's path: the caller runs it before serving questions and caches the
//! result by the job description's content hash. The reply is read whole (nothing is shown while
//! it streams) and cut to [`limits::REQUIREMENTS_MAX`] bullets in Rust, whatever the prompt said.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::limits::{self, Flag};
use crate::provider::{ChatRequest, LlmError, Message, Provider};

const PROMPT: &str = include_str!("../prompts/requirements.md");

/// SPEC § Providers: the slow phase gets 20 s; this one-off call gets the same.
pub const REQUIREMENTS_TIMEOUT: Duration = Duration::from_secs(20);
/// Words of job description sent. A job description is a page or two; this keeps a pasted
/// novel from filling the model's context.
pub const JOB_DESCRIPTION_WORDS: usize = 3_000;

/// The summary: the bullets as the prompt will carry them, and every limit that was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirements {
    pub bullets: Vec<String>,
    pub flags: Vec<Flag>,
}

/// The JSON schema of the reply.
pub fn requirements_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "requirements": {
                "type": "array",
                "items": { "type": "string" },
                "maxItems": limits::REQUIREMENTS_MAX
            }
        },
        "required": ["requirements"]
    })
}

/// The request for `job_description`.
pub fn requirements_request(job_description: &str) -> ChatRequest {
    let words: Vec<&str> = job_description.split_whitespace().collect();
    let text = if words.len() > JOB_DESCRIPTION_WORDS {
        words[..JOB_DESCRIPTION_WORDS].join(" ")
    } else {
        job_description.trim().to_owned()
    };
    ChatRequest::new(vec![
        Message::system(PROMPT.trim_end()),
        Message::user(format!("## Job description\n{text}\n")),
    ])
    .with_schema(requirements_schema())
    .with_temperature(0.0)
    .with_timeout(REQUIREMENTS_TIMEOUT)
}

/// Summarise `job_description` into requirement bullets on `provider`.
pub async fn summarise_job_description<P: Provider + ?Sized>(
    provider: &P,
    job_description: &str,
    cancel: CancellationToken,
) -> Result<Requirements, LlmError> {
    let mut stream = provider
        .chat(requirements_request(job_description), cancel)
        .await?;
    let mut text = String::new();
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        text.push_str(&delta.text);
        if delta.done {
            break;
        }
    }
    drop(stream);
    let malformed = |detail: String| LlmError::MalformedStream {
        provider: provider.name(),
        detail,
    };
    let value: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| malformed(format!("requirements reply is not JSON: {e}")))?;
    let raw: Vec<String> = value
        .get("requirements")
        .and_then(|r| r.as_array())
        .ok_or_else(|| malformed("requirements reply has no \"requirements\" array".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut flags = Vec::new();
    let bullets = limits::limit_requirements(&raw, &mut flags);
    tracing::debug!(
        bullets = bullets.len(),
        flags = flags.len(),
        "job description summarised"
    );
    Ok(Requirements { bullets, flags })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{REQUIREMENT_CHARS, REQUIREMENTS_MAX};
    use crate::mock::{MockProvider, Step};
    use serde_json::json;

    async fn summarise(mock: &MockProvider) -> Result<Requirements, LlmError> {
        summarise_job_description(mock, "We need Rust and Kafka.", CancellationToken::new()).await
    }

    #[tokio::test]
    async fn a_reply_becomes_trimmed_one_line_bullets() {
        let mock = MockProvider::json(&json!({
            "requirements": [
                "- Design distributed services in Go or Rust",
                "2. Postgres sharding,\n Redis caching, Kafka streaming",
                "   ",
                "Own reliability: SLOs, on-call, incident review"
            ]
        }));
        let r = summarise(&mock).await.unwrap();
        assert_eq!(
            r.bullets,
            [
                "Design distributed services in Go or Rust",
                "Postgres sharding, Redis caching, Kafka streaming",
                "Own reliability: SLOs, on-call, incident review",
            ]
        );
        assert!(r.flags.is_empty());
        let sent = &mock.requests()[0];
        assert!(sent.messages[1].content.contains("We need Rust and Kafka."));
        assert_eq!(sent.timeout, REQUIREMENTS_TIMEOUT);
        assert_eq!(sent.schema, Some(requirements_schema()));
    }

    #[tokio::test]
    async fn more_than_eight_or_over_long_bullets_are_cut_in_rust() {
        let long = "word ".repeat(60);
        let many: Vec<String> = (0..12).map(|i| format!("Requirement {i}")).collect();
        let mut all = vec![long.clone()];
        all.extend(many);
        let mock = MockProvider::json(&json!({ "requirements": all }));
        let r = summarise(&mock).await.unwrap();
        assert_eq!(r.bullets.len(), REQUIREMENTS_MAX);
        assert!(r.bullets[0].chars().count() <= REQUIREMENT_CHARS);
        let fields: Vec<&str> = r.flags.iter().map(|f| f.field.as_str()).collect();
        assert!(fields.contains(&"requirements"), "{fields:?}");
        assert!(fields.contains(&"requirements[0]"), "{fields:?}");
    }

    #[tokio::test]
    async fn a_reply_without_the_array_is_an_error_not_an_empty_summary() {
        let mock = MockProvider::json(&json!({ "bullets": ["x"] }));
        assert!(matches!(
            summarise(&mock).await,
            Err(LlmError::MalformedStream { .. })
        ));
        let mock = MockProvider::new(vec![Step::Text("not json".into()), Step::Done]);
        assert!(matches!(
            summarise(&mock).await,
            Err(LlmError::MalformedStream { .. })
        ));
    }

    #[test]
    fn a_huge_job_description_is_cut_before_it_is_sent() {
        let jd = "rust ".repeat(JOB_DESCRIPTION_WORDS + 500);
        let req = requirements_request(&jd);
        let words = req.messages[1].content.split_whitespace().count();
        assert_eq!(words, JOB_DESCRIPTION_WORDS + 3, "3 words of heading");
    }
}
