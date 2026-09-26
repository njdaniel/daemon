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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub audio: Audio,
    pub stt: Stt,
    pub detect: Detect,
    pub llm: Llm,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SttDevice {
    #[default]
    Cpu,
    Gpu,
    Auto,
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
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            always_on_top: true,
            force_x11: false,
            opacity: 1.0,
            never_auto_expand: true,
            coding_solutions: CodingSolutions::Hints,
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

[detect]
sensitivity = "balanced"             # eager | balanced | strict

[llm]
provider = "ollama"                  # ollama | openai | anthropic. Only ollama keeps transcripts local.
model = "qwen3.6:35b-a3b"
base_url = "http://localhost:11434"
api_key_env = ""                     # NAME of an env var holding the key; never the key itself
answer_length = "concise"            # concise (5 lines) | minimal (3 lines)
fast_timeout_ms = 4000
slow_timeout_ms = 20000              # the six expandable sections; 0 turns the slow phase off
num_ctx = 0                          # Ollama context window to ask for; 0 = the server default

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above on GNOME Wayland
opacity = 1.0                        # 0.7 – 1.0
never_auto_expand = true
coding_solutions = "hints"           # hints | full: worked solutions in Deeper Answer (s toggles)

[privacy]
record_audio = false                 # write WAVs of the session (shows a red REC dot)
keep_transcripts = true
"#;

impl Config {
    /// Parse a TOML document. Missing keys take defaults; unknown keys are errors.
    pub fn from_toml(text: &str, path_for_errors: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path_for_errors.to_owned(),
            source,
        })
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

    /// The effective config as TOML (no comments).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
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

[detect]
sensitivity = "balanced"             # eager | balanced | strict

[llm]
provider = "ollama"                  # ollama | openai | anthropic
model = "qwen3.6:35b-a3b"           # measured 2026-09-21 on the dev 4090; see CLAUDE.md
base_url = "http://localhost:11434"
api_key_env = ""                     # name of an env var; never the key itself
answer_length = "concise"            # concise (5) | minimal (3)
fast_timeout_ms = 4000
slow_timeout_ms = 20000              # 0 turns the slow phase off
num_ctx = 0                          # Ollama context window to ask for; 0 = the server default

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above
opacity = 1.0
never_auto_expand = true
coding_solutions = "hints"           # hints | full (worked solutions in Deeper Answer)

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
        assert_eq!(keys.len(), 26, "spec example has 26 keys: {keys:?}");
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
