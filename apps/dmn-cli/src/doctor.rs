//! `dmn doctor`: one line per check, a fix for every failure, exit 1 if a required check fails.
//!
//! Every check is a pure function over injected inputs ([`Runner`] for commands and PATH,
//! [`Http`] for the Ollama endpoint) so the unit tests run without PipeWire or Ollama, which
//! CI does not have. `--probe` (play a tone and capture it back, issue #5) lives in `probe.rs`.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use dmn_core::config::Provider;
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
    let chunked = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.contains("chunked"));
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

pub fn whisper_model(models_dir: &Path, model: &str) -> Check {
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
    let file = dmn_stt::model_path(models_dir, m);
    match std::fs::metadata(&file) {
        Ok(meta) if meta.len() == m.size_bytes() => {
            Check::pass("whisper model", file.display().to_string())
        }
        Ok(meta) => Check::fail(
            "whisper model",
            format!(
                "{} is {} bytes, expected {} (partial download?)",
                file.display(),
                meta.len(),
                m.size_bytes()
            ),
            format!("rm {} && dmn doctor --fetch {model}", file.display()),
        ),
        Err(_) => Check::fail(
            "whisper model",
            format!("{} missing", file.display()),
            format!("dmn doctor --fetch {model}"),
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
    let names = ollama_model_names(&body);
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

fn ollama_model_names(body: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
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

pub fn run_all(cfg: &Config, paths: &Paths, r: &dyn Runner, h: &dyn Http) -> Report {
    let mut header = String::new();
    let _ = writeln!(header, "config:   {}", paths.config_file.display());
    let _ = writeln!(header, "log:      {}", paths.log_file.display());
    let provider = format!("{:?}", cfg.llm.provider).to_lowercase();
    let privacy = if cfg.llm.provider.is_remote() {
        "transcript text, retrieved chunks and the session summary LEAVE this machine"
    } else {
        "nothing leaves this machine"
    };
    let _ = writeln!(
        header,
        "provider: {provider}, model {} at {} — {privacy}",
        cfg.llm.model, cfg.llm.base_url
    );

    let mut checks = vec![
        libpipewire_headers(r),
        pipewire_server(r),
        pipewire_tools(r),
        whisper_model(&paths.models_dir, &cfg.stt.model),
    ];
    match cfg.llm.provider {
        Provider::Ollama => checks.extend(ollama(h, &cfg.llm.base_url, &cfg.llm.model)),
        Provider::OpenAI | Provider::Anthropic => {
            checks.push(remote_api_key(cfg, |name| std::env::var(name).ok()));
        }
    }
    checks.push(gpu_backend(r));
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
    fn model_file_check_names_the_fetch_command() {
        let dir = tempfile::tempdir().unwrap();
        let missing = whisper_model(dir.path(), "small.en");
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
        let partial = whisper_model(dir.path(), "small.en");
        assert!(
            !partial.ok && partial.detail.contains("partial download"),
            "{}",
            partial.detail
        );
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(dmn_stt::Model::SmallEn.size_bytes())
            .unwrap();
        assert!(whisper_model(dir.path(), "small.en").ok);
        assert!(!whisper_model(dir.path(), "small").ok, "unknown names fail");
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
    fn gpu_check_is_informational() {
        let c = gpu_backend(&FakeRunner::default().on_path(&["nvidia-smi"]));
        assert!(c.ok && !c.required);
        assert!(c.detail.contains("nvidia-smi present"));
        assert!(c.detail.contains("vulkaninfo missing"));
        assert!(c.detail.contains("without cuda/vulkan"));
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
        let report = run_all(&cfg, &paths, &runner, &FakeHttp(Ok(TAGS.into())));
        // whisper model is the only required failure; tools are a warning; gpu is info.
        assert_eq!(report.failed_required(), 1);
        let text = report.render();
        assert!(text.contains("provider: ollama, model qwen3.6:35b-a3b"));
        assert!(text.contains("nothing leaves this machine"));
        assert!(text.contains("FAIL whisper model"));
        assert!(text.contains("warn pipewire tools"));
        assert!(text.contains("info gpu backend"));
        assert!(
            text.trim_end().ends_with("1 of 5 required checks failed"),
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
        assert!(!ollama_model_names(&body).is_empty());
    }
}
