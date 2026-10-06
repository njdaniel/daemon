//! The translation `t` speaks (#109): the user's last turn, in their own language, into the
//! interview's language (`[speak].target`), to be synthesized in their cloned voice.
//!
//! One request through the session's [`Provider`] (Ollama's `think: false` included, as on every
//! request it sends), read whole: nothing is shown or spoken until the translation is complete,
//! since the TTS server takes the text in one piece. The budget is the fast phase's timeout. The
//! reply is plain text, not JSON; [`limits::limit_translation`] makes it one line of at most
//! [`limits::TRANSLATION_CHARS`] in Rust, whatever the prompt said (CLAUDE.md: limits live in
//! `limits.rs`, never in prompts).

use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::limits::{self, Flag};
use crate::provider::{ChatRequest, LlmError, Message, Provider};

const PROMPT: &str = include_str!("../prompts/translate.md");

/// What [`translate`] produced: the text to speak, and every limit that cut it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    pub text: String,
    pub flags: Vec<Flag>,
}

/// The request for `text` into `target`, a language as the model should read it ("Spanish (es)").
/// The system prompt says "translate verbatim, output only the translation"; the user message is
/// the utterance and nothing else, so nothing in it can pass for an instruction about it.
pub fn translate_request(text: &str, target: &str, timeout: Duration) -> ChatRequest {
    ChatRequest::new(vec![
        Message::system(PROMPT.trim_end().replace("{target}", target)),
        Message::user(text.trim()),
    ])
    .with_temperature(0.0)
    .with_timeout(timeout)
}

/// Translate `text` into `target` on `provider` within `timeout`.
pub async fn translate<P: Provider + ?Sized>(
    provider: &P,
    text: &str,
    target: &str,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<Translation, LlmError> {
    let mut stream = provider
        .chat(translate_request(text, target, timeout), cancel)
        .await?;
    let mut raw = String::new();
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        raw.push_str(&delta.text);
        if delta.done {
            break;
        }
    }
    drop(stream);
    let mut flags = Vec::new();
    let text = limits::limit_translation(&raw, &mut flags);
    if text.is_empty() {
        return Err(LlmError::MalformedStream {
            provider: provider.name(),
            detail: "the translation is empty".into(),
        });
    }
    tracing::debug!(
        chars = text.chars().count(),
        flags = flags.len(),
        "translated"
    );
    Ok(Translation { text, flags })
}

/// How a failed translation reads on the status line, where it leads the keys for 5 s: short,
/// and the cause in the provider's own terms. The full error goes to the log.
pub fn failure_text(e: &LlmError) -> String {
    let why = match e {
        LlmError::Timeout(d) => format!("timed out after {:.0} s", d.as_secs_f64()),
        LlmError::Cancelled => "cancelled".to_owned(),
        LlmError::Unreachable { provider, .. } => format!("{provider} unreachable"),
        LlmError::HttpStatus {
            provider, status, ..
        } => format!("{provider} HTTP {status}"),
        LlmError::MalformedStream { detail, .. } if detail.contains("empty") => {
            "empty reply".to_owned()
        }
        LlmError::MalformedStream { provider, .. } => format!("bad reply from {provider}"),
        LlmError::Remote { message, .. } => message.chars().take(40).collect(),
    };
    format!("translate failed: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OllamaProvider;
    use crate::limits::{FlagKind, TRANSLATION_CHARS};
    use crate::mock::{MockProvider, Step};

    const SAID: &str = "I'd start with a token bucket per API key, kept in Redis.";
    const TARGET: &str = "Spanish (es)";
    const BUDGET: Duration = Duration::from_millis(4000);

    fn reply(text: &str) -> MockProvider {
        MockProvider::new(vec![Step::Text(text.into()), Step::Done])
    }

    async fn run(mock: &MockProvider) -> Result<Translation, LlmError> {
        translate(mock, SAID, TARGET, BUDGET, CancellationToken::new()).await
    }

    #[test]
    fn the_prompt_names_the_target_asks_for_the_translation_only_and_carries_the_utterance() {
        let req = translate_request(&format!("  {SAID}\n"), TARGET, BUDGET);
        assert_eq!(req.messages.len(), 2);
        let system = &req.messages[0].content;
        assert!(
            system.starts_with(
                "Translate the user's message verbatim into Spanish (es); output only the translation."
            ),
            "{system}"
        );
        assert!(!system.contains("{target}"), "{system}");
        assert!(system.contains("add nothing"), "{system}");
        assert_eq!(
            req.messages[1].content, SAID,
            "the utterance and nothing else"
        );
        assert_eq!(req.schema, None, "plain text, not JSON");
        assert_eq!(req.temperature, 0.0);
        assert_eq!(req.timeout, BUDGET, "the caller's budget: the fast phase's");
        // No length rule in the prompt: the limit is limits.rs's.
        assert!(!system.contains("600") && !system.contains("characters"));
    }

    /// The configured provider's own wire format: Ollama gets `think: false`, as every request
    /// it sends does, and no `format`.
    #[test]
    fn through_ollama_the_request_says_think_false() {
        let ollama = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
        let body = ollama.build_body(&translate_request(SAID, TARGET, BUDGET));
        assert_eq!(body["think"], false);
        assert!(body.get("format").is_none(), "{body}");
        assert_eq!(body["messages"][1]["content"], SAID);
    }

    #[tokio::test]
    async fn the_reply_is_read_whole_and_comes_back_as_one_line() {
        let mock = MockProvider::new(vec![
            Step::Text("Empezaría con un token bucket ".into()),
            Step::Text("por clave de API,\nguardado en Redis.".into()),
            Step::Done,
        ]);
        let t = run(&mock).await.unwrap();
        assert_eq!(
            t.text,
            "Empezaría con un token bucket por clave de API, guardado en Redis."
        );
        assert!(t.flags.is_empty());
        assert_eq!(mock.requests().len(), 1, "exactly one request");
        assert_eq!(mock.requests()[0].messages[1].content, SAID);
    }

    #[tokio::test]
    async fn quotes_around_the_whole_reply_are_removed_and_inner_ones_kept() {
        let t = run(&reply("\"Hola, ¿qué tal?\"")).await.unwrap();
        assert_eq!(t.text, "Hola, ¿qué tal?");
        let t = run(&reply("«Dije \"sí\" al final.»")).await.unwrap();
        assert_eq!(t.text, "Dije \"sí\" al final.");
        let t = run(&reply("\"Sí\" y \"no\"")).await.unwrap();
        assert_eq!(t.text, "\"Sí\" y \"no\"", "not one pair around the whole");
    }

    #[tokio::test]
    async fn an_over_long_translation_is_cut_in_rust_and_flagged() {
        let sentence = "Esta es una frase de prueba bastante larga para el límite. ";
        let long = sentence.repeat(20);
        let t = run(&reply(&long)).await.unwrap();
        assert!(t.text.chars().count() <= TRANSLATION_CHARS, "{}", t.text);
        assert!(t.text.ends_with("límite."), "cut at a sentence: {}", t.text);
        assert!(matches!(
            t.flags.as_slice(),
            [Flag { field, kind: FlagKind::Truncated { .. } }] if field == "translation"
        ));
        let words = "palabra ".repeat(120);
        let t = run(&reply(&words)).await.unwrap();
        assert!(t.text.chars().count() <= TRANSLATION_CHARS);
        assert!(t.text.ends_with('…'), "cut at a word: {}", t.text);
    }

    #[tokio::test]
    async fn provider_errors_surface_as_short_status_text() {
        let err = run(&MockProvider::new(vec![Step::Error(
            "model not found".into(),
        )]))
        .await
        .unwrap_err();
        assert_eq!(failure_text(&err), "translate failed: model not found");

        let slow = MockProvider::new(vec![Step::Text("Hola".into()), Step::Done])
            .with_delay(Duration::from_millis(200));
        let err = translate(
            &slow,
            SAID,
            TARGET,
            Duration::from_millis(30),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LlmError::Timeout(_)), "{err}");
        assert_eq!(failure_text(&err), "translate failed: timed out after 0 s");

        let err = run(&reply("  \n ")).await.unwrap_err();
        assert!(matches!(err, LlmError::MalformedStream { .. }), "{err}");
        assert_eq!(failure_text(&err), "translate failed: empty reply");

        let unreachable = LlmError::Unreachable {
            provider: "ollama",
            url: "http://localhost:11434/api/chat".into(),
            detail: "connection refused".into(),
        };
        assert_eq!(
            failure_text(&unreachable),
            "translate failed: ollama unreachable"
        );
        let status = LlmError::HttpStatus {
            provider: "openai",
            status: 429,
            body: "slow down".into(),
        };
        assert_eq!(failure_text(&status), "translate failed: openai HTTP 429");
        assert_eq!(
            failure_text(&LlmError::Timeout(Duration::from_millis(4000))),
            "translate failed: timed out after 4 s"
        );
    }
}
