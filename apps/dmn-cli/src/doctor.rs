//! `dmn doctor`: one line per check, a fix for every failure, exit 1 if a required check fails.
//!
//! Every check is a pure function over injected inputs ([`Runner`] for commands and PATH,
//! [`Http`] for the Ollama endpoint, [`Resolver`] for the privacy line) so the unit tests run
//! without PipeWire, Ollama or DNS, none of which CI has. `--probe` (play a tone and capture it back, issue #5) lives in `probe.rs`.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use dmn_core::config::{LookupProvider, Provider, SpeakEngine};
use dmn_core::{Config, Paths};

/// Result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    /// A failed required check makes `dmn doctor` exit 1; informational checks never do.
    pub required: bool,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    pub(crate) fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            required: true,
            detail: detail.into(),
            fix: None,
        }
    }
    pub(crate) fn fail(
        name: &'static str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            name,
            ok: false,
            required: true,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
    pub(crate) fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

/// Runs commands and looks up PATH. The real one shells out; tests use a fake.
pub trait Runner {
    /// stdout of `cmd args...` when it exits 0, else `None`.
    fn run(&self, cmd: &str, args: &[&str]) -> Option<String>;
    fn on_path(&self, cmd: &str) -> bool;
}

/// Fetches a URL. The real one is a tiny HTTP/1.1 client; tests return canned bodies.
pub trait Http {
    fn get(&self, url: &str) -> Result<String, String>;
}

/// Resolves a hostname to addresses. Injected so the privacy line is testable offline.
pub trait Resolver {
    /// Addresses for `host`, or `None` when it cannot be resolved.
    fn resolve(&self, host: &str) -> Option<Vec<IpAddr>>;
}

pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(&self, host: &str) -> Option<Vec<IpAddr>> {
        let addrs: Vec<IpAddr> = (host, 0u16)
            .to_socket_addrs()
            .ok()?
            .map(|a| a.ip())
            .collect();
        (!addrs.is_empty()).then_some(addrs)
    }
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
    fn on_path(&self, cmd: &str) -> bool {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|d| d.join(cmd).is_file())
    }
}

/// Std-only HTTP/1.1 GET with a 2 s timeout; enough for Ollama's `/api/tags` on localhost.
/// No async runtime enters the workspace before issue #11.
pub struct StdHttp;

const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

impl Http for StdHttp {
    fn get(&self, url: &str) -> Result<String, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("only http:// URLs are supported, got {url}"))?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let hostport_with_port = if hostport.contains(':') {
            hostport.to_owned()
        } else {
            format!("{hostport}:80")
        };
        // Try every address: `localhost` often resolves to ::1 first while Ollama listens on
        // 127.0.0.1 only.
        let addrs: Vec<_> = hostport_with_port
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {hostport}: {e}"))?
            .collect();
        let mut last_err = format!("cannot resolve {hostport}");
        let mut connected = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, HTTP_TIMEOUT) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => last_err = format!("cannot connect to {addr}: {e}"),
            }
        }
        let mut stream = connected.ok_or(last_err)?;
        stream
            .set_read_timeout(Some(HTTP_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(HTTP_TIMEOUT)))
            .map_err(|e| e.to_string())?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )
        .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
        parse_http_response(&raw)
    }
}

/// Split status line, headers and body; decode chunked bodies. Returns the body on 2xx.
///
/// Works on bytes throughout: chunk sizes are byte counts and a chunk boundary may fall inside
/// a multi-byte UTF-8 character, so the body is only converted to text once reassembled.
fn parse_http_response(raw: &[u8]) -> Result<String, String> {
    let split = find(raw, b"\r\n\r\n").ok_or_else(|| "malformed HTTP response".to_owned())?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let status = head.lines().next().unwrap_or_default();
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("malformed status line: {status}"))?;
    if !(200..300).contains(&code) {
        return Err(format!("HTTP {code}"));
    }
    // Header names and transfer-coding values are both case-insensitive: "Transfer-Encoding:
    // Chunked" is as valid as the lowercase spelling (Copilot on PR #18).
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_vec()
    };
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(eol) = find(rest, b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..eol]);
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("0").trim(), 16)
            .unwrap_or(0);
        let after = &rest[eol + 2..];
        if size == 0 || after.len() < size {
            break;
        }
        out.extend_from_slice(&after[..size]);
        rest = after[size..].strip_prefix(b"\r\n").unwrap_or(&[]);
    }
    out
}

// ---- checks ---------------------------------------------------------------------------------

pub fn libpipewire_headers(r: &dyn Runner) -> Check {
    match r.run("pkg-config", &["--modversion", "libpipewire-0.3"]) {
        Some(v) => Check::pass("libpipewire headers", v.trim().to_owned()),
        None => Check::fail(
            "libpipewire headers",
            "pkg-config cannot find libpipewire-0.3",
            "sudo apt install libpipewire-0.3-dev clang libclang-dev cmake",
        ),
    }
}

pub fn pipewire_server(r: &dyn Runner) -> Check {
    match r.run("pw-cli", &["info", "0"]) {
        Some(info) => {
            let version = info
                .lines()
                .find_map(|l| l.trim().strip_prefix("version:"))
                .map(|v| v.trim().trim_matches('"').to_owned())
                .unwrap_or_else(|| "unknown version".to_owned());
            Check::pass("pipewire server", format!("running, {version}"))
        }
        None => Check::fail(
            "pipewire server",
            "pw-cli cannot reach a PipeWire server",
            "systemctl --user start pipewire pipewire-pulse wireplumber",
        ),
    }
}

pub fn pipewire_tools(r: &dyn Runner) -> Check {
    let tools = ["pw-record", "pw-play", "pw-dump"];
    let missing: Vec<&str> = tools.iter().copied().filter(|t| !r.on_path(t)).collect();
    if missing.is_empty() {
        Check::pass("pipewire tools", tools.join(" ")).optional()
    } else {
        Check::fail(
            "pipewire tools",
            format!(
                "missing on PATH: {} (emergency fallback only)",
                missing.join(" ")
            ),
            "sudo apt install pipewire-bin",
        )
        .optional()
    }
}

pub fn whisper_model(
    models_dir: &Path,
    model: &str,
    language: &dmn_core::config::SttLanguage,
) -> Check {
    let m = match dmn_stt::Model::from_name(model) {
        Ok(m) => m,
        Err(e) => {
            return Check::fail(
                "whisper model",
                e.to_string(),
                "set [stt].model in ~/.config/dmn/config.toml to one of the known names",
            );
        }
    };
    // #77: `[stt].language` other than `en` needs a multilingual model, and the code must be
    // one whisper knows; both are refused here with the fix rather than at `dmn run`.
    if let Err(e) = dmn_stt::Language::parse(language.label()).and_then(|l| l.check(m)) {
        return Check::fail(
            "whisper model",
            e.to_string(),
            "set [stt].model to small, medium or large-v3 (dmn doctor --fetch <model>), or [stt].language to \"en\"",
        );
    }
    let file = dmn_stt::model_path(models_dir, m);
    if !file.exists() {
        return Check::fail(
            "whisper model",
            format!("{} missing", file.display()),
            format!("dmn doctor --fetch {model}"),
        );
    }
    // #84: the size, then the SHA-256 against the published file: a model with a few wrong
    // bytes loads fine and transcribes nothing, so the size alone is not enough.
    match dmn_stt::verify_model(&file, m) {
        Ok(()) => Check::pass("whisper model", format!("{} (sha256 ok)", file.display())),
        Err(dmn_stt::SttError::SizeMismatch { got, expected, .. }) => Check::fail(
            "whisper model",
            format!(
                "{} is {got} bytes, expected {expected} (partial download?)",
                file.display()
            ),
            format!("dmn doctor --fetch {model}"),
        ),
        Err(dmn_stt::SttError::ChecksumMismatch { got, expected, .. }) => Check::fail(
            "whisper model",
            format!(
                "{} has the right size but sha256 {got}, not the published {expected} (damaged file)",
                file.display()
            ),
            format!("dmn doctor --fetch {model}"),
        ),
        Err(e) => Check::fail(
            "whisper model",
            e.to_string(),
            format!("dmn doctor --fetch {model}"),
        ),
    }
}

/// `[stt].vad = "silero"` (#62): the model file is there and is exactly the pinned release (size
/// and SHA-256), so a missing or wrong file fails here, with the fix, instead of at `dmn run`.
pub fn silero_model(models_dir: &Path) -> Check {
    let file = dmn_vad::silero::model_path(models_dir);
    match dmn_vad::silero::verify_model(&file) {
        Ok(()) => Check::pass("silero model", file.display().to_string()),
        Err(dmn_vad::SileroError::Missing { .. }) => Check::fail(
            "silero model",
            format!("{} missing ([stt].vad = \"silero\")", file.display()),
            "dmn doctor --fetch silero",
        ),
        Err(dmn_vad::SileroError::Mismatch { size, sha256, .. }) => Check::fail(
            "silero model",
            format!(
                "{} is {size} bytes, sha256 {sha256}; expected {} bytes, sha256 {}",
                file.display(),
                dmn_vad::silero::MODEL_SIZE,
                dmn_vad::silero::MODEL_SHA256
            ),
            format!("rm {} && dmn doctor --fetch silero", file.display()),
        ),
        Err(e) => Check::fail(
            "silero model",
            e.to_string(),
            format!("rm {} && dmn doctor --fetch silero", file.display()),
        ),
    }
}

/// Two checks: the server answers, and the configured model is pulled.
pub fn ollama(h: &dyn Http, base_url: &str, model: &str) -> Vec<Check> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let body = match h.get(&url) {
        Ok(b) => b,
        Err(e) => {
            return vec![Check::fail(
                "ollama",
                format!("{url}: {e}"),
                "ollama serve   (or: curl -fsSL https://ollama.com/install.sh | sh)",
            )];
        }
    };
    // A 2xx body we cannot parse is not an empty model list: it is a proxy, a captive portal or
    // the wrong endpoint, and `ollama pull` would not fix it (Copilot on PR #18).
    let Some(names) = ollama_model_names(&body) else {
        return vec![Check::fail(
            "ollama",
            format!("unexpected response from {url}: not the JSON /api/tags returns"),
            format!("check that {base_url} is an Ollama server (curl {url}) and not a proxy"),
        )];
    };
    let mut out = vec![Check::pass(
        "ollama",
        format!("{base_url} reachable, {} model(s)", names.len()),
    )];
    if names
        .iter()
        .any(|n| n == model || n.strip_suffix(":latest") == Some(model))
    {
        out.push(Check::pass("ollama model", format!("{model} present")));
    } else {
        out.push(Check::fail(
            "ollama model",
            format!("{model} not pulled"),
            format!("ollama pull {model}"),
        ));
    }
    out
}

/// Model names from an `/api/tags` body, or `None` when the body is not that shape at all
/// (unparsable JSON, or no `models` array). An Ollama with nothing pulled answers
/// `{"models":[]}`, which is `Some(vec![])` and a different situation.
fn ollama_model_names(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v.get("models")?.as_array()?;
    // Every entry must carry a string name. Dropping nameless entries would turn a malformed
    // body into "no models pulled" and an `ollama pull` fix again (Copilot on PR #32).
    arr.iter()
        .map(|m| m.get("name")?.as_str().map(str::to_owned))
        .collect()
}

/// For remote providers the key must be present in the named env var.
pub fn remote_api_key(cfg: &Config, lookup: impl Fn(&str) -> Option<String>) -> Check {
    let var = &cfg.llm.api_key_env;
    if var.is_empty() {
        return Check::fail(
            "api key",
            "[llm].api_key_env is empty",
            "set api_key_env = \"<ENV_VAR_NAME>\" in config.toml and export that variable",
        );
    }
    match cfg.api_key_from(lookup) {
        Some(_) => Check::pass("api key", format!("read from ${var} (never logged)")),
        None => Check::fail(
            "api key",
            format!("${var} is unset or empty"),
            format!("export {var}=<your key>"),
        ),
    }
}

/// #102: the web lookup, informational either way: off (nothing is searched), ready (on, with
/// its key), or on with the key variable unset (every lookup fails and the first answer stays;
/// nothing else breaks, so this warns rather than fails the run).
///
/// For `searxng` (on PR #104) there is no key: with the lookup on, doctor asks the instance one
/// search (`dmn`) with the 2 s timeout every doctor request has, and reports it unreachable, or
/// its JSON format disabled (403), as a warning. Doctor's client speaks plain `http://`, so an
/// `https://` instance is reported by its URL, unprobed.
pub fn web_lookup(cfg: &Config, h: &dyn Http, lookup: impl Fn(&str) -> Option<String>) -> Check {
    let l = &cfg.lookup;
    if !l.enabled {
        return Check::pass(
            "web lookup",
            "off ([lookup].enabled = false): no search query leaves this machine",
        )
        .optional();
    }
    let at = l.endpoint();
    if l.provider == LookupProvider::Searxng {
        return searxng(&at, h);
    }
    let var = &l.api_key_env;
    match l.api_key_from(lookup) {
        Some(_) => Check::pass(
            "web lookup",
            format!(
                "ready: {} web search at {at}, key from ${var} (never logged); a search query leaves this machine",
                l.provider.as_str()
            ),
        )
        .optional(),
        None => {
            let shown = if var.is_empty() {
                "[lookup].api_key_env is empty".to_owned()
            } else {
                format!("${var} is unset or empty")
            };
            Check::fail(
                "web lookup",
                format!("on, but {shown}: every lookup fails and the first answer stays"),
                if var.is_empty() {
                    "set api_key_env = \"OLLAMA_API_KEY\" in [lookup] and export that variable"
                        .to_owned()
                } else {
                    format!("export {var}=<your ollama.com key>, or set [lookup].enabled = false")
                },
            )
            .optional()
        }
    }
}

/// The `searxng` half of [`web_lookup`].
fn searxng(at: &str, h: &dyn Http) -> Check {
    let ready = |how: String| {
        Check::pass(
            "web lookup",
            format!(
                "ready: SearXNG at {at} ({how}); a search query goes there and on to the engines it is set up with"
            ),
        )
        .optional()
    };
    if !at.starts_with("http://") {
        return ready("not probed: doctor checks http:// instances only".to_owned());
    }
    let warn =
        |detail: String, fix: &str| Check::fail("web lookup", detail, fix.to_owned()).optional();
    match h.get(&format!("{at}/search?q=dmn&format=json")) {
        Ok(body) => match dmn_llm::lookup::parse_results("searxng", &body) {
            Ok(results) => ready(format!("answered with {} result(s)", results.len())),
            Err(e) => warn(
                format!("SearXNG at {at} answered, but not with search results: {e}"),
                "check [lookup].base_url points at a SearXNG instance",
            ),
        },
        Err(e) if e == "HTTP 403" => warn(
            format!(
                "SearXNG at {at}: JSON format disabled (403): every lookup fails and the first answer stays"
            ),
            "add `json` to `search.formats` in the instance's settings.yml and restart it",
        ),
        Err(e) => warn(
            format!(
                "SearXNG unreachable at {at}: {e}: every lookup fails and the first answer stays"
            ),
            "start the instance, or set [lookup].base_url to where it runs",
        ),
    }
}

/// `[speak]` (#113): the model `t` translates with and whether Ollama has it, and the TTS
/// server's device from its `/health`. One informational line while speak is off.
pub fn speak(cfg: &Config, h: &dyn Http) -> Vec<Check> {
    if !cfg.speak.enabled {
        return vec![Check::pass("speak", "off ([speak].enabled = false)").optional()];
    }
    vec![translator(cfg, h), tts_server(cfg, h)]
}

/// The translator model: `[speak].model`, or `[llm].model` when it is left out.
fn translator(cfg: &Config, h: &dyn Http) -> Check {
    let model = cfg.translator_model();
    let whose = if cfg.translator_is_separate() {
        format!(
            "[speak].model; [llm].model {} is not loaded while assistance is off",
            cfg.llm.model
        )
    } else {
        "[llm].model; a small [speak].model (llama3.1:8b) keeps the answer model out of VRAM with assistance off".to_owned()
    };
    if cfg.llm.provider != Provider::Ollama {
        return Check::pass(
            "translator",
            format!("{model} on {} ({whose})", cfg.llm.provider.as_str()),
        );
    }
    let url = format!("{}/api/tags", cfg.llm.base_url.trim_end_matches('/'));
    let names = match h.get(&url) {
        Ok(body) => ollama_model_names(&body),
        Err(e) => {
            return Check::fail(
                "translator",
                format!("{model}: cannot ask Ollama at {url}: {e}"),
                "start Ollama: `ollama serve`",
            );
        }
    };
    match names {
        Some(names)
            if names
                .iter()
                .any(|n| n == model || n.strip_suffix(":latest") == Some(model)) =>
        {
            Check::pass("translator", format!("{model} present ({whose})"))
        }
        Some(_) => Check::fail(
            "translator",
            format!("{model} not pulled: `t` cannot translate"),
            format!("ollama pull {model}"),
        ),
        None => Check::fail(
            "translator",
            format!("{model}: unexpected response from {url}"),
            "check [llm].base_url points at Ollama",
        ),
    }
}

/// The TTS server for `[speak].engine` (#122): Chatterbox's `/health` (up, on `cuda` or
/// `cpu`), or the model vLLM-Omni serves for Qwen3-TTS.
fn tts_server(cfg: &Config, h: &dyn Http) -> Check {
    let at = cfg.speak.tts_url.trim().trim_end_matches('/');
    if !cfg.speak.is_local() {
        let local = match cfg.speak.engine {
            SpeakEngine::Chatterbox => "http://localhost:4123",
            SpeakEngine::Qwen3 => "http://localhost:4124",
        };
        return Check::fail(
            "tts server",
            format!("{at} is not on this machine; dmn sends text and your voice only to localhost"),
            format!("set [speak].tts_url to {local}"),
        );
    }
    match cfg.speak.engine {
        SpeakEngine::Chatterbox => chatterbox_server(at, h),
        SpeakEngine::Qwen3 => qwen3_server(at, h),
    }
}

/// Qwen3-TTS on vLLM-Omni: `GET /v1/models` lists the model it serves. vLLM-Omni runs on the
/// GPU only, so the device is `cuda` whenever it answers.
fn qwen3_server(at: &str, h: &dyn Http) -> Check {
    let start =
        "start it: docker compose -f sidecars/qwen3-tts/compose.yaml up -d (README § Speak)";
    let body = match h.get(&format!("{at}/v1/models")) {
        Ok(body) => body,
        Err(e) => {
            return Check::fail(
                "tts server",
                format!(
                    "engine qwen3: {at} is not answering ({e}); `dmn run` with [speak] on will not start"
                ),
                start,
            );
        }
    };
    let models: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .map(|d| d.iter().filter_map(|m| m["id"].as_str()).collect())
        .unwrap_or_default();
    if ids.contains(&dmn_tts::QWEN3_MODEL) {
        Check::pass(
            "tts server",
            format!(
                "engine qwen3: {at} serves {} on vLLM-Omni, device cuda",
                dmn_tts::QWEN3_MODEL
            ),
        )
    } else {
        Check::fail(
            "tts server",
            format!(
                "engine qwen3: {at} serves {}, not {}",
                if ids.is_empty() {
                    "no model it names".to_owned()
                } else {
                    ids.join(", ")
                },
                dmn_tts::QWEN3_MODEL
            ),
            format!("point [speak].tts_url at the Qwen3-TTS sidecar, or {start}"),
        )
    }
}

/// chatterbox-tts-api's `/health`: up, and on `cuda` or `cpu`. A CPU server works but renders
/// about 4-5× slower than real time, so it is a warning with the way to the GPU.
fn chatterbox_server(at: &str, h: &dyn Http) -> Check {
    let body = match h.get(&format!("{at}/health")) {
        Ok(body) => body,
        Err(e) => {
            return Check::fail(
                "tts server",
                format!(
                    "engine chatterbox: {at} is not answering ({e}); `dmn run` with [speak] on will not start"
                ),
                "start chatterbox-tts-api (README § Speak)",
            );
        }
    };
    let health: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let status = health["status"].as_str().unwrap_or("unknown");
    match health["device"].as_str() {
        Some("cpu") => Check::fail(
            "tts server",
            format!(
                "engine chatterbox: {at} {status}, device cpu: about 4-5x slower than real time, 8-18 s to the first sound of a sentence"
            ),
            "run it on the GPU (1.9 s for a sentence): README § Speak, docker/docker-compose.gpu.yml",
        )
        .optional(),
        Some(device) => Check::pass(
            "tts server",
            format!("engine chatterbox: {at} {status}, device {device}"),
        ),
        None => Check::pass(
            "tts server",
            format!("engine chatterbox: {at} answering; its /health names no device"),
        ),
    }
}

/// Informational (#125): `[audio].echo_guard`, and what it costs while it is on: each of the
/// user's finals waits up to 1.5 s for a copy on the call's stream, and with `[speak]` on every
/// translation waits with it.
pub fn echo_guard(cfg: &Config) -> Check {
    if !cfg.audio.echo_guard {
        return Check::pass(
            "echo guard",
            "off: your lines are not held (with speakers, set [audio].echo_guard = true)",
        )
        .optional();
    }
    let speak = if cfg.speak.enabled {
        "; [speak] translations start that much later too"
    } else {
        ""
    };
    Check::pass(
        "echo guard",
        format!(
            "on: each of your sentences waits up to 1.5 s (4 s while the call is talking) for an echo{speak}; on headphones set [audio].echo_guard = false"
        ),
    )
    .optional()
}

/// Informational: which GPU features this binary carries and which GPU tools exist.
pub fn gpu_backend(r: &dyn Runner) -> Check {
    let mut built = Vec::new();
    if cfg!(feature = "cuda") {
        built.push("cuda");
    }
    if cfg!(feature = "vulkan") {
        built.push("vulkan");
    }
    let built = if built.is_empty() {
        "built without cuda/vulkan (CPU whisper)".to_owned()
    } else {
        format!("built with {}", built.join("+"))
    };
    let tool = |t: &str| {
        if r.on_path(t) {
            format!("{t} present")
        } else {
            format!("{t} missing")
        }
    };
    Check::pass(
        "gpu backend",
        format!("{built}; {}, {}", tool("nvidia-smi"), tool("vulkaninfo")),
    )
    .optional()
}

/// Where whisper would run under `[stt].device` (#61), and every GPU this build can see. A GPU
/// asked for and unavailable fails this (optional) check with the reason; the run itself still
/// falls back to the CPU.
pub fn whisper_device(
    choice: dmn_core::config::SttDevice,
    devices: &[dmn_stt::GpuDevice],
) -> Check {
    let placement = dmn_stt::gpu::resolve(
        crate::pipeline::device_choice(choice),
        dmn_stt::Backend::compiled(),
        devices,
    );
    let seen = if devices.is_empty() {
        "no GPU visible to this build".to_owned()
    } else {
        let list: Vec<String> = devices
            .iter()
            .map(|d| {
                let kind = if d.integrated { ", integrated" } else { "" };
                format!("{}{kind}", d.label())
            })
            .collect();
        format!("GPUs: {}", list.join("; "))
    };
    let setting = format!("{choice:?}").to_lowercase();
    let detail = format!(
        "[stt].device = {setting} → whisper on {}; {seen}",
        placement.label()
    );
    let check = if placement.is_fallback() {
        Check::fail(
            "whisper device",
            detail,
            "build with --features vulkan (or cuda), or set [stt].device = \"cpu\"",
        )
    } else {
        Check::pass("whisper device", detail)
    };
    check.optional()
}

// ---- privacy --------------------------------------------------------------------------------

/// Where the configured LLM endpoint actually is. `provider = "ollama"` says nothing about
/// this: `base_url` can point at another host, and then transcript text leaves the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locality {
    /// Every resolved address is a loopback address.
    Local,
    /// At least one resolved address is not loopback, or the provider is a remote API.
    Remote,
    /// The host could not be resolved, so locality cannot be proven; treated as remote.
    Unresolved,
}

/// The host part of `http://host:port/path`, without brackets for IPv6 literals.
fn host_of(url: &str) -> &str {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    match rest.strip_prefix('[') {
        // [::1]:11434
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => rest.split(':').next().unwrap_or(rest),
    }
}

/// Decide from the resolved host, not from the provider enum (review finding on PR #18).
pub fn endpoint_locality(cfg: &Config, r: &dyn Resolver) -> Locality {
    if cfg.llm.provider.is_remote() {
        return Locality::Remote;
    }
    let host = host_of(&cfg.llm.base_url);
    if host.is_empty() {
        return Locality::Unresolved;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if ip.is_loopback() {
            Locality::Local
        } else {
            Locality::Remote
        };
    }
    match r.resolve(host) {
        Some(addrs) if addrs.iter().all(IpAddr::is_loopback) => Locality::Local,
        Some(_) => Locality::Remote,
        None => Locality::Unresolved,
    }
}

/// The one-sentence privacy statement in the report header (SPEC § Privacy rules).
fn privacy_line(cfg: &Config, locality: &Locality) -> String {
    let host = host_of(&cfg.llm.base_url);
    let line = llm_privacy(host, locality);
    // #102: what the web lookup sends, while it is on.
    let endpoint = cfg.lookup.endpoint();
    let host = host_of(&endpoint);
    // On PR #104: a SearXNG is the user's own, and it forwards the query to its engines.
    let search = match cfg.lookup.provider {
        LookupProvider::Ollama => host.to_owned(),
        LookupProvider::Searxng => {
            format!("your SearXNG at {host}, which forwards it to its search engines")
        }
    };
    match (cfg.lookup.enabled, locality) {
        (false, _) => line,
        (true, Locality::Local) => format!(
            "nothing leaves this machine but a web search query ([lookup] on: a few words the model writes, no transcript, go to {search})"
        ),
        (true, _) => format!("{line}; [lookup] on: a web search query also goes to {search}"),
    }
}

fn llm_privacy(host: &str, locality: &Locality) -> String {
    match locality {
        Locality::Local => "nothing leaves this machine".to_owned(),
        Locality::Remote => format!(
            "transcript text, retrieved chunks and the session summary LEAVE this machine ({host} is not this machine)"
        ),
        Locality::Unresolved => format!(
            "cannot resolve {host}: assume transcript text, retrieved chunks and the session summary LEAVE this machine"
        ),
    }
}

// ---- report ----------------------------------------------------------------------------------

pub struct Report {
    pub header: String,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed_required(&self) -> usize {
        self.checks.iter().filter(|c| c.required && !c.ok).count()
    }

    pub fn render(&self) -> String {
        let mut s = self.header.clone();
        for c in &self.checks {
            let mark = match (c.ok, c.required) {
                (true, true) => " ok ",
                (true, false) => "info",
                (false, true) => "FAIL",
                (false, false) => "warn",
            };
            let _ = writeln!(s, "{mark} {:<20} {}", c.name, c.detail);
            if let Some(fix) = &c.fix {
                let _ = writeln!(s, "     fix: {fix}");
            }
        }
        let failed = self.failed_required();
        let required = self.checks.iter().filter(|c| c.required).count();
        if failed == 0 {
            let _ = writeln!(s, "all {required} required checks passed");
        } else {
            let _ = writeln!(s, "{failed} of {required} required checks failed");
        }
        s
    }
}

pub fn run_all(
    cfg: &Config,
    paths: &Paths,
    r: &dyn Runner,
    h: &dyn Http,
    res: &dyn Resolver,
) -> Report {
    let mut header = String::new();
    let _ = writeln!(header, "config:   {}", paths.config_file.display());
    let _ = writeln!(header, "log:      {}", paths.log_file.display());
    let provider = format!("{:?}", cfg.llm.provider).to_lowercase();
    let privacy = privacy_line(cfg, &endpoint_locality(cfg, res));
    let _ = writeln!(
        header,
        "provider: {provider}, model {} at {} — {privacy}",
        cfg.llm.model, cfg.llm.base_url
    );

    let mut checks = vec![
        libpipewire_headers(r),
        pipewire_server(r),
        pipewire_tools(r),
        whisper_model(&paths.models_dir, &cfg.stt.model, &cfg.stt.language),
    ];
    if cfg.stt.vad == dmn_core::config::Vad::Silero {
        checks.push(silero_model(&paths.models_dir));
    }
    match cfg.llm.provider {
        Provider::Ollama => checks.extend(ollama(h, &cfg.llm.base_url, &cfg.llm.model)),
        Provider::OpenAI | Provider::Anthropic => {
            checks.push(remote_api_key(cfg, |name| std::env::var(name).ok()));
        }
    }
    checks.push(web_lookup(cfg, h, |name| std::env::var(name).ok()));
    checks.extend(speak(cfg, h));
    checks.push(echo_guard(cfg));
    checks.push(gpu_backend(r));
    checks.push(whisper_device(cfg.stt.device, &dmn_stt::gpu_devices()));
    Report { header, checks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeRunner {
        outputs: HashMap<String, String>,
        path: Vec<&'static str>,
    }
    impl FakeRunner {
        fn with(mut self, cmd: &str, args: &[&str], out: &str) -> Self {
            self.outputs
                .insert(format!("{cmd} {}", args.join(" ")), out.to_owned());
            self
        }
        fn on_path(mut self, tools: &[&'static str]) -> Self {
            self.path.extend_from_slice(tools);
            self
        }
    }
    impl Runner for FakeRunner {
        fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
            self.outputs
                .get(&format!("{cmd} {}", args.join(" ")))
                .cloned()
        }
        fn on_path(&self, cmd: &str) -> bool {
            self.path.contains(&cmd)
        }
    }

    struct FakeHttp(Result<String, String>);
    impl Http for FakeHttp {
        fn get(&self, _url: &str) -> Result<String, String> {
            self.0.clone()
        }
    }

    /// Resolves the hosts it was given and nothing else.
    struct FakeResolver(HashMap<&'static str, Vec<IpAddr>>);
    impl FakeResolver {
        fn local() -> Self {
            Self(HashMap::new()).with("localhost", &["127.0.0.1", "::1"])
        }
        fn with(mut self, host: &'static str, ips: &[&str]) -> Self {
            self.0.insert(
                host,
                ips.iter()
                    .filter_map(|i| i.parse().ok())
                    .collect::<Vec<_>>(),
            );
            self
        }
    }
    impl Resolver for FakeResolver {
        fn resolve(&self, host: &str) -> Option<Vec<IpAddr>> {
            self.0.get(host).cloned()
        }
    }

    const TAGS: &str = r#"{"models":[{"name":"qwen3.6:35b-a3b","size":1},{"name":"nomic-embed-text:latest","size":2}]}"#;

    #[test]
    fn headers_pass_and_fail_with_apt_fix() {
        let ok = libpipewire_headers(&FakeRunner::default().with(
            "pkg-config",
            &["--modversion", "libpipewire-0.3"],
            "1.6.8\n",
        ));
        assert!(ok.ok);
        assert_eq!(ok.detail, "1.6.8");
        let bad = libpipewire_headers(&FakeRunner::default());
        assert!(!bad.ok && bad.required);
        assert!(bad.fix.as_deref().unwrap().contains("libpipewire-0.3-dev"));
    }

    #[test]
    fn server_version_is_parsed_from_pw_cli() {
        let info = "\tid: 0\n\ttype: PipeWire:Interface:Core/4\n\tversion: \"1.5.85\"\n\tname: \"pipewire-0\"\n";
        let c = pipewire_server(&FakeRunner::default().with("pw-cli", &["info", "0"], info));
        assert!(c.ok);
        assert_eq!(c.detail, "running, 1.5.85");
        let down = pipewire_server(&FakeRunner::default());
        assert!(!down.ok);
        assert!(
            down.fix
                .as_deref()
                .unwrap()
                .contains("systemctl --user start pipewire")
        );
    }

    #[test]
    fn tools_are_optional() {
        let all =
            pipewire_tools(&FakeRunner::default().on_path(&["pw-record", "pw-play", "pw-dump"]));
        assert!(all.ok && !all.required);
        let some = pipewire_tools(&FakeRunner::default().on_path(&["pw-record"]));
        assert!(!some.ok && !some.required);
        assert!(some.detail.contains("pw-play pw-dump"));
    }

    #[test]
    fn silero_check_fails_clearly_on_a_missing_or_wrong_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = silero_model(dir.path());
        assert!(!missing.ok && missing.required);
        assert_eq!(missing.fix.as_deref(), Some("dmn doctor --fetch silero"));
        std::fs::write(dmn_vad::silero::model_path(dir.path()), b"not a model").unwrap();
        let wrong = silero_model(dir.path());
        assert!(!wrong.ok, "{}", wrong.detail);
        assert!(wrong.detail.contains("sha256"), "{}", wrong.detail);
        assert!(
            wrong
                .fix
                .as_deref()
                .unwrap()
                .ends_with("dmn doctor --fetch silero"),
            "{:?}",
            wrong.fix
        );
    }

    #[test]
    fn model_file_check_names_the_fetch_command() {
        let dir = tempfile::tempdir().unwrap();
        let missing = whisper_model(
            dir.path(),
            "small.en",
            &dmn_core::config::SttLanguage::English,
        );
        assert!(!missing.ok);
        assert!(
            missing
                .fix
                .as_deref()
                .unwrap()
                .starts_with("dmn doctor --fetch small.en")
        );
        let file = dir.path().join("ggml-small.en.bin");
        std::fs::write(&file, b"x").unwrap();
        let partial = whisper_model(
            dir.path(),
            "small.en",
            &dmn_core::config::SttLanguage::English,
        );
        assert!(
            !partial.ok && partial.detail.contains("partial download"),
            "{}",
            partial.detail
        );
        assert_eq!(
            partial.fix.as_deref(),
            Some("dmn doctor --fetch small.en"),
            "--fetch replaces a wrong-sized file itself"
        );
        // #84: the right size and the wrong bytes (zeros) is a damaged file, not a pass.
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(dmn_stt::Model::SmallEn.size_bytes())
            .unwrap();
        use dmn_core::config::SttLanguage;
        let damaged = whisper_model(dir.path(), "small.en", &SttLanguage::English);
        assert!(
            !damaged.ok && damaged.detail.contains("damaged file"),
            "{damaged:?}"
        );
        assert_eq!(damaged.fix.as_deref(), Some("dmn doctor --fetch small.en"));
        assert!(
            !whisper_model(dir.path(), "smallish", &SttLanguage::English).ok,
            "unknown names fail"
        );
        // #77: the English-only file cannot detect or translate.
        let en_only = whisper_model(dir.path(), "small.en", &SttLanguage::Auto);
        assert!(
            !en_only.ok && en_only.detail.contains("English-only"),
            "{en_only:?}"
        );
        let turbo = whisper_model(dir.path(), "large-v3-turbo", &SttLanguage::Auto);
        assert!(
            !turbo.ok && turbo.detail.contains("not trained to translate"),
            "{turbo:?}"
        );
        let unknown = whisper_model(dir.path(), "small.en", &SttLanguage::Code("xx".into()));
        assert!(
            !unknown.ok && unknown.detail.contains("unknown language"),
            "{unknown:?}"
        );
    }

    #[test]
    fn ollama_unreachable_prints_serve_fix() {
        let checks = ollama(
            &FakeHttp(Err("connection refused".into())),
            "http://localhost:11434",
            "qwen3.6:35b-a3b",
        );
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].ok);
        assert!(checks[0].detail.contains("/api/tags"));
        assert!(
            checks[0]
                .fix
                .as_deref()
                .unwrap()
                .starts_with("ollama serve")
        );
    }

    #[test]
    fn ollama_model_missing_prints_pull_fix() {
        let checks = ollama(
            &FakeHttp(Ok(TAGS.into())),
            "http://localhost:11434/",
            "llama3.1:8b",
        );
        assert!(checks[0].ok);
        assert!(checks[0].detail.contains("2 model(s)"));
        assert!(!checks[1].ok);
        assert_eq!(checks[1].fix.as_deref(), Some("ollama pull llama3.1:8b"));
    }

    #[test]
    fn ollama_model_present_with_or_without_latest_tag() {
        let a = ollama(
            &FakeHttp(Ok(TAGS.into())),
            "http://localhost:11434",
            "qwen3.6:35b-a3b",
        );
        assert!(a[1].ok);
        let b = ollama(
            &FakeHttp(Ok(TAGS.into())),
            "http://localhost:11434",
            "nomic-embed-text",
        );
        assert!(b[1].ok, "name:latest matches a bare name");
    }

    #[test]
    fn remote_provider_requires_key_env() {
        let mut cfg = Config::default();
        cfg.llm.provider = Provider::Anthropic;
        let none = remote_api_key(&cfg, |_| None);
        assert!(!none.ok && none.detail.contains("api_key_env is empty"));
        cfg.llm.api_key_env = "ANTHROPIC_API_KEY".into();
        let unset = remote_api_key(&cfg, |_| None);
        assert_eq!(
            unset.fix.as_deref(),
            Some("export ANTHROPIC_API_KEY=<your key>")
        );
        let set = remote_api_key(&cfg, |_| Some("sk-secret".into()));
        assert!(set.ok);
        assert!(!set.detail.contains("secret"));
    }

    #[test]
    fn doctor_reports_the_three_web_lookup_states() {
        let mut cfg = Config::default();
        let h = FakeHttp(Err("never asked".into()));
        let off = web_lookup(&cfg, &h, |_| Some("ol-secret".into()));
        assert!(off.ok && !off.required, "{off:?}");
        assert!(off.detail.starts_with("off"), "{}", off.detail);

        cfg.lookup.enabled = true;
        let ready = web_lookup(&cfg, &h, |name| {
            (name == "OLLAMA_API_KEY").then(|| "ol-secret".to_owned())
        });
        assert!(ready.ok && !ready.required, "{ready:?}");
        assert!(ready.detail.starts_with("ready"), "{}", ready.detail);
        assert!(
            ready.detail.contains("https://ollama.com"),
            "{}",
            ready.detail
        );
        assert!(ready.detail.contains("$OLLAMA_API_KEY"), "{}", ready.detail);
        assert!(!ready.detail.contains("ol-secret"));

        let unset = web_lookup(&cfg, &h, |_| None);
        assert!(
            !unset.ok && !unset.required,
            "a warning, not a failure: {unset:?}"
        );
        assert!(
            unset.detail.contains("$OLLAMA_API_KEY is unset"),
            "{}",
            unset.detail
        );
        assert!(
            unset
                .fix
                .as_deref()
                .unwrap_or("")
                .contains("export OLLAMA_API_KEY=")
        );

        // The header says what leaves, only while it is on.
        let res = FakeResolver::local();
        let on = privacy_line(&cfg, &endpoint_locality(&cfg, &res));
        assert!(
            on.starts_with("nothing leaves this machine but a web search query"),
            "{on}"
        );
        assert!(on.contains("no transcript, go to ollama.com"), "{on}");
        cfg.lookup.enabled = false;
        assert_eq!(
            privacy_line(&cfg, &endpoint_locality(&cfg, &res)),
            "nothing leaves this machine"
        );
    }

    #[test]
    fn doctor_reports_searxng_off_ready_unreachable_and_json_disabled() {
        let mut cfg = Config::default();
        cfg.lookup.provider = LookupProvider::Searxng;
        let never = FakeHttp(Err("never asked".into()));
        // Off: nothing probed (the fake would say "never asked").
        let off = web_lookup(&cfg, &never, |_| None);
        assert!(off.ok && off.detail.starts_with("off"), "{off:?}");

        cfg.lookup.enabled = true;
        let ok = FakeHttp(Ok(r#"{"results": [{"title": "t", "url": "https://go.dev/", "content": "c"}], "answers": []}"#.into()));
        let ready = web_lookup(&cfg, &ok, |_| None);
        assert!(ready.ok && !ready.required, "no key needed: {ready:?}");
        assert!(
            ready
                .detail
                .contains("SearXNG at http://localhost:8888 (answered with 1 result(s))"),
            "{}",
            ready.detail
        );

        let forbidden = web_lookup(&cfg, &FakeHttp(Err("HTTP 403".into())), |_| None);
        assert!(
            !forbidden.ok && !forbidden.required,
            "a warning: {forbidden:?}"
        );
        assert!(
            forbidden.detail.contains("JSON format disabled (403)"),
            "{}",
            forbidden.detail
        );
        assert!(
            forbidden
                .fix
                .as_deref()
                .unwrap_or("")
                .contains("`json` to `search.formats`")
        );

        let down = web_lookup(
            &cfg,
            &FakeHttp(Err("cannot connect to 127.0.0.1:8888: refused".into())),
            |_| None,
        );
        assert!(!down.ok && !down.required, "{down:?}");
        assert!(
            down.detail
                .starts_with("SearXNG unreachable at http://localhost:8888")
        );

        let html = web_lookup(&cfg, &FakeHttp(Ok("<html>".into())), |_| None);
        assert!(
            !html.ok && html.detail.contains("not with search results"),
            "{html:?}"
        );

        cfg.lookup.base_url = "https://searx.example".into();
        let tls = web_lookup(&cfg, &never, |_| None);
        assert!(tls.ok && tls.detail.contains("not probed"), "{tls:?}");

        // The header names the instance's host, not ollama.com.
        cfg.lookup.base_url = "https://ollama.com".into();
        let res = FakeResolver::local();
        let line = privacy_line(&cfg, &endpoint_locality(&cfg, &res));
        assert!(
            line.contains("go to your SearXNG at localhost, which forwards it"),
            "{line}"
        );
    }

    /// An `Http` that answers by URL suffix; anything else is unreachable.
    struct RoutedHttp(Vec<(&'static str, Result<String, String>)>);

    impl Http for RoutedHttp {
        fn get(&self, url: &str) -> Result<String, String> {
            self.0
                .iter()
                .find(|(suffix, _)| url.ends_with(suffix))
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| Err("connection refused".into()))
        }
    }

    fn health(device: &str) -> String {
        format!(r#"{{"status":"healthy","model_loaded":true,"device":"{device}"}}"#)
    }

    /// #113: doctor names the model `t` translates with and whether Ollama has it, and the TTS
    /// server's device, with the way to the GPU when it is the CPU.
    /// #125: the echo guard is off by default and doctor says so; on, it names the wait, and
    /// with `[speak]` on that translations wait too. Informational either way.
    #[test]
    fn doctor_names_the_echo_guard_and_its_wait() {
        let mut cfg = Config::default();
        let off = echo_guard(&cfg);
        assert!(off.ok && !off.required, "{off:?}");
        assert!(off.detail.starts_with("off"), "{off:?}");
        cfg.audio.echo_guard = true;
        let on = echo_guard(&cfg);
        assert!(on.ok && !on.required, "{on:?}");
        assert!(on.detail.contains("up to 1.5 s"), "{on:?}");
        assert!(!on.detail.contains("[speak]"), "{on:?}");
        cfg.speak.enabled = true;
        assert!(echo_guard(&cfg).detail.contains("[speak] translations"));
    }

    #[test]
    fn doctor_names_the_translator_and_the_tts_device() {
        let mut cfg = Config::default();
        let off = speak(&cfg, &RoutedHttp(vec![]));
        assert_eq!(off.len(), 1);
        assert!(off[0].ok && !off[0].required, "{off:?}");

        cfg.speak.enabled = true;
        cfg.speak.target = "de".into();
        cfg.speak.model = Some("llama3.1:8b".into());
        let tags = r#"{"models":[{"name":"qwen3.6:35b-a3b"},{"name":"llama3.1:8b"}]}"#;
        let gpu = RoutedHttp(vec![
            ("/api/tags", Ok(tags.into())),
            ("/health", Ok(health("cuda"))),
        ]);
        let checks = speak(&cfg, &gpu);
        let report = Report {
            header: String::new(),
            checks: checks.clone(),
        }
        .render();
        assert!(checks.iter().all(|c| c.ok), "{report}");
        assert!(report.contains("translator"), "{report}");
        assert!(
            report.contains("llama3.1:8b present ([speak].model"),
            "{report}"
        );
        assert!(
            report.contains("qwen3.6:35b-a3b is not loaded while assistance is off"),
            "{report}"
        );
        assert!(
            report.contains("http://localhost:4123 healthy, device cuda"),
            "{report}"
        );

        let cpu = RoutedHttp(vec![
            (
                "/api/tags",
                Ok(r#"{"models":[{"name":"qwen3.6:35b-a3b"}]}"#.into()),
            ),
            ("/health", Ok(health("cpu"))),
        ]);
        let checks = speak(&cfg, &cpu);
        let report = Report {
            header: String::new(),
            checks: checks.clone(),
        }
        .render();
        assert!(report.contains("FAIL translator"), "{report}");
        assert!(report.contains("fix: ollama pull llama3.1:8b"), "{report}");
        assert!(report.contains("warn tts server"), "{report}");
        assert!(report.contains("device cpu"), "{report}");
        assert!(report.contains("docker-compose.gpu.yml"), "{report}");
        assert!(!checks[1].required, "a CPU server works: a warning");

        // Left out, the translator is [llm].model; a server that is down is a failure.
        cfg.speak.model = None;
        let checks = speak(&cfg, &RoutedHttp(vec![("/api/tags", Ok(tags.into()))]));
        assert!(
            checks[0].ok
                && checks[0]
                    .detail
                    .contains("qwen3.6:35b-a3b present ([llm].model")
        );
        assert!(!checks[1].ok && checks[1].required, "{:?}", checks[1]);
        assert!(
            checks[1].detail.contains("not answering"),
            "{:?}",
            checks[1]
        );
    }

    /// #122: with `engine = "qwen3"` doctor names the engine, its URL, the model vLLM-Omni
    /// serves and the device; another model there, or nothing, is a failure with the way to
    /// the sidecar.
    #[test]
    fn doctor_names_the_qwen3_engine_its_model_and_device() {
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.target = "zh".into();
        cfg.speak.engine = SpeakEngine::Qwen3;
        cfg.speak.tts_url = "http://localhost:4124".into();
        let tags = r#"{"models":[{"name":"qwen3.6:35b-a3b"}]}"#;
        let models = r#"{"object":"list","data":[{"id":"Qwen/Qwen3-TTS-12Hz-1.7B-Base","object":"model","owned_by":"vllm-omni"}]}"#;
        let up = RoutedHttp(vec![
            ("/api/tags", Ok(tags.into())),
            ("/v1/models", Ok(models.into())),
        ]);
        let checks = speak(&cfg, &up);
        assert!(checks[1].ok, "{:?}", checks[1]);
        assert_eq!(
            checks[1].detail,
            "engine qwen3: http://localhost:4124 serves Qwen/Qwen3-TTS-12Hz-1.7B-Base on vLLM-Omni, device cuda"
        );

        let other = RoutedHttp(vec![
            ("/api/tags", Ok(tags.into())),
            (
                "/v1/models",
                Ok(r#"{"data":[{"id":"FunAudioLLM/Fun-CosyVoice3-0.5B-2512"}]}"#.into()),
            ),
        ]);
        let checks = speak(&cfg, &other);
        assert!(!checks[1].ok && checks[1].required, "{:?}", checks[1]);
        assert!(
            checks[1].detail.contains("Fun-CosyVoice3"),
            "{:?}",
            checks[1]
        );

        let down = speak(&cfg, &RoutedHttp(vec![("/api/tags", Ok(tags.into()))]));
        assert!(!down[1].ok, "{:?}", down[1]);
        assert!(
            down[1]
                .detail
                .starts_with("engine qwen3: http://localhost:4124 is not answering"),
            "{:?}",
            down[1]
        );
        assert!(
            down[1]
                .fix
                .as_deref()
                .unwrap_or("")
                .contains("sidecars/qwen3-tts"),
            "{:?}",
            down[1]
        );
    }

    #[test]
    fn gpu_check_is_informational() {
        let c = gpu_backend(&FakeRunner::default().on_path(&["nvidia-smi"]));
        assert!(c.ok && !c.required);
        assert!(c.detail.contains("nvidia-smi present"));
        assert!(c.detail.contains("vulkaninfo missing"));
        assert!(c.detail.contains("without cuda/vulkan"));
    }

    #[test]
    #[cfg(not(any(feature = "cuda", feature = "vulkan")))]
    fn whisper_device_says_where_whisper_runs_and_flags_a_gpu_it_cannot_have() {
        use dmn_core::config::SttDevice;
        let cpu = whisper_device(SttDevice::Cpu, &[]);
        assert!(cpu.ok && !cpu.required, "{cpu:?}");
        assert!(
            cpu.detail.contains("device = cpu → whisper on CPU"),
            "{}",
            cpu.detail
        );

        let gpu = whisper_device(SttDevice::Gpu, &[]);
        assert!(
            !gpu.ok && !gpu.required,
            "a GPU it cannot have is a warning, not a failure"
        );
        assert!(
            gpu.detail.contains("without a GPU backend"),
            "{}",
            gpu.detail
        );
        assert!(
            gpu.fix
                .as_deref()
                .unwrap_or("")
                .contains("--features vulkan")
        );

        let auto = whisper_device(SttDevice::Auto, &[]);
        assert!(auto.ok, "auto on a CPU build is simply the CPU: {auto:?}");
    }

    #[test]
    fn host_is_parsed_from_every_url_shape() {
        assert_eq!(host_of("http://localhost:11434"), "localhost");
        assert_eq!(host_of("http://127.0.0.1:11434/api/tags"), "127.0.0.1");
        assert_eq!(host_of("http://[::1]:11434"), "::1");
        assert_eq!(host_of("https://api.openai.com"), "api.openai.com");
        assert_eq!(host_of("10.0.0.5:11434"), "10.0.0.5");
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn locality_follows_the_resolved_host_not_the_provider() {
        let res = FakeResolver::local().with("ollama.lan", &["10.0.0.5"]);
        let local = |url: &str| {
            let mut cfg = Config::default();
            cfg.llm.base_url = url.into();
            endpoint_locality(&cfg, &res)
        };
        // provider = ollama, but the endpoint is somewhere else: text leaves the machine.
        assert_eq!(local("http://localhost:11434"), Locality::Local);
        assert_eq!(local("http://127.0.0.1:11434"), Locality::Local);
        assert_eq!(local("http://[::1]:11434"), Locality::Local);
        assert_eq!(local("http://10.0.0.5:11434"), Locality::Remote);
        assert_eq!(local("http://ollama.lan:11434"), Locality::Remote);
        assert_eq!(local("http://nowhere.invalid:11434"), Locality::Unresolved);

        // A remote provider is remote whatever the URL says.
        let mut remote = Config::default();
        remote.llm.provider = Provider::Anthropic;
        remote.llm.base_url = "http://localhost:11434".into();
        assert_eq!(endpoint_locality(&remote, &res), Locality::Remote);
    }

    #[test]
    fn privacy_line_names_the_host_when_text_leaves() {
        let mut cfg = Config::default();
        cfg.llm.base_url = "http://10.0.0.5:11434".into();
        let line = privacy_line(&cfg, &Locality::Remote);
        assert!(line.contains("LEAVE this machine"), "{line}");
        assert!(line.contains("10.0.0.5"), "{line}");
        assert_eq!(
            privacy_line(&cfg, &Locality::Local),
            "nothing leaves this machine"
        );
        let unknown = privacy_line(&cfg, &Locality::Unresolved);
        assert!(unknown.contains("cannot resolve 10.0.0.5"), "{unknown}");
        assert!(unknown.contains("LEAVE this machine"), "{unknown}");
    }

    #[test]
    fn ollama_on_another_host_says_text_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.llm.base_url = "http://10.0.0.5:11434".into();
        let report = run_all(
            &cfg,
            &Paths::under(dir.path()),
            &FakeRunner::default(),
            &FakeHttp(Ok(TAGS.into())),
            &FakeResolver::local(),
        );
        // Printed with --nocapture; this is the line a user sees for a remote Ollama.
        println!("{}", report.header.trim_end());
        assert!(
            report.header.contains("LEAVE this machine"),
            "{}",
            report.header
        );
        // Still an ollama provider, so the ollama checks still run.
        assert!(report.checks.iter().any(|c| c.name == "ollama"));
    }

    #[test]
    fn report_exit_status_counts_only_required_failures() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let cfg = Config::default();
        let runner = FakeRunner::default()
            .with("pkg-config", &["--modversion", "libpipewire-0.3"], "1.6.8")
            .with("pw-cli", &["info", "0"], "version: \"1.5.85\"")
            .on_path(&["pw-record", "pw-play"]);
        let report = run_all(
            &cfg,
            &paths,
            &runner,
            &FakeHttp(Ok(TAGS.into())),
            &FakeResolver::local(),
        );
        // The two models are the only required failures (Silero is the default VAD since #63,
        // so its model is required too); tools are a warning; gpu is info.
        assert_eq!(report.failed_required(), 2);
        let text = report.render();
        assert!(text.contains("provider: ollama, model qwen3.6:35b-a3b"));
        assert!(text.contains("nothing leaves this machine"));
        assert!(text.contains("FAIL whisper model"));
        assert!(text.contains("FAIL silero model"));
        assert!(text.contains("warn pipewire tools"));
        assert!(text.contains("info gpu backend"));
        assert!(
            text.trim_end().ends_with("2 of 6 required checks failed"),
            "{text}"
        );
    }

    #[test]
    fn remote_provider_header_says_text_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.llm.provider = Provider::OpenAI;
        cfg.llm.base_url = "https://api.openai.com".into();
        let report = run_all(
            &cfg,
            &Paths::under(dir.path()),
            &FakeRunner::default(),
            &FakeHttp(Err("unused".into())),
            &FakeResolver::local(),
        );
        assert!(report.header.contains("LEAVE this machine"));
        assert!(report.checks.iter().any(|c| c.name == "api key"));
        assert!(!report.checks.iter().any(|c| c.name == "ollama"));
    }

    #[test]
    fn http_response_parsing_handles_content_length_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_http_response(plain).unwrap(), "{}");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n2\r\n\"}\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(chunked).unwrap(), "{\"a\"}");
        assert_eq!(
            parse_http_response(b"HTTP/1.1 404 Not Found\r\n\r\n").unwrap_err(),
            "HTTP 404"
        );
        assert!(parse_http_response(b"garbage").is_err());
    }

    #[test]
    fn transfer_encoding_value_is_case_insensitive() {
        // Copilot on PR #18: only the header name was lowercased, so "Chunked" was not decoded
        // and the raw chunk framing reached the JSON parser.
        for header in [
            "Transfer-Encoding: Chunked",
            "transfer-encoding: chunked",
            "Transfer-Encoding: CHUNKED",
        ] {
            let raw = format!("HTTP/1.1 200 OK\r\n{header}\r\n\r\n2\r\n{{}}\r\n0\r\n\r\n");
            assert_eq!(
                parse_http_response(raw.as_bytes()).unwrap(),
                "{}",
                "{header}"
            );
        }
    }

    #[test]
    fn a_2xx_that_is_not_api_tags_is_not_a_missing_model() {
        // Copilot on PR #18: unparsable JSON became an empty model list, so a proxy or the
        // wrong port was misdiagnosed as "model not pulled" with an `ollama pull` fix.
        assert_eq!(ollama_model_names(r#"{"models":[]}"#), Some(vec![]));
        assert_eq!(ollama_model_names("<html>hello</html>"), None);
        assert_eq!(ollama_model_names(r#"{"error":"nope"}"#), None);
        // A models array whose entries have no string name is malformed, not empty.
        assert_eq!(ollama_model_names(r#"{"models":[{}]}"#), None);
        assert_eq!(ollama_model_names(r#"{"models":[{"name":7}]}"#), None);

        let checks = ollama(
            &FakeHttp(Ok("<html>proxy</html>".into())),
            "http://localhost:11434",
            "qwen3.6:35b-a3b",
        );
        assert_eq!(checks.len(), 1, "no model check on an unusable response");
        assert!(!checks[0].ok);
        assert!(
            checks[0].detail.contains("unexpected response"),
            "{}",
            checks[0].detail
        );
        let fix = checks[0].fix.as_deref().unwrap_or_default();
        assert!(!fix.contains("ollama pull"), "{fix}");

        // An Ollama with nothing pulled is still an Ollama: that one does say `ollama pull`.
        let empty = ollama(
            &FakeHttp(Ok(r#"{"models":[]}"#.into())),
            "http://localhost:11434",
            "qwen3.6:35b-a3b",
        );
        assert_eq!(empty.len(), 2);
        assert!(empty[0].ok, "reachable");
        assert!(
            empty[1]
                .fix
                .as_deref()
                .unwrap_or_default()
                .contains("ollama pull")
        );
    }

    #[test]
    fn chunked_body_split_inside_multibyte_char_decodes_intact() {
        // "🎤" is 4 bytes; the first chunk ends after its second byte. The old str-based
        // decoder panicked slicing at a non-char boundary (review finding on PR #18).
        let mic = "🎤".as_bytes();
        let json_tail = r#"","name":"café"}"#.as_bytes();
        let mut chunk1 = br#"{"model":""#.to_vec();
        chunk1.extend_from_slice(&mic[..2]);
        let mut chunk2 = mic[2..].to_vec();
        chunk2.extend_from_slice(json_tail);
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for chunk in [&chunk1, &chunk2] {
            raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            raw.extend_from_slice(chunk);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        let body = parse_http_response(&raw).unwrap();
        assert_eq!(body, r#"{"model":"🎤","name":"café"}"#);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "🎤");
        assert_eq!(v["name"], "café");
    }

    /// Needs a live Ollama; run with `cargo test -p dmn-cli -- --ignored` on the dev machine.
    #[test]
    #[ignore]
    fn live_ollama_tags() {
        let body = StdHttp.get("http://localhost:11434/api/tags").unwrap();
        assert!(
            !ollama_model_names(&body)
                .expect("an /api/tags body")
                .is_empty()
        );
    }
}
