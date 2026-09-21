//! Typed `config.toml`. Every field has the default from docs/SPEC.md; unknown keys are errors so
//! a typo never silently falls back to a default.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
    pub vad: Vad,
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            engine: SttEngine::WhisperCpp,
            model: "small.en".into(),
            threads: 4,
            vad: Vad::Energy,
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
    pub slow_timeout_ms: u64,
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
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            always_on_top: true,
            force_x11: false,
            opacity: 1.0,
            never_auto_expand: true,
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
threads = 4
vad = "energy"                       # energy | silero (M2)

[detect]
sensitivity = "balanced"             # eager | balanced | strict

[llm]
provider = "ollama"                  # ollama | openai | anthropic. Only ollama keeps transcripts local.
model = "qwen3.6:35b-a3b"
base_url = "http://localhost:11434"
api_key_env = ""                     # NAME of an env var holding the key; never the key itself
answer_length = "concise"            # concise (5 lines) | minimal (3 lines)
fast_timeout_ms = 4000
slow_timeout_ms = 20000

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above on GNOME Wayland
opacity = 1.0                        # 0.7 – 1.0
never_auto_expand = true

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
    pub fn write_template(path: &Path, force: bool) -> Result<(), ConfigError> {
        let shown = path.display().to_string();
        if path.exists() && !force {
            return Err(ConfigError::Exists(shown));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
                path: shown.clone(),
                source,
            })?;
        }
        std::fs::write(path, TEMPLATE).map_err(|source| ConfigError::Write {
            path: shown,
            source,
        })
    }

    /// The effective config as TOML (no comments).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Read the API key from the env var named in `[llm].api_key_env`. `None` when unset or empty.
    pub fn api_key(&self) -> Option<ApiKey> {
        self.api_key_from(|name| std::env::var(name).ok())
    }

    /// Same as [`Config::api_key`] with the environment lookup injected (tests use a closure).
    pub fn api_key_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Option<ApiKey> {
        if self.llm.api_key_env.is_empty() {
            return None;
        }
        lookup(&self.llm.api_key_env)
            .filter(|v| !v.is_empty())
            .map(ApiKey::new)
    }
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
threads = 4
vad = "energy"                       # energy | silero (M2)

[detect]
sensitivity = "balanced"             # eager | balanced | strict

[llm]
provider = "ollama"                  # ollama | openai | anthropic
model = "qwen3.6:35b-a3b"           # measured 2026-09-21 on the dev 4090; see CLAUDE.md
base_url = "http://localhost:11434"
api_key_env = ""                     # name of an env var; never the key itself
answer_length = "concise"            # concise (5) | minimal (3)
fast_timeout_ms = 4000
slow_timeout_ms = 20000

[ui]
always_on_top = true
force_x11 = false                    # launch under XWayland for keep-above
opacity = 1.0
never_auto_expand = true

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
        assert_eq!(keys.len(), 22, "spec example has 22 keys: {keys:?}");
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
}
