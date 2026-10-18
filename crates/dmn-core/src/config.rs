//! Typed `config.toml`. Every field has the default from docs/SPEC.md; unknown keys are errors so
//! a typo never silently falls back to a default.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use dmn_session::{CodingSolutions, SpeakLanguage, VoiceMode};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot serialise config: {0}")]
    Serialise(#[from] toml::ser::Error),
    #[error("{0} already exists; pass --force to overwrite")]
    Exists(String),
    /// An in-place edit of the config file (`dmn speak enroll`) that could not be made safely.
    #[error("{0}")]
    Edit(String),
    /// Keys that parse but cannot go together ([`Config::check`]).
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub audio: Audio,
    pub stt: Stt,
    pub detect: Detect,
    pub llm: Llm,
    pub lookup: Lookup,
    pub assist: Assist,
    pub speak: Speak,
    pub ui: Ui,
    pub privacy: Privacy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// PipeWire `node.name`, or "default" for the default source.
    pub mic: String,
    pub system_audio: bool,
    /// `application.name` to match; "" means the default sink monitor.
    pub app_stream: String,
    /// #60: drop mic text that copies the call within 300 ms, for a mic that hears the call
    /// (speakers, a laptop without echo cancellation). Off by default (#125): it holds each of
    /// the user's finals up to 1.5 s (4 s while the call is talking) waiting for the copy, and
    /// translate mode (#114) waits with it; on headphones there is nothing to catch.
    pub echo_guard: bool,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            mic: "default".into(),
            system_audio: true,
            app_stream: "Firefox".into(),
            echo_guard: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    pub engine: SttEngine,
    /// whisper model name, resolved under `Paths::models_dir`.
    pub model: String,
    pub threads: u16,
    /// Silero since #63. On the four fixtures (`just eval`, 500 ms close) Silero scored recall
    /// 1.00 / precision 1.00 against energy's 0.96 / 0.98, and #62 found it holds utterance
    /// bounds under keyboard clicks (80 of 92 turns) where energy loses almost all of them (1 of
    /// 92). Its cost is ~0.1 s later utterance ends, which `vad_close_ms` buys back. Needs the
    /// model: `dmn doctor --fetch silero`.
    pub vad: Vad,
    /// Trailing silence that ends an utterance, in ms. Every ms here is a ms of question-end →
    /// Approach latency; too short splits questions at a pause. #16 measured the fixtures at 1×
    /// (8 threads, energy): 700 → p50 1.96 s, 600 → 2.01 s, 500 → 1.78–1.85 s with recall 0.97
    /// and precision 0.83; live session 5 at 700 missed the 2.0 s p50 by 0.11 s. #63 (Silero,
    /// split questions continued): 500 → p50 1.93 s sysdesign / 2.02 s continuous, recall 1.00
    /// / precision 1.00; 400 → 1.83 s / 1.82 s, recall 0.96 / precision 0.94. 400 is the close
    /// that keeps both fixtures inside the 2.0 s p50.
    pub vad_close_ms: u16,
    /// Where whisper runs (#61): `cpu`, `gpu` (a GPU build's device, else the CPU with a
    /// warning), or `auto` (the GPU when there is one, quietly the CPU otherwise).
    pub device: SttDevice,
    /// The audio's language (#77): `en` (English only, the default and the M1 path), `auto` (a
    /// multilingual model detects each utterance's language and translates its text to
    /// English) or an ISO 639-1 code such as `es` (translate from that language, no
    /// detection). Anything but `en` needs a multilingual model that translates, `small`,
    /// `medium` or `large-v3`: the `.en` files cannot, and `large-v3-turbo` was trained without translation
    /// and returns the original language. `dmn doctor` checks the pair.
    pub language: SttLanguage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SttDevice {
    #[default]
    Cpu,
    Gpu,
    Auto,
}

/// `[stt].language` (#77), written as the plain string `en`, `auto` or a language code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub enum SttLanguage {
    #[default]
    English,
    Auto,
    /// An ISO 639-1 code (`es`, `de`, ...). Whether whisper knows it is checked when the model
    /// loads, so this layer only insists on the shape.
    Code(String),
}

impl SttLanguage {
    /// The config spelling.
    pub fn label(&self) -> &str {
        match self {
            SttLanguage::English => "en",
            SttLanguage::Auto => "auto",
            SttLanguage::Code(code) => code,
        }
    }

    pub fn is_english(&self) -> bool {
        matches!(self, SttLanguage::English)
    }
}

impl TryFrom<String> for SttLanguage {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl std::str::FromStr for SttLanguage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "en" => Ok(SttLanguage::English),
            "auto" => Ok(SttLanguage::Auto),
            code if (2..=3).contains(&code.len())
                && code.bytes().all(|b| b.is_ascii_lowercase()) =>
            {
                Ok(SttLanguage::Code(code.to_owned()))
            }
            other => Err(format!(
                "[stt].language {other:?} is not \"en\", \"auto\" or a two-letter language code such as \"es\""
            )),
        }
    }
}

impl From<SttLanguage> for String {
    fn from(l: SttLanguage) -> Self {
        l.label().to_owned()
    }
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            engine: SttEngine::WhisperCpp,
            model: "small.en".into(),
            // Per lane. #15 measured `dmn run --replay` on the sysdesign fixture at 1×
            // (Ryzen 7700X, small.en on CPU): question + Approach p50 2.29 s at 4 threads,
            // 1.95 s at 8. The final whisper pass is the stage threads move.
            threads: 8,
            vad: Vad::Silero,
            vad_close_ms: 400,
            device: SttDevice::Cpu,
            language: SttLanguage::English,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttEngine {
    #[serde(rename = "whisper.cpp")]
    WhisperCpp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vad {
    Energy,
    /// Silero via ONNX Runtime; M2.
    Silero,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Detect {
    pub sensitivity: Sensitivity,
    /// Whose questions are detected and answered (#90). A detector input, like `sensitivity`:
    /// it selects eligible lanes, and never changes a transcript label or the audio routing.
    pub question_source: QuestionSource,
}

/// `[detect].question_source` (#90): the mic lane (`you`), the call's output lane
/// (`them`, the default) or either (`both`). Fixed for a session, like `[stt].language`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuestionSource {
    #[default]
    /// Spelled `interviewer` before #128; a config that still says so keeps working.
    #[serde(alias = "interviewer")]
    Them,
    You,
    Both,
}

impl QuestionSource {
    /// The config-file spelling, for the status line and `dmn config show`.
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionSource::Them => "them",
            QuestionSource::You => "you",
            QuestionSource::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    Eager,
    #[default]
    Balanced,
    Strict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    /// Name of an environment variable holding the API key. Never the key itself.
    pub api_key_env: String,
    pub answer_length: AnswerLength,
    pub fast_timeout_ms: u64,
    /// The slow phase's deadline (#55; SPEC: 20 s). `0` turns the slow phase off: the six
    /// sections are never asked for, and the model serves only the fast phase.
    pub slow_timeout_ms: u64,
    /// Ollama context window dmn asks for (`options.num_ctx`, #61); `0` leaves the server's own
    /// (`OLLAMA_CONTEXT_LENGTH`). A value different from the server's makes Ollama reload the
    /// model when another client used the default in between.
    pub num_ctx: u32,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            provider: Provider::Ollama,
            // Measured on the dev machine 2026-09-21 (see CLAUDE.md); the spec's llama3.1:8b baseline
            // misclassifies question kinds and qwen3.6:27b misses the 2 s budget.
            model: "qwen3.6:35b-a3b".into(),
            base_url: "http://localhost:11434".into(),
            api_key_env: String::new(),
            answer_length: AnswerLength::Concise,
            fast_timeout_ms: 4000,
            slow_timeout_ms: 20000,
            num_ctx: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Ollama,
    OpenAI,
    Anthropic,
}

impl Provider {
    /// The config-file spelling, for logs, notices and the TUI's model dot.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Ollama => "ollama",
            Provider::OpenAI => "openai",
            Provider::Anthropic => "anthropic",
        }
    }

    /// True when transcript text is sent off the machine. Settings and `dmn doctor` say so in words.
    pub fn is_remote(self) -> bool {
        !matches!(self, Provider::Ollama)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerLength {
    /// Five approach lines.
    Concise,
    /// Three approach lines.
    Minimal,
}

/// The web lookup (#102): when the fast phase says an answer depends on what is current and names
/// a search query (#101), send that query, and nothing else, to a web search and answer once more
/// from the results. Off by default: with it on, a search query leaves the machine even with a
/// local model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lookup {
    pub enabled: bool,
    pub provider: LookupProvider,
    /// Where the search goes ([`Lookup::endpoint`]): Ollama's own service by default; for
    /// `searxng`, left at that default (or empty), `http://localhost:8888`. Tests use loopback.
    pub base_url: String,
    /// Name of an environment variable holding the search key. Never the key itself. Ollama's
    /// search needs one; SearXNG has none, and this is ignored for it.
    pub api_key_env: String,
    /// The whole search: past it the first answer stays and the status line says `lookup
    /// failed`. Left out of the file, it depends on the provider ([`Lookup::timeout`]): 3000 for
    /// `ollama`, 6000 for `searxng`, because a stock SearXNG waits up to its own 3 s
    /// `request_timeout` on slow engines and 3000 would lose exactly those searches. Written in
    /// the file, it is used as written. `dmn config show` prints the value in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Results asked for, 1–10; each goes into the prompt as a title, a 300-character snippet
    /// and its domain. 5 since PR #104's measurement: with 3, the page that named the newest
    /// release was often not among them.
    pub max_results: u8,
}

impl Default for Lookup {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: LookupProvider::Ollama,
            base_url: OLLAMA_SEARCH_URL.into(),
            api_key_env: "OLLAMA_API_KEY".into(),
            timeout_ms: None,
            max_results: 5,
        }
    }
}

/// `[lookup].provider`: whose web search answers (#102). Each is one `dmn_llm::WebSearch`
/// implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LookupProvider {
    /// Ollama's hosted web search, with a key from an ollama.com account.
    #[default]
    Ollama,
    /// The user's own SearXNG instance (on PR #104): no account, no key.
    Searxng,
}

impl LookupProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            LookupProvider::Ollama => "ollama",
            LookupProvider::Searxng => "searxng",
        }
    }

    /// True when the provider needs `api_key_env` to name a key.
    pub fn needs_key(self) -> bool {
        matches!(self, LookupProvider::Ollama)
    }
}

/// `[lookup].timeout_ms` left out, for `ollama`.
pub const LOOKUP_TIMEOUT_MS: u64 = 3000;
/// `[lookup].timeout_ms` left out, for `searxng` (see [`Lookup::timeout_ms`]).
pub const SEARXNG_TIMEOUT_MS: u64 = 6000;

/// `[lookup].base_url`'s default: Ollama's web search.
const OLLAMA_SEARCH_URL: &str = "https://ollama.com";
/// Where a SearXNG is unless `base_url` says otherwise: its Docker image's port, on this machine.
pub const SEARXNG_URL: &str = "http://localhost:8888";

impl Lookup {
    /// Where the search goes, without a trailing slash: `base_url`, or for `searxng` while
    /// `base_url` is empty or still Ollama's default, [`SEARXNG_URL`] (an `[lookup]` that only
    /// says `provider = "searxng"` means a SearXNG on this machine, not ollama.com).
    pub fn endpoint(&self) -> String {
        let url = self.base_url.trim().trim_end_matches('/');
        match self.provider {
            LookupProvider::Searxng if url.is_empty() || url == OLLAMA_SEARCH_URL => {
                SEARXNG_URL.to_owned()
            }
            _ => url.to_owned(),
        }
    }

    /// The search key from the variable `api_key_env` names. `None` when unset or empty.
    pub fn api_key(&self) -> Option<ApiKey> {
        self.api_key_from(|name| std::env::var(name).ok())
    }

    /// The search's deadline: `timeout_ms` as written, or when it is left out
    /// [`LOOKUP_TIMEOUT_MS`] for `ollama` and [`SEARXNG_TIMEOUT_MS`] for `searxng`.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.timeout_ms())
    }

    /// [`Lookup::timeout`] in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(match self.provider {
            LookupProvider::Ollama => LOOKUP_TIMEOUT_MS,
            LookupProvider::Searxng => SEARXNG_TIMEOUT_MS,
        })
    }

    /// Same as [`Lookup::api_key`] with the environment lookup injected (tests use a closure).
    pub fn api_key_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<ApiKey> {
        if self.api_key_env.is_empty() {
            return None;
        }
        lookup(&self.api_key_env)
            .filter(|v| !v.is_empty())
            .map(ApiKey::new)
    }

    /// The sentence `dmn run`, `dmn replay` and `dmn config show` print while the lookup is on
    /// (SPEC § Privacy rules): what leaves, where to, and when.
    pub fn privacy_notice(&self) -> Option<String> {
        self.enabled.then(|| {
            let to = match self.provider {
                LookupProvider::Ollama => format!("Ollama's web search at {}", self.endpoint()),
                LookupProvider::Searxng => format!(
                    "your SearXNG at {}, which forwards it to the search engines it is set up with",
                    self.endpoint()
                ),
            };
            format!(
                "web lookup on: when an answer depends on what is current, its search query (a \
                 few words the model writes, no transcript) goes to {to} ([lookup].enabled = true)"
            )
        })
    }
}

/// Interview assistance (#79): the layer over transcription that detects questions and asks the
/// LLM for an Approach. Off, Daemon is a live transcriber / translator only: no detector, no
/// context, no LLM request, and the transcript takes the screen. `dmn run --no-assist` starts a
/// run with it off; Ctrl+Shift+A toggles it during the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assist {
    pub enabled: bool,
}

impl Default for Assist {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Speak (#107): synthesize text in the enrolled voice clone through a TTS server on this
/// machine (chatterbox-tts-api, OpenAI-compatible). Off by default; `dmn speak` refuses to run
/// while `enabled` is false or `tts_url` is another host. #109: `t` in `dmn run` translates the
/// user's last turn into `target` and speaks it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Speak {
    pub enabled: bool,
    /// #122: which TTS server `tts_url` is: `chatterbox` (chatterbox-tts-api, the default) or
    /// `qwen3` (Qwen3-TTS on vLLM-Omni, `sidecars/qwen3-tts/`: first audio in under 0.1 s, and
    /// one enrolled voice speaks every language it knows).
    pub engine: SpeakEngine,
    /// Base URL of the TTS server, like `http://localhost:4123`.
    pub tts_url: String,
    /// Path to the enrolled reference clip (`dmn speak enroll` writes it); "" = not enrolled.
    pub voice_ref: String,
    /// Where the server should run (`cpu` | `cuda`), a hint for the server setup doc only —
    /// dmn never reads it. The 35B Ollama model takes 22 of the 4090's 24 GB, so GPU TTS is
    /// opt-in: check `nvidia-smi` first.
    pub device: String,
    /// #109: the language `t` translates into and speaks, a two- or three-letter code (`es`):
    /// the interview's language, and the one the voice was enrolled with (`dmn speak enroll
    /// --language`), since the server speaks each voice in its enrollment language only. No
    /// default: "" is unset, and `enabled = true` without it is a config error naming the key.
    #[serde(deserialize_with = "language_code")]
    pub target: String,
    /// #113: the Ollama model `t` translates with, under `[llm]`'s provider, endpoint and
    /// timeouts. Left out (or blank), `[llm].model`. A small one (`llama3.1:8b`, 4.6 GB) keeps
    /// the answer model out of memory while assistance is off ([`Config::translator_llm`]).
    pub model: Option<String>,
    /// #113: the TTS model's sampling temperature, sent with every speech request. 0.6 by
    /// default (the server's own is 0.8): lower makes its end-of-text noise burst rarer. The
    /// server takes 0.05–5.0.
    pub temperature: f64,
    /// #113: the TTS model's classifier-free guidance weight (pace), 0.0–1.0; sent only when
    /// set (the server's default is 0.5).
    pub cfg_weight: Option<f64>,
    /// #113: the TTS model's emotion intensity, 0.25–2.0; sent only when set (the server's
    /// default is 0.5).
    pub exaggeration: Option<f64>,
    /// #114: what `dmn-voice` carries when `dmn run` starts: `passthrough` (the default: your
    /// own voice, live; `t` speaks one turn translated over it) or `translate` (every turn you
    /// finish is translated into `target` and spoken instead). `l` switches during the run.
    pub mode: VoiceMode,
    /// #115: the languages the picker offers in `dmn run`, each with the voice enrolled for it
    /// (`languages = [{ code = "de", voice = "voice-de" }, ...]`; `dmn speak enroll --language
    /// de` adds or updates an entry). With the list present `target` must be one of its codes
    /// and names the language at start; left out, `target` and the single `voice_ref` are the
    /// one language, as before #115.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "language_list"
    )]
    pub languages: Vec<SpeakLanguage>,
}

/// `[speak].temperature`'s default (#113).
pub const SPEAK_TEMPERATURE: f64 = 0.6;

/// `[speak].engine` (#122): the TTS server behind `tts_url`. An unknown value is a parse error
/// that names both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SpeakEngine {
    /// chatterbox-tts-api: a voice speaks the one language it was enrolled with; ~1-2 s to the
    /// first sound of a sentence on the GPU.
    #[default]
    Chatterbox,
    /// Qwen3-TTS 1.7B Base on vLLM-Omni: cross-lingual (one clip, ten languages), streamed,
    /// first audio in 0.04-0.08 s on the 4090.
    Qwen3,
}

impl SpeakEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            SpeakEngine::Chatterbox => "chatterbox",
            SpeakEngine::Qwen3 => "qwen3",
        }
    }
}

/// The shape `[speak].target` and a `languages` code take, as `[stt].language` does: two or
/// three lowercase letters. Whether whisper and the TTS server know it is checked when `dmn run`
/// starts.
pub fn is_language_code(code: &str) -> bool {
    (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())
}

/// `[speak].target`: "" (unset) or a code of the shape [`is_language_code`] takes.
fn language_code<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let raw = String::deserialize(d)?;
    let code = raw.trim();
    if code.is_empty() || is_language_code(code) {
        return Ok(code.to_owned());
    }
    Err(serde::de::Error::custom(format!(
        "[speak].target {raw:?} is not a two- or three-letter language code such as \"es\""
    )))
}

/// `[speak].languages` (#115): every code of the shape [`is_language_code`] takes, every voice
/// a name, no code twice.
fn language_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SpeakLanguage>, D::Error> {
    let raw: Vec<SpeakLanguage> = Vec::deserialize(d)?;
    let mut list = Vec::with_capacity(raw.len());
    for entry in raw {
        let code = entry.code.trim();
        if !is_language_code(code) {
            return Err(serde::de::Error::custom(format!(
                "[speak].languages: code {:?} is not a two- or three-letter language code such as \"es\"",
                entry.code
            )));
        }
        let voice = entry.voice.trim();
        if voice.is_empty() {
            return Err(serde::de::Error::custom(format!(
                "[speak].languages: the entry for {code:?} needs `voice`, the name of the voice enrolled for it (dmn speak enroll --wav <clip> --language {code})"
            )));
        }
        if list.iter().any(|l: &SpeakLanguage| l.code == code) {
            return Err(serde::de::Error::custom(format!(
                "[speak].languages lists {code:?} twice; one voice per language"
            )));
        }
        list.push(SpeakLanguage {
            code: code.to_owned(),
            voice: voice.to_owned(),
        });
    }
    Ok(list)
}

/// The voice name the TTS server knows a `voice_ref` clip by: its file stem (#107: the
/// clip is stored as `<data dir>/<name>.wav`), `voice` when the path has none.
pub fn voice_ref_name(voice_ref: &str) -> String {
    Path::new(voice_ref)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "voice".to_owned())
}

impl Speak {
    /// `tts_url` is on this machine (a loopback name or address). `dmn speak` refuses to run
    /// otherwise: the text, and at enrollment the voice clip, would leave the machine, and
    /// SPEC § Privacy promises they do not.
    pub fn is_local(&self) -> bool {
        is_loopback_url(self.tts_url.trim())
    }

    /// #115: the languages a run can speak, each with its voice: `languages` when the list is
    /// present, else the one `target` + `voice_ref` pair (the shape before #115); empty while
    /// `target` is unset.
    pub fn voices(&self) -> Vec<SpeakLanguage> {
        if !self.languages.is_empty() {
            return self.languages.clone();
        }
        if self.target.is_empty() {
            return Vec::new();
        }
        vec![SpeakLanguage {
            code: self.target.clone(),
            voice: voice_ref_name(&self.voice_ref),
        }]
    }

    /// #115: the voice enrolled for `code`, from [`Speak::voices`].
    pub fn voice_for(&self, code: &str) -> Option<String> {
        self.voices()
            .into_iter()
            .find(|l| l.code == code)
            .map(|l| l.voice)
    }
}

impl Default for Speak {
    fn default() -> Self {
        Self {
            enabled: false,
            engine: SpeakEngine::Chatterbox,
            tts_url: "http://localhost:4123".into(),
            voice_ref: String::new(),
            device: "cpu".into(),
            target: String::new(),
            model: None,
            temperature: SPEAK_TEMPERATURE,
            cfg_weight: None,
            exaggeration: None,
            mode: VoiceMode::Passthrough,
            languages: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    pub always_on_top: bool,
    /// Launch under XWayland so keep-above works on compositors that ignore it.
    pub force_x11: bool,
    pub opacity: f32,
    pub never_auto_expand: bool,
    /// What Deeper Answer may hold for a coding question (#50): `hints` (insight, data
    /// structure, edge cases, complexity, no complete code) or `full` (a worked solution too).
    /// `dmn run --solutions` overrides it for one run and `s` toggles it during a call.
    pub coding_solutions: CodingSolutions,
    /// `dmn run` reads the mouse (#95): the wheel scrolls the open section and a click on a
    /// section row opens it. `false` enables no capture, so the terminal keeps selection and
    /// its own wheel.
    pub mouse: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            always_on_top: true,
            force_x11: false,
            opacity: 1.0,
            never_auto_expand: true,
            coding_solutions: CodingSolutions::Hints,
            mouse: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    /// Write WAVs of the session under `sessions/` and show a red REC dot.
    pub record_audio: bool,
    pub keep_transcripts: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Self {
            record_audio: false,
            keep_transcripts: true,
        }
    }
}

/// An API key read from the environment. Debug and Display never print it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The commented config `dmn doctor --init` writes. Parsing it yields exactly `Config::default()`
/// (checked by a test), so the comments are the single place defaults are explained.
pub const TEMPLATE: &str = r#"# Daemon (dmn) configuration. Every key is optional; missing keys take the defaults shown.
# Docs: docs/SPEC.md § Data, config and privacy.

[audio]
mic = "default"                      # PipeWire node.name, or "default"
system_audio = true                  # capture the other person from an app's output stream
app_stream = "Firefox"               # application.name to capture; "" = whole default sink monitor
echo_guard = false                   # true when the mic can hear the call (speakers): drops mic text that copies the call, at the cost of a 1.5 s hold on each of your sentences (#125)

[stt]
engine = "whisper.cpp"
model = "small.en"                   # under ~/.cache/dmn/models/; `dmn doctor --fetch small.en`
threads = 8                          # whisper threads per lane; the final pass is most of the 2 s budget
vad = "silero"                       # silero | energy; silero needs `dmn doctor --fetch silero`
vad_close_ms = 400                   # silence that ends an utterance; shorter = faster, more splits
device = "cpu"                       # cpu | gpu | auto: where whisper runs (gpu needs --features vulkan|cuda)
language = "en"                      # en | auto | es, de, ...: auto detects and translates to English; needs a multilingual model (small)

[detect]
sensitivity = "balanced"             # eager | balanced | strict
question_source = "them"             # them | you | both: whose questions are detected and answered

[llm]
provider = "ollama"                  # ollama | openai | anthropic. Only ollama keeps transcripts local.
model = "qwen3.6:35b-a3b"
base_url = "http://localhost:11434"
api_key_env = ""                     # NAME of an env var holding the key; never the key itself
answer_length = "concise"            # concise (5 lines) | minimal (3 lines)
fast_timeout_ms = 4000
slow_timeout_ms = 20000              # the six expandable sections; 0 turns the slow phase off
num_ctx = 0                          # Ollama context window to ask for; 0 = the server default

[lookup]
enabled = false                      # true = a question whose answer depends on what is current is searched on the web
provider = "ollama"                  # ollama (hosted, needs an ollama.com key) | searxng (your own instance, no key)
base_url = "https://ollama.com"      # with provider = "searxng", left as is: http://localhost:8888
api_key_env = "OLLAMA_API_KEY"       # NAME of an env var holding the key; never the key itself; unused by searxng
# timeout_ms = 3000                  # past this the first answer stays, nothing retried; left out: 3000 for ollama, 6000 for searxng
max_results = 5                      # 1-10 results, each a title, a 300-character snippet and its domain

[assist]
enabled = true                       # false = transcribe / translate only: no detector, no LLM (Ctrl+Shift+A toggles)

[speak]
enabled = false                      # speak text in your cloned voice via a local TTS server (`dmn speak`, #107)
engine = "chatterbox"                # chatterbox | qwen3 (Qwen3-TTS on vLLM-Omni, sidecars/qwen3-tts, tts_url = "http://localhost:4124"; #122)
tts_url = "http://localhost:4123"    # the TTS server on this machine (no other host); README § Speak
voice_ref = ""                       # enrolled reference clip; `dmn speak enroll --wav <clip>` sets it
device = "cpu"                       # cpu | cuda: a hint for the server setup; check nvidia-smi first
target = ""                          # dmn run: `t` speaks your last turn in this language (es, de, ...); the voice's enroll --language; needed with enabled = true
# model = "llama3.1:8b"              # the Ollama model `t` translates with; left out: [llm].model. A small one keeps the answer model out of VRAM with assistance off
temperature = 0.6                    # the TTS model's sampling temperature (server default 0.8); lower = fewer noise bursts after a sentence
# cfg_weight = 0.5                   # the TTS model's pace, 0.0-1.0; left out: the server's
# exaggeration = 0.5                 # the TTS model's emotion intensity, 0.25-2.0; left out: the server's
mode = "passthrough"                 # dmn run: passthrough = your own voice live into dmn-voice, `t` speaks one turn translated; translate = every turn you finish goes out translated (`l` toggles, #114)
# languages = [{ code = "de", voice = "voice-de" }, { code = "es", voice = "voice-es" }]   # dmn run: the languages `v` picks between, each with its enrolled voice (`enroll --language de` adds one); target must be one of them (#115)

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above on GNOME Wayland
opacity = 1.0                        # 0.7 – 1.0
never_auto_expand = true
coding_solutions = "hints"           # hints | full: worked solutions in Deeper Answer (s toggles)
mouse = true                         # dmn run: wheel scrolls an open section, a click opens one; false leaves the mouse to the terminal

[privacy]
record_audio = false                 # write WAVs of the session (shows a red REC dot)
keep_transcripts = true
"#;

impl Config {
    /// Parse a TOML document. Missing keys take defaults; unknown keys are errors, and so is a
    /// combination no run can use ([`Config::check`]).
    pub fn from_toml(text: &str, path_for_errors: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path_for_errors.to_owned(),
            source,
        })?;
        cfg.check().map_err(|message| ConfigError::Invalid {
            path: path_for_errors.to_owned(),
            message,
        })?;
        Ok(cfg)
    }

    /// Keys that are each valid but cannot go together. #109: `[speak].enabled = true` needs
    /// `target`, the language `t` speaks; it has no default, since it must be the language the
    /// voice was enrolled with. #115: with `languages` listed, `target` is one of its codes.
    pub fn check(&self) -> Result<(), String> {
        if self.speak.enabled && self.speak.target.is_empty() {
            return Err(
                "[speak].enabled = true needs `target` under [speak]: the language `t` speaks, \
                 a code such as \"es\", the one the voice was enrolled with \
                 (`dmn speak enroll --language es`)"
                    .into(),
            );
        }
        let target = &self.speak.target;
        if !target.is_empty()
            && !self.speak.languages.is_empty()
            && !self.speak.languages.iter().any(|l| &l.code == target)
        {
            let listed: Vec<&str> = self
                .speak
                .languages
                .iter()
                .map(|l| l.code.as_str())
                .collect();
            return Err(format!(
                "[speak].target = {target:?} is not one of [speak].languages ({}): set target to \
                 one of them, or enroll a voice for it (dmn speak enroll --wav <clip> --language {target})",
                listed.join(", ")
            ));
        }
        // #113: the TTS server's own ranges; outside them it refuses every speech request.
        let s = &self.speak;
        for (key, value, lo, hi) in [
            ("temperature", Some(s.temperature), 0.05, 5.0),
            ("cfg_weight", s.cfg_weight, 0.0, 1.0),
            ("exaggeration", s.exaggeration, 0.25, 2.0),
        ] {
            if let Some(v) = value
                && !(lo..=hi).contains(&v)
            {
                return Err(format!(
                    "[speak].{key} = {v} is outside {lo}-{hi}, the range the TTS server takes"
                ));
            }
        }
        Ok(())
    }

    /// #113: the model `t` translates with: `[speak].model`, or `[llm].model` when it is left
    /// out or blank.
    pub fn translator_model(&self) -> &str {
        match self.speak.model.as_deref().map(str::trim) {
            Some(m) if !m.is_empty() => m,
            _ => &self.llm.model,
        }
    }

    /// #113: whether translation has a model of its own, different from `[llm].model`: then a
    /// run that starts with assistance off loads only that one.
    pub fn translator_is_separate(&self) -> bool {
        self.translator_model() != self.llm.model
    }

    /// #113: `[llm]` as the translator uses it: the same provider, endpoint, key and timeouts,
    /// with [`Config::translator_model`] as the model.
    pub fn translator_llm(&self) -> Llm {
        Llm {
            model: self.translator_model().to_owned(),
            ..self.llm.clone()
        }
    }

    /// Load `path`. A missing file is the default config, not an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, &path.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Write the commented template. Refuses to overwrite unless `force`.
    ///
    /// Without `force` the file is created with `create_new`, so the check and the write are one
    /// atomic step: a config that appears in between is not clobbered, and a symlink pointing
    /// somewhere else is not followed (Copilot on PR #17).
    pub fn write_template(path: &Path, force: bool) -> Result<(), ConfigError> {
        let shown = path.display().to_string();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
                path: shown.clone(),
                source,
            })?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true);
        if force {
            opts.create(true).truncate(true);
        } else {
            opts.create_new(true);
        }
        let mut file = match opts.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ConfigError::Exists(shown));
            }
            Err(source) => {
                return Err(ConfigError::Write {
                    path: shown,
                    source,
                });
            }
        };
        std::io::Write::write_all(&mut file, TEMPLATE.as_bytes()).map_err(|source| {
            ConfigError::Write {
                path: shown,
                source,
            }
        })
    }

    /// This config with every value that depends on another written out as it takes effect
    /// (#102: `[lookup].timeout_ms` left out is the provider's own; #113: `[speak].model` left
    /// out is `[llm].model`): what `dmn config show`
    /// prints, so it shows what runs.
    pub fn effective(&self) -> Self {
        let mut cfg = self.clone();
        cfg.lookup.timeout_ms = Some(self.lookup.timeout_ms());
        // #113: the model `t` translates with, `[llm].model` when `[speak].model` is left out.
        cfg.speak.model = Some(self.translator_model().to_owned());
        cfg
    }

    /// The config as TOML (no comments).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Set `[speak].voice_ref` in the config file at `path`, keeping every other line, comment
    /// and section as it is (#107: `dmn speak enroll` records the enrolled clip here): the one
    /// `voice_ref` line is replaced in place, or added to `[speak]`, or a `[speak]` section is
    /// appended. A missing file is created from the template first. The edited text is parsed
    /// before anything is written, so a file this edit cannot handle is left untouched and the
    /// error says so. A symlinked config (a dotfiles repo) is edited where it points, and the
    /// write goes through a sibling temp file and a rename, keeping the file's permissions, so
    /// an interrupted enroll never leaves a truncated config.
    pub fn write_voice_ref(path: &Path, voice_ref: &str) -> Result<(), ConfigError> {
        let value = toml::Value::String(voice_ref.to_owned()).to_string();
        Self::edit_speak(
            path,
            |_| vec![("voice_ref", value.clone())],
            |c| c.speak.voice_ref == voice_ref,
        )
    }

    /// #115: set `[speak].target` to `code` in the config file at `path`, the same in-place
    /// edit as [`Config::write_voice_ref`] (the picker saves its choice here, so the next run
    /// starts with it).
    pub fn write_speak_target(path: &Path, code: &str) -> Result<(), ConfigError> {
        let value = toml::Value::String(code.to_owned()).to_string();
        Self::edit_speak(
            path,
            |_| vec![("target", value.clone())],
            |c| c.speak.target == code,
        )
    }

    /// #115: add `{ code, voice }` to `[speak].languages` in the config file at `path`, or
    /// update the entry for `code`, with the same in-place edit as [`Config::write_voice_ref`]
    /// (`dmn speak enroll --language`). A file with no list yet gets one that also holds its
    /// current `target` + `voice_ref` pair, so the target stays valid; a `target` still unset
    /// is set to `code`, so turning `[speak]` on needs nothing more.
    pub fn write_speak_language(path: &Path, code: &str, voice: &str) -> Result<(), ConfigError> {
        let wanted = |current: &Config| -> Vec<SpeakLanguage> {
            let mut list = current.speak.voices();
            match list.iter_mut().find(|l| l.code == code) {
                Some(entry) => entry.voice = voice.to_owned(),
                None => list.push(SpeakLanguage {
                    code: code.to_owned(),
                    voice: voice.to_owned(),
                }),
            }
            list
        };
        Self::edit_speak(
            path,
            |current| {
                let list = wanted(current);
                let mut edits = vec![("languages", inline_languages(&list))];
                if current.speak.target.is_empty() {
                    edits.push(("target", toml::Value::String(code.to_owned()).to_string()));
                }
                edits
            },
            |c| c.speak.languages == wanted(c) && !c.speak.target.is_empty(),
        )
    }

    /// The in-place edit behind the three writers above: `edits` gives the `[speak]` keys to
    /// set (each as its TOML value text) from the file as it is now, and `check` says whether
    /// the edited file parses to what was meant.
    fn edit_speak(
        path: &Path,
        edits: impl Fn(&Config) -> Vec<(&'static str, String)>,
        check: impl Fn(&Config) -> bool,
    ) -> Result<(), ConfigError> {
        let shown = path.display().to_string();
        if !path.exists() {
            Self::write_template(path, false)?;
        }
        let target = std::fs::canonicalize(path).map_err(|source| ConfigError::Read {
            path: shown.clone(),
            source,
        })?;
        let text = std::fs::read_to_string(&target).map_err(|source| ConfigError::Read {
            path: shown.clone(),
            source,
        })?;
        let current = Self::from_toml(&text, &shown)?;
        let mut edited = text.clone();
        let mut keys = Vec::new();
        for (key, value) in edits(&current) {
            edited = set_speak_key(&edited, key, &value);
            keys.push(format!("{key} = {value}"));
        }
        let parsed = Self::from_toml(&edited, &shown)?;
        if !check(&parsed) {
            return Err(ConfigError::Edit(format!(
                "{shown}: cannot set [speak] keys in place (is one a dotted key or an inline \
                 table?); write {} under [speak] by hand",
                keys.join(" and ")
            )));
        }
        let write_err = |source| ConfigError::Write {
            path: shown.clone(),
            source,
        };
        let mut tmp_name = target.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".dmn-tmp");
        let tmp = target.with_file_name(tmp_name);
        std::fs::write(&tmp, &edited).map_err(write_err)?;
        let renamed = std::fs::metadata(&target)
            .and_then(|m| std::fs::set_permissions(&tmp, m.permissions()))
            .and_then(|()| std::fs::rename(&tmp, &target));
        if let Err(e) = renamed {
            let _ = std::fs::remove_file(&tmp);
            return Err(write_err(e));
        }
        Ok(())
    }

    /// Read the API key from the env var named in `[llm].api_key_env`. `None` when unset or empty.
    pub fn api_key(&self) -> Option<ApiKey> {
        self.llm.api_key()
    }

    /// Same as [`Config::api_key`] with the environment lookup injected (tests use a closure).
    pub fn api_key_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<ApiKey> {
        self.llm.api_key_from(lookup)
    }
}

impl Llm {
    /// The API key from the env var `api_key_env` names. `None` when unset or empty.
    pub fn api_key(&self) -> Option<ApiKey> {
        self.api_key_from(|name| std::env::var(name).ok())
    }

    /// Same as [`Llm::api_key`] with the environment lookup injected (tests use a closure).
    pub fn api_key_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<ApiKey> {
        if self.api_key_env.is_empty() {
            return None;
        }
        lookup(&self.api_key_env)
            .filter(|v| !v.is_empty())
            .map(ApiKey::new)
    }

    /// The URL the provider talks to: `base_url`, or the provider's public API while
    /// `base_url` is still the Ollama default (an `[llm]` that only says `provider =
    /// "anthropic"` means Anthropic's API, not localhost:11434).
    pub fn endpoint(&self) -> String {
        let ollama_default = self.base_url.trim_end_matches('/') == Llm::default().base_url;
        match self.provider {
            Provider::OpenAI if ollama_default => "https://api.openai.com".into(),
            Provider::Anthropic if ollama_default => "https://api.anthropic.com".into(),
            _ => self.base_url.clone(),
        }
    }

    /// Transcript text leaves this machine (SPEC § Providers): a remote provider, or an
    /// endpoint whose host is not a loopback name or address. `dmn doctor` also resolves the
    /// host for its report; this is the quick check `dmn run` and `dmn config show` use.
    pub fn leaves_machine(&self) -> bool {
        self.provider.is_remote() || !is_loopback_url(&self.endpoint())
    }

    /// The sentence `dmn run` and `dmn config show` print when text leaves the machine.
    pub fn privacy_notice(&self) -> Option<String> {
        self.leaves_machine().then(|| {
            format!(
                "transcript text, retrieved chunks and the session summary leave this machine: \
                 [llm].provider = \"{}\" at {}",
                self.provider.as_str(),
                self.endpoint()
            )
        })
    }
}

/// `http://localhost:11434`, `http://127.0.0.1`, `http://[::1]:8080` and the like.
fn is_loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => authority.split(':').next().unwrap_or(authority),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// #115: `[speak].languages` as one inline line, `[{ code = "de", voice = "voice-de" }, ...]`,
/// each string quoted through `toml::Value` so any name is escaped correctly.
fn inline_languages(list: &[SpeakLanguage]) -> String {
    let entries: Vec<String> = list
        .iter()
        .map(|l| {
            format!(
                "{{ code = {}, voice = {} }}",
                toml::Value::String(l.code.clone()),
                toml::Value::String(l.voice.clone())
            )
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

/// `text` with `[speak].<key>` set to `quoted` (a TOML value as text): replace the key where it
/// stands (keeping an inline comment, and uncommenting a template line `# key = …` that stands
/// in for it), insert it under an existing `[speak]` header, or append a new section.
fn set_speak_key(text: &str, key: &str, quoted: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_speak = false;
    let mut section_seen = false;
    let mut written = false;
    // Blank lines trailing a section are held back so the key lands before them, not after.
    let mut blanks: Vec<String> = Vec::new();
    // `key = value`, or the template's commented-out `# key = value` standing in for it; the
    // commented line is only used when the section has no real one.
    let is_key = |candidate: &str| {
        candidate
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let has_real_key = {
        let mut in_speak = false;
        text.lines().map(str::trim_start).any(|trimmed| {
            if trimmed.starts_with('[') {
                in_speak = split_comment(trimmed).0.trim_end() == "[speak]";
                return false;
            }
            in_speak && is_key(trimmed)
        })
    };
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if in_speak && !written {
                out.push(format!("{key} = {quoted}"));
                written = true;
            }
            out.append(&mut blanks);
            in_speak = split_comment(trimmed).0.trim_end() == "[speak]";
            section_seen |= in_speak;
            out.push(line.to_owned());
            continue;
        }
        if trimmed.is_empty() {
            blanks.push(line.to_owned());
            continue;
        }
        out.append(&mut blanks);
        let commented_out = !has_real_key
            && trimmed
                .strip_prefix('#')
                .map(str::trim_start)
                .is_some_and(is_key);
        if in_speak && !written && (is_key(trimmed) || commented_out) {
            // A commented template line's own comment is what follows its value.
            let code = if commented_out { &trimmed[1..] } else { line };
            let (_, comment) = split_comment(code);
            let mut new_line = format!("{key} = {quoted}");
            if !comment.is_empty() {
                // The template's comment column, and at least one space before the `#`.
                new_line.push(' ');
                while new_line.len() < 39 {
                    new_line.push(' ');
                }
                new_line.push_str(comment);
            }
            out.push(new_line);
            written = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if in_speak && !written {
        out.push(format!("{key} = {quoted}"));
    }
    out.append(&mut blanks);
    if !section_seen {
        out.push(String::new());
        out.push("[speak]".to_owned());
        out.push(format!("{key} = {quoted}"));
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Split a TOML line into (code, comment) at the first `#` outside a quoted string; the
/// returned comment includes its `#`. Knows basic strings (`"..."`, with `\"` escapes) and
/// literal strings (`'...'`, no escapes).
fn split_comment(line: &str) -> (&str, &str) {
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (i, &b) in line.as_bytes().iter().enumerate() {
        match quote {
            Some(b'"') if escaped => escaped = false,
            Some(b'"') if b == b'\\' => escaped = true,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'#' => return (&line[..i], &line[i..]),
            None => {}
        }
    }
    (line, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from docs/SPEC.md § Data, config and privacy, verbatim.
    const SPEC_EXAMPLE: &str = r#"
[audio]
mic = "default"                      # pw node.name or "default"
system_audio = true
app_stream = "Firefox"               # application.name match; "" = sink monitor
echo_guard = false                   # true when the mic hears the call (#125)

[stt]
engine = "whisper.cpp"
model = "small.en"
threads = 8                          # per lane; 4 missed the 2 s p50 (#15)
vad = "silero"                       # silero | energy (#63: fixtures and keyboard noise)
vad_close_ms = 400                   # silence that ends an utterance; shorter = faster, more splits
device = "cpu"                       # cpu | gpu | auto: where whisper runs (gpu needs --features vulkan|cuda)
language = "en"                      # en | auto | es, de, ...: auto detects and translates to English; needs a multilingual model (small)

[detect]
sensitivity = "balanced"             # eager | balanced | strict
question_source = "them"             # them | you | both: whose questions trigger an answer

[llm]
provider = "ollama"                  # ollama | openai | anthropic
model = "qwen3.6:35b-a3b"           # measured 2026-09-21 on the dev 4090; see CLAUDE.md
base_url = "http://localhost:11434"
api_key_env = ""                     # name of an env var; never the key itself
answer_length = "concise"            # concise (5) | minimal (3)
fast_timeout_ms = 4000
slow_timeout_ms = 20000              # 0 turns the slow phase off
num_ctx = 0                          # Ollama context window to ask for; 0 = the server default

[lookup]
enabled = false                      # true = search the web when an answer depends on what is current
provider = "ollama"                  # ollama (hosted, key) | searxng (your own instance, no key)
base_url = "https://ollama.com"      # searxng: left as is, http://localhost:8888
api_key_env = "OLLAMA_API_KEY"       # name of an env var; never the key itself
# timeout_ms = 3000                  # left out: 3000 for ollama, 6000 for searxng
max_results = 5

[assist]
enabled = true                       # false = transcribe / translate only (Ctrl+Shift+A toggles)

[speak]
enabled = false                      # `dmn speak` (#107): clone-voice TTS through a local server
tts_url = "http://localhost:4123"    # chatterbox-tts-api (OpenAI-compatible); this machine only
voice_ref = ""                       # enrolled reference clip; `dmn speak enroll --wav <clip>` sets it
device = "cpu"                       # cpu | cuda: a hint for the server setup; check nvidia-smi first
target = ""                          # `t` (#109) speaks in this language (es, de, ...): the voice's enroll --language
# model = "llama3.1:8b"              # `t`'s translator (#113); left out: [llm].model
temperature = 0.6                    # TTS sampling temperature (#113); lower = fewer noise bursts
# cfg_weight = 0.5                   # TTS pace, 0.0-1.0; left out: the server's
# exaggeration = 0.5                 # TTS emotion, 0.25-2.0; left out: the server's
mode = "passthrough"                 # passthrough (your voice live into dmn-voice) | translate (every turn translated; `l` toggles, #114)
# languages = [{ code = "de", voice = "voice-de" }]   # `v` picks between these (#115); `enroll --language` adds one; target must be one

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above
opacity = 1.0
never_auto_expand = true
coding_solutions = "hints"           # hints | full (worked solutions in Deeper Answer)
mouse = true                         # dmn run: wheel scrolls an open section, a click opens one

[privacy]
record_audio = false                 # write WAVs of the session
keep_transcripts = true
"#;

    #[test]
    fn spec_example_round_trips_and_matches_defaults() {
        let parsed = Config::from_toml(SPEC_EXAMPLE, "spec").unwrap();
        let again = Config::from_toml(&parsed.to_toml().unwrap(), "again").unwrap();
        assert_eq!(parsed, again, "serialise → parse must be lossless");

        assert_eq!(
            parsed,
            Config::default(),
            "every key in the spec example is a default"
        );
    }

    /// #114: `[speak].mode` is `passthrough` unless the file says `translate`; anything else
    /// names both.
    #[test]
    fn speak_mode_defaults_to_passthrough_and_parses_translate() {
        assert_eq!(Config::default().speak.mode, VoiceMode::Passthrough);
        let c = Config::from_toml("[speak]\nmode = \"translate\"\n", "m").unwrap();
        assert_eq!(c.speak.mode, VoiceMode::Translate);
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("mode = \"translate\""), "{toml}");
        assert_eq!(Config::from_toml(&toml, "again").unwrap(), c);
        let err = Config::from_toml("[speak]\nmode = \"both\"\n", "bad")
            .expect_err("an unknown mode is a config error")
            .to_string();
        assert!(
            err.contains("passthrough") && err.contains("translate"),
            "{err}"
        );
    }

    /// #125: the echo guard is off unless the file turns it on.
    #[test]
    fn echo_guard_defaults_to_off_and_parses_on() {
        assert!(!Config::default().audio.echo_guard);
        assert!(
            !Config::from_toml("[audio]\nmic = \"default\"\n", "f")
                .unwrap()
                .audio
                .echo_guard
        );
        let on = Config::from_toml("[audio]\necho_guard = true\n", "on").unwrap();
        assert!(on.audio.echo_guard);
    }

    /// #122: `[speak].engine` is `chatterbox` unless the file says `qwen3`; anything else is a
    /// config error naming both.
    #[test]
    fn speak_engine_defaults_to_chatterbox_and_parses_qwen3() {
        assert_eq!(Config::default().speak.engine, SpeakEngine::Chatterbox);
        let c = Config::from_toml("[speak]\nengine = \"qwen3\"\n", "e").unwrap();
        assert_eq!(c.speak.engine, SpeakEngine::Qwen3);
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("engine = \"qwen3\""), "{toml}");
        assert_eq!(Config::from_toml(&toml, "again").unwrap(), c);
        let err = Config::from_toml("[speak]\nengine = \"elevenlabs\"\n", "bad")
            .expect_err("an unknown engine is a config error")
            .to_string();
        assert!(
            err.contains("elevenlabs") && err.contains("chatterbox") && err.contains("qwen3"),
            "{err}"
        );
    }

    #[test]
    fn assistance_defaults_to_on_and_parses_off() {
        assert!(Config::default().assist.enabled);
        let c = Config::from_toml("[assist]\nenabled = false\n", "off").unwrap();
        assert!(!c.assist.enabled);
        assert!(c.to_toml().unwrap().contains("enabled = false"));
    }

    #[test]
    fn question_source_defaults_to_them_and_round_trips_its_three_values() {
        assert_eq!(
            Config::default().detect.question_source,
            QuestionSource::Them
        );
        for (name, want) in [
            ("them", QuestionSource::Them),
            ("you", QuestionSource::You),
            ("both", QuestionSource::Both),
        ] {
            let cfg = Config::from_toml(&format!("[detect]\nquestion_source = \"{name}\"\n"), name)
                .unwrap();
            assert_eq!(cfg.detect.question_source, want);
            let toml = cfg.to_toml().unwrap();
            assert!(
                toml.contains(&format!("question_source = \"{name}\"")),
                "{toml}"
            );
            assert_eq!(Config::from_toml(&toml, name).unwrap(), cfg);
            assert_eq!(want.as_str(), name);
        }
        // #128: the name before `them` still reads, and is written back as `them`.
        let old =
            Config::from_toml("[detect]\nquestion_source = \"interviewer\"\n", "old").unwrap();
        assert_eq!(old.detect.question_source, QuestionSource::Them);
        assert!(
            old.to_toml()
                .unwrap()
                .contains("question_source = \"them\"")
        );
        // An unknown value names the three choices rather than falling back to a default.
        let err = Config::from_toml("[detect]\nquestion_source = \"candidate\"\n", "bad")
            .expect_err("an unknown question source is a config error")
            .to_string();
        for name in ["them", "you", "both"] {
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn stt_language_parses_en_auto_and_codes_and_rejects_the_rest() {
        assert_eq!("en".parse::<SttLanguage>().unwrap(), SttLanguage::English);
        assert_eq!("auto".parse::<SttLanguage>().unwrap(), SttLanguage::Auto);
        assert_eq!(
            " es ".parse::<SttLanguage>().unwrap(),
            SttLanguage::Code("es".into())
        );
        for bad in ["", "spanish", "ES", "e1", "auto-detect"] {
            assert!(bad.parse::<SttLanguage>().is_err(), "{bad:?} parsed");
        }
        let cfg = Config::from_toml("[stt]\nlanguage = \"auto\"\n", "test").unwrap();
        assert_eq!(cfg.stt.language, SttLanguage::Auto);
        assert!(cfg.to_toml().unwrap().contains("language = \"auto\""));
        assert!(
            Config::from_toml("[stt]\nlanguage = \"Spanish\"\n", "test").is_err(),
            "a bad language is a config error"
        );
    }

    #[test]
    fn every_spec_key_is_known() {
        // deny_unknown_fields plus a full parse proves each spec key maps to a struct field.
        let value: toml::Value = toml::from_str(SPEC_EXAMPLE).unwrap();
        let keys: Vec<String> = value
            .as_table()
            .unwrap()
            .iter()
            .flat_map(|(section, v)| {
                v.as_table()
                    .unwrap()
                    .keys()
                    .map(move |k| format!("{section}.{k}"))
            })
            .collect();
        assert_eq!(keys.len(), 42, "spec example has 42 keys: {keys:?}");
        Config::from_toml(SPEC_EXAMPLE, "spec").unwrap();
    }

    #[test]
    fn template_parses_to_defaults() {
        assert_eq!(
            Config::from_toml(TEMPLATE, "template").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn empty_and_partial_documents_take_defaults() {
        assert_eq!(Config::from_toml("", "empty").unwrap(), Config::default());
        let c = Config::from_toml("[llm]\nprovider = \"anthropic\"\n", "partial").unwrap();
        assert_eq!(c.llm.provider, Provider::Anthropic);
        assert!(c.llm.provider.is_remote());
        assert_eq!(c.llm.model, Config::default().llm.model);
    }

    #[test]
    fn coding_solutions_default_to_hints_and_parse_full() {
        assert_eq!(
            Config::default().ui.coding_solutions,
            CodingSolutions::Hints
        );
        let c = Config::from_toml("[ui]\ncoding_solutions = \"full\"\n", "full").unwrap();
        assert_eq!(c.ui.coding_solutions, CodingSolutions::Full);
        assert!(c.to_toml().unwrap().contains("coding_solutions = \"full\""));
        let err = Config::from_toml("[ui]\ncoding_solutions = \"all\"\n", "bad").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn the_mouse_defaults_to_on_and_parses_off() {
        assert!(Config::default().ui.mouse);
        let c = Config::from_toml("[ui]\nmouse = false\n", "off").unwrap();
        assert!(!c.ui.mouse);
        assert!(c.to_toml().unwrap().contains("mouse = false"));
        assert!(
            Config::default()
                .to_toml()
                .unwrap()
                .contains("mouse = true")
        );
    }

    #[test]
    fn speak_defaults_off_and_round_trips() {
        let s = &Config::default().speak;
        assert!(!s.enabled);
        assert_eq!(s.tts_url, "http://localhost:4123");
        assert!(s.voice_ref.is_empty());
        assert_eq!(s.device, "cpu");

        assert!(s.target.is_empty(), "no default language");

        let c = Config::from_toml(
            "[speak]\nenabled = true\ntarget = \"es\"\nvoice_ref = \"/home/u/.local/share/dmn/voice.wav\"\n",
            "speak",
        )
        .unwrap();
        assert!(c.speak.enabled);
        assert_eq!(c.speak.target, "es");
        assert_eq!(c.speak.voice_ref, "/home/u/.local/share/dmn/voice.wav");
        assert_eq!(c.speak.tts_url, "http://localhost:4123", "default fills in");
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("[speak]"), "{toml}");
        assert!(toml.contains("target = \"es\""), "{toml}");
        assert_eq!(Config::from_toml(&toml, "again").unwrap(), c);
    }

    /// #113: `[speak].model` is optional and stands in for `[llm].model` in translation only;
    /// `dmn config show` prints the one in effect; the TTS knobs round-trip and are range-checked.
    #[test]
    fn speak_model_and_tuning_parse_round_trip_and_show() {
        let d = Config::default();
        assert_eq!(d.speak.model, None);
        assert_eq!(
            d.translator_model(),
            "qwen3.6:35b-a3b",
            "unset: [llm].model"
        );
        assert!(!d.translator_is_separate());
        assert_eq!(d.speak.temperature, 0.6);
        assert_eq!((d.speak.cfg_weight, d.speak.exaggeration), (None, None));
        let plain = d.to_toml().unwrap();
        assert!(
            !plain.contains("cfg_weight") && !plain.contains("exaggeration"),
            "{plain}"
        );
        let shown = d.effective().to_toml().unwrap();
        let speak = &shown[shown.find("[speak]").expect(&shown)..];
        assert!(speak.contains("model = \"qwen3.6:35b-a3b\""), "{shown}");
        assert!(speak.contains("temperature = 0.6"), "{shown}");

        let c = Config::from_toml(
            "[speak]\nmodel = \"llama3.1:8b\"\ntemperature = 0.5\ncfg_weight = 0.3\nexaggeration = 0.7\n",
            "speak",
        )
        .unwrap();
        assert_eq!(c.speak.model.as_deref(), Some("llama3.1:8b"));
        assert_eq!(c.translator_model(), "llama3.1:8b");
        assert!(c.translator_is_separate());
        let llm = c.translator_llm();
        assert_eq!(llm.model, "llama3.1:8b");
        assert_eq!(
            Llm {
                model: c.llm.model.clone(),
                ..llm
            },
            c.llm,
            "everything else is [llm]'s"
        );
        assert_eq!(
            (
                c.speak.temperature,
                c.speak.cfg_weight,
                c.speak.exaggeration
            ),
            (0.5, Some(0.3), Some(0.7))
        );
        let toml = c.to_toml().unwrap();
        for line in [
            "model = \"llama3.1:8b\"",
            "temperature = 0.5",
            "cfg_weight = 0.3",
            "exaggeration = 0.7",
        ] {
            assert!(toml.contains(line), "{line}: {toml}");
        }
        assert_eq!(Config::from_toml(&toml, "again").unwrap(), c);
        assert!(
            c.effective()
                .to_toml()
                .unwrap()
                .contains("model = \"llama3.1:8b\""),
            "config show prints it"
        );

        // Blank, or the answer model itself, is no separate translator.
        let blank = Config::from_toml("[speak]\nmodel = \" \"\n", "b").unwrap();
        assert_eq!(blank.translator_model(), "qwen3.6:35b-a3b");
        let same = Config::from_toml("[speak]\nmodel = \"qwen3.6:35b-a3b\"\n", "s").unwrap();
        assert!(!same.translator_is_separate());

        for (bad, key) in [
            (
                "temperature = 0.0",
                "[speak].temperature = 0 is outside 0.05-5",
            ),
            (
                "cfg_weight = 1.5",
                "[speak].cfg_weight = 1.5 is outside 0-1",
            ),
            (
                "exaggeration = 0.1",
                "[speak].exaggeration = 0.1 is outside 0.25-2",
            ),
        ] {
            let err = Config::from_toml(&format!("[speak]\n{bad}\n"), "r").unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn speak_unknown_keys_are_errors() {
        let err = Config::from_toml("[speak]\ntts_ur = \"x\"\n", "typo").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    /// #109: `t` speaks in `target`, which has no default; on without it is an error that names
    /// the key and the fix, whatever command loads the file.
    #[test]
    fn speak_enabled_without_a_target_is_a_config_error_naming_target() {
        let err = Config::from_toml(
            "[speak]\nenabled = true\n",
            "/home/u/.config/dmn/config.toml",
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        let msg = err.to_string();
        assert!(
            msg.starts_with("/home/u/.config/dmn/config.toml: "),
            "{msg}"
        );
        assert!(msg.contains("needs `target` under [speak]"), "{msg}");
        assert!(msg.contains("--language es"), "{msg}");
        // Blank is unset too.
        assert!(Config::from_toml("[speak]\nenabled = true\ntarget = \" \"\n", "t").is_err());
        // Off, an unset target is fine (the default), and a set one is kept for later.
        assert!(Config::from_toml("[speak]\nenabled = false\n", "t").is_ok());
        let off = Config::from_toml("[speak]\ntarget = \"de\"\n", "t").unwrap();
        assert_eq!(off.speak.target, "de");
        assert!(Config::default().check().is_ok());
    }

    /// The shape `[stt].language` checks for a code: two or three lowercase letters.
    #[test]
    fn speak_target_takes_the_shape_of_a_language_code() {
        for ok in ["es", "de", "yue", " fr "] {
            let cfg = Config::from_toml(&format!("[speak]\ntarget = {ok:?}\n"), "t").unwrap();
            assert_eq!(cfg.speak.target, ok.trim());
        }
        for bad in ["Spanish", "ES", "e", "auto", "es-MX", "e5"] {
            let err =
                Config::from_toml(&format!("[speak]\ntarget = {bad:?}\n"), "t").expect_err(bad);
            assert!(matches!(err, ConfigError::Parse { .. }), "{bad}: {err}");
            assert!(
                err.to_string().contains("[speak].target"),
                "{bad}: names the key: {err}"
            );
        }
    }

    #[test]
    fn write_voice_ref_updates_the_key_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::write_template(&path, false).unwrap();
        Config::write_voice_ref(&path, "/data/voice.wav").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("voice_ref = \"/data/voice.wav\"          # enrolled reference clip"),
            "value replaced, comment kept:\n{text}"
        );
        assert!(text.contains("# Daemon (dmn) configuration"));
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.speak.voice_ref, "/data/voice.wav");
        assert_eq!(cfg.llm, Config::default().llm, "other keys untouched");

        // A second enroll replaces the value in place.
        Config::write_voice_ref(&path, "/data/other.wav").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().speak.voice_ref,
            "/data/other.wav"
        );
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("voice_ref")
                .count(),
            1
        );
    }

    #[test]
    fn write_voice_ref_creates_a_missing_config_from_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/config.toml");
        Config::write_voice_ref(&path, "/data/voice.wav").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.speak.voice_ref, "/data/voice.wav");
        assert_eq!(cfg.audio, Config::default().audio);
    }

    #[test]
    fn set_voice_ref_appends_a_section_when_there_is_none_and_escapes_values() {
        let set_voice_ref = |text: &str, value: &str| {
            set_speak_key(
                text,
                "voice_ref",
                &toml::Value::String(value.to_owned()).to_string(),
            )
        };
        let edited = set_voice_ref("[llm]\nmodel = \"m\"\n", "/tmp/we\"ird.wav");
        let cfg = Config::from_toml(&edited, "edited").unwrap();
        assert_eq!(cfg.speak.voice_ref, "/tmp/we\"ird.wav");
        assert_eq!(cfg.llm.model, "m");

        // A [speak] section without the key gains it at the end of the section body.
        let edited = set_voice_ref(
            "[speak]\nenabled = true\ntarget = \"es\"\n\n[ui]\n",
            "/v.wav",
        );
        assert!(
            edited.contains("target = \"es\"\nvoice_ref = \"/v.wav\"\n\n[ui]"),
            "{edited}"
        );
        let cfg = Config::from_toml(&edited, "edited").unwrap();
        assert!(cfg.speak.enabled);
        assert_eq!(cfg.speak.voice_ref, "/v.wav");
    }

    /// A user's own config, comments, a live `[lookup]` and odd spacing included: the edit
    /// changes the `voice_ref` line and nothing else, byte for byte.
    #[test]
    fn write_voice_ref_on_a_users_file_changes_only_that_line() {
        let user = "\
# my dmn config; keep these notes
[llm]
model = \"qwen3.6:35b-a3b\"   # measured
num_ctx = 8192

[lookup]
enabled = true                # the SearXNG container
provider = \"searxng\"
base_url = 'http://localhost:8888/#not-a-comment'

[speak]   # added by hand
enabled = true
target = \"es\"
voice_ref = \"/old/voice.wav\"   # set by dmn speak enroll
tts_url = \"http://127.0.0.1:4123\"

[ui]
mouse = false
";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, user).unwrap();
        Config::write_voice_ref(&path, "/home/u/.local/share/dmn/voice.wav").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let changed: Vec<(&str, &str)> = user
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(
            changed,
            [(
                "voice_ref = \"/old/voice.wav\"   # set by dmn speak enroll",
                "voice_ref = \"/home/u/.local/share/dmn/voice.wav\" # set by dmn speak enroll"
            )],
            "{after}"
        );
        assert_eq!(user.lines().count(), after.lines().count());
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.speak.voice_ref, "/home/u/.local/share/dmn/voice.wav");
        assert!(cfg.lookup.enabled);
        assert_eq!(cfg.lookup.base_url, "http://localhost:8888/#not-a-comment");
        assert_eq!(cfg.llm.num_ctx, 8192);
        assert!(!cfg.ui.mouse);
        assert!(
            !dir.path().join("config.toml.dmn-tmp").exists(),
            "no temp file left"
        );
    }

    #[test]
    fn write_voice_ref_adds_the_section_to_a_file_without_one() {
        let user = "# notes\n[lookup]\nenabled = true # on\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, user).unwrap();
        Config::write_voice_ref(&path, "/v.wav").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(user), "{after}");
        assert!(
            after.ends_with("\n[speak]\nvoice_ref = \"/v.wav\"\n"),
            "{after}"
        );
        assert!(Config::load(&path).unwrap().lookup.enabled);
    }

    // --- #115: [speak].languages, the picker's list ------------------------------------------

    fn lang(code: &str, voice: &str) -> SpeakLanguage {
        SpeakLanguage {
            code: code.into(),
            voice: voice.into(),
        }
    }

    /// #115: the list parses from inline tables and from `[[speak.languages]]` tables, round
    /// trips, and is left out of the TOML while empty; a bad code, a missing voice or a code
    /// listed twice is a config error naming it.
    #[test]
    fn speak_languages_parse_and_round_trip() {
        assert!(Config::default().speak.languages.is_empty());
        assert!(!Config::default().to_toml().unwrap().contains("languages"));
        let c = Config::from_toml(
            "[speak]\ntarget = \"es\"\nlanguages = [{ code = \"de\", voice = \"voice-de\" }, { code = \" es \", voice = \"me\" }]\n",
            "inline",
        )
        .unwrap();
        assert_eq!(
            c.speak.languages,
            [lang("de", "voice-de"), lang("es", "me")],
            "codes are trimmed"
        );
        assert_eq!(
            c.speak.voices(),
            c.speak.languages,
            "the list is the voices"
        );
        assert_eq!(c.speak.voice_for("es").as_deref(), Some("me"));
        assert_eq!(c.speak.voice_for("ru"), None);
        let toml = c.to_toml().unwrap();
        assert!(toml.contains("[[speak.languages]]"), "{toml}");
        assert_eq!(Config::from_toml(&toml, "again").unwrap(), c);
        let tables = Config::from_toml(
            "[speak]\ntarget = \"de\"\n[[speak.languages]]\ncode = \"de\"\nvoice = \"voice-de\"\n",
            "tables",
        )
        .unwrap();
        assert_eq!(tables.speak.languages, [lang("de", "voice-de")]);

        for (bad, names) in [
            (
                "languages = [{ code = \"German\", voice = \"v\" }]",
                "code \"German\" is not",
            ),
            ("languages = [{ code = \"de\" }]", "missing field `voice`"),
            (
                "languages = [{ code = \"de\", voice = \" \" }]",
                "the entry for \"de\" needs `voice`",
            ),
            (
                "languages = [{ code = \"de\", voice = \"a\" }, { code = \"de\", voice = \"b\" }]",
                "lists \"de\" twice",
            ),
            (
                "languages = [{ code = \"de\", voice = \"v\", clip = \"x\" }]",
                "unknown field `clip`",
            ),
        ] {
            let err = Config::from_toml(&format!("[speak]\n{bad}\n"), "bad")
                .expect_err(bad)
                .to_string();
            assert!(err.contains(names), "{bad}: {err}");
        }
    }

    /// #115: with the list present `target` is one of its codes; the error names both. Left
    /// out, `target` + `voice_ref` are the one language, as the owner's config has them.
    #[test]
    fn speak_target_must_be_one_of_the_listed_languages() {
        let err = Config::from_toml(
            "[speak]\ntarget = \"ru\"\nlanguages = [{ code = \"de\", voice = \"voice-de\" }, { code = \"es\", voice = \"voice-es\" }]\n",
            "ru",
        )
        .expect_err("ru is not listed")
        .to_string();
        assert!(
            err.contains("[speak].target = \"ru\" is not one of [speak].languages (de, es)"),
            "{err}"
        );
        assert!(
            err.contains("dmn speak enroll --wav <clip> --language ru"),
            "{err}"
        );
        // An unset target with a list is fine while speak is off (the enabled check names it).
        let off = Config::from_toml(
            "[speak]\nlanguages = [{ code = \"de\", voice = \"voice-de\" }]\n",
            "off",
        )
        .unwrap();
        assert_eq!(off.speak.voices(), [lang("de", "voice-de")]);
        // The owner's shape: no list, the voice from `voice_ref`'s file stem.
        let owner = Config::from_toml(
            "[speak]\nenabled = true\ntarget = \"ru\"\nvoice_ref = \"/home/u/.local/share/dmn/voice-ru.wav\"\nmodel = \"llama3.1:8b\"\ntemperature = 0.6\n",
            "owner",
        )
        .unwrap();
        assert!(owner.speak.languages.is_empty());
        assert_eq!(owner.speak.voices(), [lang("ru", "voice-ru")]);
        assert_eq!(owner.speak.voice_for("ru").as_deref(), Some("voice-ru"));
        assert_eq!(voice_ref_name(""), "voice");
        assert_eq!(voice_ref_name("/x/narrator.wav"), "narrator");
        let unset = Config::from_toml("[speak]\nvoice_ref = \"/v.wav\"\n", "unset").unwrap();
        assert!(unset.speak.voices().is_empty(), "no target, no language");
    }

    /// #115: `dmn speak enroll --language` adds or updates one entry in place, the user's
    /// comments and other sections untouched; a file with no list gets one holding the
    /// current `target` + `voice_ref` too, so `target` stays valid.
    #[test]
    fn write_speak_language_adds_or_updates_one_entry_and_changes_nothing_else() {
        let user = "\
# my dmn config; keep these notes
[llm]
model = \"qwen3.6:35b-a3b\"   # measured
num_ctx = 8192

[speak]   # added by hand
enabled = true
target = \"ru\"
voice_ref = \"/home/u/.local/share/dmn/voice-ru.wav\"   # set by dmn speak enroll
model = \"llama3.1:8b\"

[ui]
mouse = false
";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, user).unwrap();
        Config::write_speak_language(&path, "de", "voice-de").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let added: Vec<&str> = after.lines().filter(|l| !user.contains(l)).collect();
        assert_eq!(
            added,
            [
                "languages = [{ code = \"ru\", voice = \"voice-ru\" }, { code = \"de\", voice = \"voice-de\" }]"
            ],
            "{after}"
        );
        assert_eq!(after.lines().count(), user.lines().count() + 1);
        assert!(
            after.contains("model = \"llama3.1:8b\"\nlanguages = ["),
            "at the end of the section body:\n{after}"
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.speak.target, "ru", "the target is kept");
        assert_eq!(
            cfg.speak.languages,
            [lang("ru", "voice-ru"), lang("de", "voice-de")]
        );
        assert_eq!(cfg.llm.num_ctx, 8192);
        assert!(!cfg.ui.mouse);

        // A second language appends; the same language again updates its voice in place.
        Config::write_speak_language(&path, "es", "voice-es").unwrap();
        Config::write_speak_language(&path, "de", "narrator").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("languages").count(), 1, "{after}");
        assert_eq!(
            Config::load(&path).unwrap().speak.languages,
            [
                lang("ru", "voice-ru"),
                lang("de", "narrator"),
                lang("es", "voice-es")
            ]
        );
        assert_eq!(after.lines().count(), user.lines().count() + 1);

        // From the template: the commented `# languages = …` line is the one that is set, its
        // comment kept, and an unset target takes the enrolled language.
        let path = dir.path().join("fresh.toml");
        Config::write_template(&path, false).unwrap();
        Config::write_speak_language(&path, "es", "voice-es").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("\nlanguages = [{ code = \"es\", voice = \"voice-es\" }] # dmn run: the languages `v` picks"),
            "{after}"
        );
        assert!(!after.contains("# languages ="), "{after}");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.speak.target, "es");
        assert_eq!(cfg.speak.languages, [lang("es", "voice-es")]);
        assert_eq!(cfg.llm, Config::default().llm);
    }

    /// #115: the picker's choice is written to `target` in place; a file that cannot be
    /// written is an error, not a panic (the pipeline shows it as a 5 s notice).
    #[test]
    fn write_speak_target_sets_the_one_line_and_a_read_only_file_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let user = "[speak]\nenabled = true\ntarget = \"de\"   # #109\nlanguages = [{ code = \"de\", voice = \"voice-de\" }, { code = \"es\", voice = \"voice-es\" }]\n\n[ui]\nmouse = false\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, user).unwrap();
        Config::write_speak_target(&path, "es").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        let changed: Vec<(&str, &str)> = user
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed.len(), 1, "{after}");
        assert_eq!(changed[0].0, "target = \"de\"   # #109");
        let (code, comment) = split_comment(changed[0].1);
        assert_eq!((code.trim_end(), comment), ("target = \"es\"", "# #109"));
        assert_eq!(after.lines().count(), user.lines().count());
        assert_eq!(Config::load(&path).unwrap().speak.target, "es");
        // A code the list does not have is refused before anything is written.
        let err = Config::write_speak_target(&path, "ru").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);

        // Read-only directory: the temp file cannot be made.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = Config::write_speak_target(&path, "de");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if nix_is_root() {
            return;
        }
        let err = result.expect_err("a read-only config dir");
        assert!(matches!(err, ConfigError::Write { .. }), "{err}");
        assert!(err.to_string().contains("cannot write"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after,
            "nothing changed"
        );
    }

    /// Root ignores file permissions, so the read-only check cannot run as root (CI does not).
    fn nix_is_root() -> bool {
        std::fs::metadata("/proc/self")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() == 0
            })
            .unwrap_or(false)
    }

    /// A dotfiles-managed config is a symlink: the file it points to is edited, the link stays,
    /// and the file keeps its permissions.
    #[test]
    fn write_voice_ref_edits_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-config.toml");
        std::fs::write(&real, "[speak]\nenabled = true\ntarget = \"es\"\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        Config::write_voice_ref(&link, "/v.wav").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(Config::load(&real).unwrap().speak.voice_ref, "/v.wav");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// An edit this line-based approach cannot make (a dotted key) leaves the file untouched,
    /// and so does a file that does not parse.
    #[test]
    fn write_voice_ref_leaves_a_file_it_cannot_edit_untouched() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("dotted", "speak.voice_ref = \"/old.wav\"\n"),
            ("broken", "[llm\nmodel = \n"),
        ] {
            let path = dir.path().join(format!("{name}.toml"));
            std::fs::write(&path, text).unwrap();
            let err = Config::write_voice_ref(&path, "/v.wav").unwrap_err();
            assert!(
                matches!(err, ConfigError::Edit(_) | ConfigError::Parse { .. }),
                "{name}: {err}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{name}");
        }
    }

    #[test]
    fn split_comment_knows_both_string_kinds() {
        assert_eq!(split_comment("a = \"x#y\" # c"), ("a = \"x#y\" ", "# c"));
        assert_eq!(split_comment("a = 'x#y' # c"), ("a = 'x#y' ", "# c"));
        assert_eq!(
            split_comment(r##"a = "q\"#" # c"##),
            (r##"a = "q\"#" "##, "# c")
        );
        assert_eq!(split_comment("a = 'it''s'"), ("a = 'it''s'", ""));
        assert_eq!(split_comment("[speak] # hi"), ("[speak] ", "# hi"));
    }

    #[test]
    fn speak_tts_url_must_be_on_this_machine() {
        assert!(Speak::default().is_local());
        for url in [
            "http://127.0.0.1:4123",
            "http://[::1]:4123/",
            " http://LOCALHOST:4123 ",
        ] {
            let s = Speak {
                tts_url: url.into(),
                ..Speak::default()
            };
            assert!(s.is_local(), "{url}");
        }
        for url in ["http://gpu-box.lan:4123", "https://tts.example.com"] {
            let s = Speak {
                tts_url: url.into(),
                ..Speak::default()
            };
            assert!(!s.is_local(), "{url}");
        }
    }

    #[test]
    fn config_lookup_is_off_by_default_and_show_prints_its_keys() {
        let lookup = Config::default().lookup;
        assert!(!lookup.enabled);
        assert!(lookup.privacy_notice().is_none(), "off says nothing leaves");
        assert_eq!(
            (
                lookup.provider.as_str(),
                lookup.base_url.as_str(),
                lookup.api_key_env.as_str(),
                lookup.timeout_ms(),
                lookup.max_results
            ),
            ("ollama", "https://ollama.com", "OLLAMA_API_KEY", 3000, 5)
        );
        // `dmn config show` prints the effective config: every `[lookup]` key, the timeout as
        // it takes effect.
        let shown = Config::default().effective().to_toml().unwrap();
        let section = &shown[shown.find("[lookup]").expect(&shown)..];
        for key in [
            "enabled = false",
            "provider = \"ollama\"",
            "base_url = \"https://ollama.com\"",
            "api_key_env = \"OLLAMA_API_KEY\"",
            "timeout_ms = 3000",
            "max_results = 5",
        ] {
            assert!(section.contains(key), "{key}: {shown}");
        }

        let on = Config::from_toml(
            "[lookup]\nenabled = true\nbase_url = \"http://127.0.0.1:9/\"\n",
            "on",
        )
        .unwrap();
        assert!(on.lookup.enabled);
        let notice = on.lookup.privacy_notice().unwrap();
        assert!(notice.contains("http://127.0.0.1:9 "), "{notice}");
        assert!(notice.contains("no transcript"), "{notice}");
        assert!(
            Config::from_toml("[lookup]\nprovider = \"bing\"\n", "bad").is_err(),
            "ollama or searxng; anything else is a config error"
        );
    }

    #[test]
    fn config_lookup_searxng_defaults_its_base_url_to_this_machine() {
        let only = Config::from_toml("[lookup]\nenabled = true\nprovider = \"searxng\"\n", "s")
            .unwrap()
            .lookup;
        assert_eq!(only.provider, LookupProvider::Searxng);
        assert!(!only.provider.needs_key());
        assert_eq!(
            only.endpoint(),
            "http://localhost:8888",
            "unset: this machine"
        );
        let shown = Config {
            lookup: only.clone(),
            ..Config::default()
        }
        .to_toml()
        .unwrap();
        assert!(shown.contains("provider = \"searxng\""), "{shown}");
        for (url, want) in [
            ("https://ollama.com/", "http://localhost:8888"),
            ("", "http://localhost:8888"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
            ("http://searx.lan", "http://searx.lan"),
        ] {
            let l = Lookup {
                base_url: url.into(),
                ..only.clone()
            };
            assert_eq!(l.endpoint(), want, "{url:?}");
        }
        let notice = only.privacy_notice().unwrap();
        assert!(
            notice.contains("your SearXNG at http://localhost:8888"),
            "{notice}"
        );
        assert!(
            notice.contains("forwards it to the search engines"),
            "{notice}"
        );
        // Ollama keeps its own URL, and stays the default provider.
        assert_eq!(Lookup::default().provider, LookupProvider::Ollama);
        assert!(LookupProvider::Ollama.needs_key());
        assert_eq!(Lookup::default().endpoint(), "https://ollama.com");
        // A stock SearXNG waits up to 3 s on slow engines itself: its default deadline is longer.
        assert_eq!(only.timeout(), std::time::Duration::from_millis(6000));
        let effective = Config {
            lookup: only.clone(),
            ..Config::default()
        }
        .effective()
        .to_toml()
        .unwrap();
        assert!(effective.contains("timeout_ms = 6000"), "{effective}");
        // Written in the file, even 3000, it is used as written.
        let written =
            Config::from_toml("[lookup]\nprovider = \"searxng\"\ntimeout_ms = 3000\n", "w")
                .unwrap()
                .lookup;
        assert_eq!(written.timeout(), std::time::Duration::from_millis(3000));
        let set = Lookup {
            timeout_ms: Some(4500),
            ..only.clone()
        };
        assert_eq!(
            set.timeout(),
            std::time::Duration::from_millis(4500),
            "a value set wins"
        );
        assert_eq!(
            Lookup::default().timeout(),
            std::time::Duration::from_millis(3000)
        );
    }

    #[test]
    fn config_lookup_key_is_read_from_the_named_variable_and_never_printed() {
        let env = |name: &str| (name == "OLLAMA_API_KEY").then(|| "ol-secret-value".to_owned());
        let mut c = Config::default();
        assert_eq!(
            c.lookup.api_key_from(env).unwrap().expose(),
            "ol-secret-value"
        );
        assert!(c.lookup.api_key_from(|_| Some(String::new())).is_none());
        assert!(c.lookup.api_key_from(|_| None).is_none());
        c.lookup.api_key_env = String::new();
        assert!(c.lookup.api_key_from(env).is_none());
        c.lookup.enabled = true;
        assert!(!c.to_toml().unwrap().contains("secret"));
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = Config::from_toml("[audio]\nmicrophone = \"x\"\n", "typo").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("microphone"));
    }

    #[test]
    fn missing_file_is_default_and_init_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        Config::write_template(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Daemon (dmn) configuration"));
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        let err = Config::write_template(&path, false).unwrap_err();
        assert!(matches!(err, ConfigError::Exists(_)), "{err}");
        Config::write_template(&path, true).unwrap();
    }

    /// Copilot on PR #17: `exists()` then `write` is two steps. A file that appears in between
    /// was overwritten, and a dangling symlink reported "missing" and was then followed.
    #[test]
    fn init_never_follows_a_symlink_or_clobbers_a_file_that_appeared() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("precious.toml");
        std::fs::write(&victim, "keep me").unwrap();

        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let err = Config::write_template(&link, false).unwrap_err();
        assert!(matches!(err, ConfigError::Exists(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");

        // A dangling symlink is still a name that exists: creating through it is refused.
        let dangling = dir.path().join("dangling.toml");
        std::os::unix::fs::symlink(dir.path().join("nowhere.toml"), &dangling).unwrap();
        let err = Config::write_template(&dangling, false).unwrap_err();
        assert!(matches!(err, ConfigError::Exists(_)), "{err}");
        assert!(
            !dir.path().join("nowhere.toml").exists(),
            "nothing written through the link"
        );

        // With --force the user has asked for it, and the template lands.
        Config::write_template(&link, true).unwrap();
        assert_eq!(Config::load(&link).unwrap(), Config::default());
    }

    #[test]
    fn api_key_is_read_from_named_env_var_and_never_printed() {
        let env = |name: &str| (name == "DMN_TEST_KEY_ENV").then(|| "sk-secret-value".to_owned());
        let mut c = Config::default();
        assert!(
            c.api_key_from(env).is_none(),
            "empty api_key_env means no key"
        );
        c.llm.api_key_env = "DMN_TEST_KEY_ENV".into();
        let key = c.api_key_from(env).unwrap();
        assert_eq!(key.expose(), "sk-secret-value");
        assert!(!format!("{key:?}").contains("secret"));
        assert!(!key.to_string().contains("secret"));
        assert!(!c.to_toml().unwrap().contains("secret"));
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn remote_providers_and_other_hosts_say_text_leaves_the_machine() {
        let ollama = Llm::default();
        assert!(!ollama.leaves_machine());
        assert!(ollama.privacy_notice().is_none());
        for url in [
            "http://127.0.0.1:11434",
            "http://[::1]:11434/",
            "http://LOCALHOST:1234",
        ] {
            let l = Llm {
                base_url: url.into(),
                ..Llm::default()
            };
            assert!(!l.leaves_machine(), "{url}");
        }
        let lan = Llm {
            base_url: "http://gpu-box.lan:11434".into(),
            ..Llm::default()
        };
        assert!(lan.leaves_machine());

        let anthropic = Llm {
            provider: Provider::Anthropic,
            ..Llm::default()
        };
        assert_eq!(anthropic.endpoint(), "https://api.anthropic.com");
        let notice = anthropic.privacy_notice().expect("remote");
        assert!(notice.contains("leave this machine"), "{notice}");
        assert!(notice.contains("anthropic"), "{notice}");

        // A local OpenAI-compatible server (LM Studio) is still a remote API by provider, as
        // the SPEC words it; the notice says where the text goes.
        let lm_studio = Llm {
            provider: Provider::OpenAI,
            base_url: "http://localhost:1234".into(),
            ..Llm::default()
        };
        assert_eq!(lm_studio.endpoint(), "http://localhost:1234");
        assert!(lm_studio.leaves_machine());
    }
}
