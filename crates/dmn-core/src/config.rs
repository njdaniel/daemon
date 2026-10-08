//! Typed `config.toml`. Every field has the default from docs/SPEC.md; unknown keys are errors so
//! a typo never silently falls back to a default.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use dmn_session::CodingSolutions;

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
    pub echo_guard: bool,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            mic: "default".into(),
            system_audio: true,
            app_stream: "Firefox".into(),
            echo_guard: true,
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
/// (`interviewer`, the default) or either (`both`). Fixed for a session, like `[stt].language`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuestionSource {
    #[default]
    Interviewer,
    You,
    Both,
}

impl QuestionSource {
    /// The config-file spelling, for the status line and `dmn config show`.
    pub fn as_str(self) -> &'static str {
        match self {
            QuestionSource::Interviewer => "interviewer",
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
}

/// `[speak].temperature`'s default (#113).
pub const SPEAK_TEMPERATURE: f64 = 0.6;

/// `[speak].target`: "" (unset) or a code of the shape `[stt].language` takes, two or three
/// lowercase letters. Whether whisper and the TTS server know it is checked when `dmn run`
/// starts.
fn language_code<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let raw = String::deserialize(d)?;
    let code = raw.trim();
    if code.is_empty()
        || ((2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Ok(code.to_owned());
    }
    Err(serde::de::Error::custom(format!(
        "[speak].target {raw:?} is not a two- or three-letter language code such as \"es\""
    )))
}

impl Speak {
    /// `tts_url` is on this machine (a loopback name or address). `dmn speak` refuses to run
    /// otherwise: the text, and at enrollment the voice clip, would leave the machine, and
    /// SPEC § Privacy promises they do not.
    pub fn is_local(&self) -> bool {
        is_loopback_url(self.tts_url.trim())
    }
}

impl Default for Speak {
    fn default() -> Self {
        Self {
            enabled: false,
            tts_url: "http://localhost:4123".into(),
            voice_ref: String::new(),
            device: "cpu".into(),
            target: String::new(),
            model: None,
            temperature: SPEAK_TEMPERATURE,
            cfg_weight: None,
            exaggeration: None,
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
system_audio = true                  # capture the interviewer from an app's output stream
app_stream = "Firefox"               # application.name to capture; "" = whole default sink monitor
echo_guard = true                    # drop mic text that duplicates the loopback within 300 ms

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
question_source = "interviewer"      # interviewer | you | both: whose questions are detected and answered

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
tts_url = "http://localhost:4123"    # chatterbox-tts-api on this machine (no other host); README § Speak
voice_ref = ""                       # enrolled reference clip; `dmn speak enroll --wav <clip>` sets it
device = "cpu"                       # cpu | cuda: a hint for the server setup; check nvidia-smi first
target = ""                          # dmn run: `t` speaks your last turn in this language (es, de, ...); the voice's enroll --language; needed with enabled = true
# model = "llama3.1:8b"              # the Ollama model `t` translates with; left out: [llm].model. A small one keeps the answer model out of VRAM with assistance off
temperature = 0.6                    # the TTS model's sampling temperature (server default 0.8); lower = fewer noise bursts after a sentence
# cfg_weight = 0.5                   # the TTS model's pace, 0.0-1.0; left out: the server's
# exaggeration = 0.5                 # the TTS model's emotion intensity, 0.25-2.0; left out: the server's

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
    /// voice was enrolled with.
    pub fn check(&self) -> Result<(), String> {
        if self.speak.enabled && self.speak.target.is_empty() {
            return Err(
                "[speak].enabled = true needs `target` under [speak]: the language `t` speaks, \
                 a code such as \"es\", the one the voice was enrolled with \
                 (`dmn speak enroll --language es`)"
                    .into(),
            );
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
        let edited = set_voice_ref(&text, voice_ref);
        let parsed = Self::from_toml(&edited, &shown)?;
        if parsed.speak.voice_ref != voice_ref {
            return Err(ConfigError::Edit(format!(
                "{shown}: cannot set [speak].voice_ref in place (is it a dotted key or an inline \
                 table?); write voice_ref = {} under [speak] by hand",
                toml::Value::String(voice_ref.to_owned())
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

/// `text` with `[speak].voice_ref` set to `value`: replace the key where it stands (keeping an
/// inline comment), insert it under an existing `[speak]` header, or append a new section.
/// The value is quoted through `toml::Value` so any path characters are escaped correctly.
fn set_voice_ref(text: &str, value: &str) -> String {
    let quoted = toml::Value::String(value.to_owned()).to_string();
    let mut out: Vec<String> = Vec::new();
    let mut in_speak = false;
    let mut section_seen = false;
    let mut written = false;
    // Blank lines trailing a section are held back so the key lands before them, not after.
    let mut blanks: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            if in_speak && !written {
                out.push(format!("voice_ref = {quoted}"));
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
        let is_voice_ref = trimmed
            .strip_prefix("voice_ref")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if in_speak && is_voice_ref {
            let (_, comment) = split_comment(line);
            let mut new_line = format!("voice_ref = {quoted}");
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
        out.push(format!("voice_ref = {quoted}"));
    }
    out.append(&mut blanks);
    if !section_seen {
        out.push(String::new());
        out.push("[speak]".to_owned());
        out.push(format!("voice_ref = {quoted}"));
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
echo_guard = true

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
question_source = "interviewer"      # interviewer | you | both: whose questions trigger an answer

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

    #[test]
    fn assistance_defaults_to_on_and_parses_off() {
        assert!(Config::default().assist.enabled);
        let c = Config::from_toml("[assist]\nenabled = false\n", "off").unwrap();
        assert!(!c.assist.enabled);
        assert!(c.to_toml().unwrap().contains("enabled = false"));
    }

    #[test]
    fn question_source_defaults_to_the_interviewer_and_round_trips_its_three_values() {
        assert_eq!(
            Config::default().detect.question_source,
            QuestionSource::Interviewer
        );
        for (name, want) in [
            ("interviewer", QuestionSource::Interviewer),
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
        // An unknown value names the three choices rather than falling back to a default.
        let err = Config::from_toml("[detect]\nquestion_source = \"candidate\"\n", "bad")
            .expect_err("an unknown question source is a config error")
            .to_string();
        for name in ["interviewer", "you", "both"] {
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
        assert_eq!(keys.len(), 41, "spec example has 41 keys: {keys:?}");
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
