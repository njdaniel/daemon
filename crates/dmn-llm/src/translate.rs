//! The translation `t` speaks (#109): the user's last turn, in their own language, into the
//! interview's language (`[speak].target`), to be synthesized in their cloned voice.
//!
//! One request through the session's [`Provider`] (Ollama's `think: false` included, as on every
//! request it sends), read whole: nothing is shown or spoken until the translation is complete,
//! since the TTS server takes the text in one piece. The budget is the fast phase's timeout. The
//! reply is plain text, not JSON; [`limits::limit_translation`] makes it one line of at most
//! [`limits::TRANSLATION_CHARS`] in Rust, whatever the prompt said (CLAUDE.md: limits live in
//! `limits.rs`, never in prompts).
//!
//! #117: the reply is then checked against the turn ([`check_translation`]): a small model given a
//! fragment sometimes stops translating and answers as the speaker instead (live, 2026-10-08:
//! "Back to English. And no input." came back as a 141-character English job-interview answer and
//! was spoken into the call). A reply far longer than the turn, or not in the target's script,
//! is `off-script`: an error with the usual 5 s notice, and nothing is spoken.

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

/// Translate `text` into `target` ("Spanish (es)") on `provider` within `timeout`; `code` is the
/// target's language code (`es`), for [`check_translation`].
pub async fn translate<P: Provider + ?Sized>(
    provider: &P,
    text: &str,
    target: &str,
    code: &str,
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
    let turn = text;
    let text = limits::limit_translation(&raw, &mut flags);
    if text.is_empty() {
        return Err(LlmError::MalformedStream {
            provider: provider.name(),
            detail: "the translation is empty".into(),
        });
    }
    if let Err(detail) = check_translation(turn, &text, code) {
        // The reply is the model's, not the user's words: it may go to the log.
        tracing::warn!(
            reply = %text,
            turn_chars = turn.trim().chars().count(),
            %detail,
            "translate: the reply is not a translation of the turn; nothing is spoken"
        );
        return Err(LlmError::MalformedStream {
            provider: provider.name(),
            detail,
        });
    }
    tracing::debug!(
        chars = text.chars().count(),
        flags = flags.len(),
        "translated"
    );
    Ok(Translation { text, flags })
}

/// The detail a rejected reply's error starts with (#117).
pub const OFF_SCRIPT: &str = "off-script";

/// A reply may be this many times the turn's characters, plus [`SLACK_CHARS`], before it cannot
/// be its translation: German and Russian run 1.2–1.6× English.
const LENGTH_RATIO: usize = 5; // halves: 2.5×
const SLACK_CHARS: usize = 20;

/// The script a target language is written in, when it is not Latin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Cyrillic,
    Greek,
    Hangul,
    Japanese,
    Han,
    Arabic,
    Hebrew,
    Devanagari,
    Thai,
}

impl Script {
    /// `code` as `[speak].target` has it (`ru`, `zh-Hant`, `pt_BR`); `None` for a Latin-script
    /// language, or one this table does not know.
    fn of(code: &str) -> Option<Self> {
        let base = code
            .trim()
            .split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Some(match base.as_str() {
            "ru" | "uk" | "bg" | "be" | "sr" | "mk" | "kk" | "ky" | "mn" | "tg" => Self::Cyrillic,
            "el" => Self::Greek,
            "ko" => Self::Hangul,
            "ja" => Self::Japanese,
            "zh" | "yue" => Self::Han,
            "ar" | "fa" | "ur" | "ps" => Self::Arabic,
            "he" | "yi" => Self::Hebrew,
            "hi" | "mr" | "ne" | "sa" => Self::Devanagari,
            "th" => Self::Thai,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cyrillic => "Cyrillic",
            Self::Greek => "Greek",
            Self::Hangul => "Hangul",
            Self::Japanese => "Japanese",
            Self::Han => "Han",
            Self::Arabic => "Arabic",
            Self::Hebrew => "Hebrew",
            Self::Devanagari => "Devanagari",
            Self::Thai => "Thai",
        }
    }

    fn holds(self, c: char) -> bool {
        let u = c as u32;
        let han = (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u);
        match self {
            Self::Cyrillic => (0x0400..=0x052F).contains(&u),
            Self::Greek => (0x0370..=0x03FF).contains(&u) || (0x1F00..=0x1FFF).contains(&u),
            Self::Hangul => {
                (0xAC00..=0xD7AF).contains(&u)
                    || (0x1100..=0x11FF).contains(&u)
                    || (0x3130..=0x318F).contains(&u)
            }
            Self::Japanese => (0x3040..=0x30FF).contains(&u) || han,
            Self::Han => han,
            Self::Arabic => (0x0600..=0x06FF).contains(&u) || (0x0750..=0x077F).contains(&u),
            Self::Hebrew => (0x0590..=0x05FF).contains(&u),
            Self::Devanagari => (0x0900..=0x097F).contains(&u),
            Self::Thai => (0x0E00..=0x0E7F).contains(&u),
        }
    }
}

/// Whether `reply` can be a translation of `turn` into the language `code` (#117). Two rules, in
/// Rust so the prompt cannot be talked out of them: the reply is at most 2.5× the turn's
/// characters plus 20 (a model that answered instead of translating writes far more), and, for a
/// target written in a script other than Latin, most of its letters are in that script (a
/// loanword or a brand name in Latin letters is fine; a whole reply in them is not). For a
/// Latin-script target there is no script rule: English and German cannot be told apart cheaply.
/// `Err` is the detail for the error, starting with [`OFF_SCRIPT`].
pub fn check_translation(turn: &str, reply: &str, code: &str) -> Result<(), String> {
    let turn_chars = turn.trim().chars().count();
    let reply_chars = reply.trim().chars().count();
    let most = turn_chars * LENGTH_RATIO / 2 + SLACK_CHARS;
    if reply_chars > most {
        return Err(format!(
            "{OFF_SCRIPT}: {reply_chars} characters for a turn of {turn_chars} (at most {most})"
        ));
    }
    if let Some(script) = Script::of(code) {
        let letters = reply.chars().filter(|c| c.is_alphabetic()).count();
        let in_script = reply.chars().filter(|&c| script.holds(c)).count();
        if letters > 0 && in_script * 2 < letters {
            return Err(format!(
                "{OFF_SCRIPT}: {in_script} of {letters} letters are {} ({code})",
                script.name()
            ));
        }
    }
    Ok(())
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
        LlmError::MalformedStream { detail, .. } if detail.starts_with(OFF_SCRIPT) => {
            "off-script reply".to_owned()
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
    const CODE: &str = "es";
    const BUDGET: Duration = Duration::from_millis(4000);
    /// The live failure of 2026-10-08 (#117): the turn, and what `llama3.1:8b` replied for `ru`.
    const FRAGMENT: &str = "Back to English. And no input.";
    const INVENTED: &str = "I'm a quick learner and I can pick up new things fast. I've been working with computers for years, so I think I'd be a good fit for this job.";

    fn reply(text: &str) -> MockProvider {
        MockProvider::new(vec![Step::Text(text.into()), Step::Done])
    }

    async fn run(mock: &MockProvider) -> Result<Translation, LlmError> {
        translate(mock, SAID, TARGET, CODE, BUDGET, CancellationToken::new()).await
    }

    async fn run_as(turn: &str, reply_text: &str, code: &str) -> Result<Translation, LlmError> {
        let mock = reply(reply_text);
        translate(
            &mock,
            turn,
            &format!("({code})"),
            code,
            BUDGET,
            CancellationToken::new(),
        )
        .await
    }

    /// #117: the live failure is rejected: an English job-interview answer for a Russian target,
    /// nearly five times the turn. Nothing comes back to speak, and the status text is short.
    #[tokio::test]
    async fn the_invented_english_answer_for_a_russian_target_is_rejected() {
        let err = run_as(FRAGMENT, INVENTED, "ru").await.unwrap_err();
        match &err {
            LlmError::MalformedStream { detail, .. } => {
                assert!(detail.starts_with(OFF_SCRIPT), "{detail}");
                assert!(detail.contains("141 characters"), "{detail}");
            }
            other => panic!("{other}"),
        }
        assert_eq!(failure_text(&err), "translate failed: off-script reply");
        // Short enough, but Latin letters for a Cyrillic target.
        let err = run_as(SAID, "I would start with a token bucket.", "ru")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LlmError::MalformedStream { detail, .. } if detail.starts_with(OFF_SCRIPT) && detail.contains("Cyrillic")),
            "{err}"
        );
        // The right script, but four times the turn: an answer, not a translation.
        let long = "Ich würde mit einem Token-Bucket pro API-Schlüssel anfangen. ".repeat(4);
        let err = run_as(SAID, &long, "de").await.unwrap_err();
        assert!(
            matches!(&err, LlmError::MalformedStream { detail, .. } if detail.starts_with(OFF_SCRIPT)),
            "{err}"
        );
    }

    /// #117: real translations pass: Russian, German (1.6× the English) and Spanish of the same
    /// line, a one-word reply, and Russian with a brand name in Latin letters.
    #[tokio::test]
    async fn real_translations_short_long_and_with_a_latin_brand_name_pass() {
        for (turn, reply_text, code) in [
            (FRAGMENT, "Назад на английский. И никакого входа.", "ru"),
            (
                SAID,
                "Ich würde mit einem Token-Bucket pro API-Schlüssel anfangen, gespeichert in Redis.",
                "de",
            ),
            (
                SAID,
                "Empezaría con un token bucket por clave de API, guardado en Redis.",
                "es",
            ),
            ("Yes.", "Ja.", "de"),
            ("Hello.", "Привет.", "ru"),
            (
                SAID,
                "Я бы начал с token bucket на каждый API-ключ, со счётчиками в Redis.",
                "ru",
            ),
            (
                SAID,
                "我会从每个 API 密钥一个令牌桶开始，计数器放在 Redis 里。",
                "zh-Hans",
            ),
            (
                SAID,
                "各APIキーごとにトークンバケットから始め、カウンターはRedisに置きます。",
                "ja",
            ),
        ] {
            let t = run_as(turn, reply_text, code)
                .await
                .unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!(t.text, reply_text);
        }
        // No letters at all: nothing to judge the script by.
        assert!(run_as("...", "...", "ru").await.is_ok());
    }

    #[test]
    fn the_check_alone_reads_codes_with_regions_and_unknown_ones_as_latin() {
        assert!(check_translation(SAID, "Назад.", "ru-RU").is_ok());
        assert!(check_translation(SAID, "Back.", "ru_RU").is_err());
        assert!(
            check_translation(SAID, "Back.", "xx").is_ok(),
            "unknown: no script rule"
        );
        assert!(check_translation(SAID, "Back.", "").is_ok());
        assert_eq!(
            check_translation("abc", &"x".repeat(28), "de"),
            Err(format!(
                "{OFF_SCRIPT}: 28 characters for a turn of 3 (at most 27)"
            ))
        );
        assert!(check_translation("abc", &"x".repeat(27), "de").is_ok());
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
        // #117: no role to play. The text is a transcript fragment, to be translated even so.
        assert!(system.contains("fragment"), "{system}");
        assert!(system.contains("Never answer it"), "{system}");
        for word in ["candidate", "interview", "Candidate", "Interview"] {
            assert!(!system.contains(word), "{word}: {system}");
        }
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
        // A turn as long as the reply, so #117's length rule is not what cuts it.
        let turn = "This is a rather long test sentence for the limit. ".repeat(20);
        let t = run_as(&turn, &long, CODE).await.unwrap();
        assert!(t.text.chars().count() <= TRANSLATION_CHARS, "{}", t.text);
        assert!(t.text.ends_with("límite."), "cut at a sentence: {}", t.text);
        assert!(matches!(
            t.flags.as_slice(),
            [Flag { field, kind: FlagKind::Truncated { .. } }] if field == "translation"
        ));
        let words = "palabra ".repeat(120);
        let t = run_as(&turn, &words, CODE).await.unwrap();
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
            CODE,
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
