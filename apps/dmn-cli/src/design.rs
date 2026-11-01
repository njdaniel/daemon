//! `dmn speak design` (#130): make a voice from a description. Qwen's pattern is design, then
//! clone: the VoiceDesign checkpoint renders one sample line in the described voice, the user
//! hears it, and the sample is enrolled into the Base server the call already uses, where the
//! `o` picker offers it. The Base server and the user's own voice are untouched.
//!
//! The VoiceDesign server is the `design` profile of `sidecars/qwen3-tts/compose.yaml` (the
//! file is compiled in, so an installed `dmn` needs no checkout). It runs only while a voice
//! is made: this command starts it, waits for `/health`, and stops it again on every way out,
//! when it was the one that started it. Ctrl+C (and a closed terminal) is one of those ways out:
//! [`watch_signals`] stops the server and deletes the sample not yet kept before the process
//! ends, since a destructor does not run on a signal. A server left running anyway (`kill -9`)
//! is found by its container name at the next run, said, and stopped at that run's end. Before
//! starting it, the GPU's free memory is checked: with too little, the command stops and names
//! the processes holding the GPU (a forgotten Chatterbox container, say). It never stops
//! anything of the user's: a server at `design_url` that is not dmn's container is left alone.
//!
//! Audio on disk: the sample is not the call. It is written where the command line names
//! (`--out`) or under the data dir as `<name>.wav`, the clip a re-enrollment needs, as
//! `dmn speak enroll` stores its clip; a sample not kept is deleted.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dmn_core::Paths;
use dmn_core::config::{Config, SpeakEngine};
use dmn_tts::{Consent, DesignRequest, Qwen3Designer, TtsProvider};

use crate::speak::{self, MIN_CLIP_SECS, SpeakError};

/// What the VoiceDesign server needs free on the GPU before it starts, in MiB: what it took on
/// the dev box's 4090 with compose.yaml's shares (0.19 + 0.07 of 24 GiB), 5206 + 1382 = 6588
/// MiB at its peak, measured 2026-10-19 (see compose.yaml).
pub const NEED_MIB: u64 = 6_600;
/// How long a start may take: the first downloads ~4.3 GB and compiles CUDA graphs.
pub const READY_WITHIN: Duration = Duration::from_secs(20 * 60);
/// How often `/health` is asked while the server starts.
const POLL: Duration = Duration::from_secs(2);
/// The Base model clones from at most this much speech.
const MAX_SAMPLE_SECS: f64 = 30.0;
/// The sample's default line: about ten seconds of English, enough to clone from.
pub const DEFAULT_LINE: &str = "Hello there, it's good to finally meet you. This is the voice I'll be using from now on, so take a good listen. Every word you hear next will sound just like this.";
/// The compose service and the file that defines it.
const SERVICE: &str = "qwen3-tts-design";
/// #130: the image both sidecar services run (compose.yaml pins it), for `dmn doctor`.
pub const IMAGE: &str = "vllm/vllm-omni:v0.30.0";
/// The Base service's container, which mounts the model cache `dmn doctor` looks in.
pub const BASE_CONTAINER: &str = "dmn-qwen3-tts";
/// The design service's container.
const DESIGN_CONTAINER: &str = "dmn-qwen3-tts-design";
const COMPOSE: &str = include_str!("../../../sidecars/qwen3-tts/compose.yaml");

/// `dmn speak design`'s arguments.
pub struct DesignArgs {
    pub name: String,
    pub describe: String,
    pub line: Option<String>,
    pub language: Option<String>,
    pub out: Option<PathBuf>,
    pub yes: bool,
    /// How long the server may take to come up ([`READY_WITHIN`]; tests use less).
    pub ready_within: Duration,
}

/// A process holding GPU memory, as `nvidia-smi` lists it, with its container when it is in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuProcess {
    pub pid: u32,
    pub name: String,
    pub mib: u64,
    pub container: Option<String>,
}

/// What the command needs from the machine, so tests run without a GPU, Docker or speakers.
pub trait DesignHost {
    /// The GPU's free memory in MiB. `Err` when there is no NVIDIA GPU to ask.
    fn free_vram_mib(&self) -> Result<u64, String>;
    /// What holds the GPU, largest first.
    fn gpu_processes(&self) -> Vec<GpuProcess>;
    /// Start the VoiceDesign service (`docker compose --profile design up -d`).
    fn start(&self) -> Result<(), String>;
    /// Stop it again.
    fn stop(&self) -> Result<(), String>;
    /// Whether dmn's own design container is running: a server left by an earlier run that
    /// was killed, as opposed to one the user runs at `design_url` themselves.
    fn ours_running(&self) -> bool;
    /// Why the service has stopped, when it has (its container exited while starting): the
    /// error it logged. `None` while it runs, or when that cannot be told.
    fn exited(&self) -> Option<String>;
    /// Play the WAV at `path` on the speakers (not into `dmn-voice`).
    fn play(&self, path: &Path) -> Result<(), String>;
    /// Ask a yes/no question; `true` for yes. No answer is no.
    fn ask(&self, question: &str) -> bool;
}

/// Stops the design server when the command ends, on every way out, if this command started
/// it.
struct Started<'a> {
    host: &'a dyn DesignHost,
    interrupt: &'a Interrupt,
    stopped: bool,
}

impl Started<'_> {
    fn stop(&mut self) -> Result<(), String> {
        self.stopped = true;
        self.interrupt.set_started(false);
        self.host.stop()
    }
}

impl Drop for Started<'_> {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        self.interrupt.set_started(false);
        if let Err(e) = self.host.stop() {
            tracing::warn!(error = %e, "cannot stop the VoiceDesign server");
            eprintln!("dmn: could not stop the VoiceDesign server: {e}");
        }
    }
}

/// What a signal must undo while `dmn speak design` runs: the server this command started (or
/// took over) and the sample not yet kept. [`design`] keeps it current; [`watch_signals`]
/// acts on it.
#[derive(Default)]
pub struct Interrupt {
    undo: Mutex<Undo>,
}

#[derive(Default)]
struct Undo {
    started: bool,
    pending: Option<PathBuf>,
}

impl Interrupt {
    fn with<R>(&self, f: impl FnOnce(&mut Undo) -> R) -> R {
        // A panic while the lock was held leaves plain data behind; keep using it.
        let mut undo = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut undo)
    }

    fn set_started(&self, started: bool) {
        self.with(|u| u.started = started);
    }

    fn set_pending(&self, pending: Option<PathBuf>) {
        self.with(|u| u.pending = pending);
    }

    /// Stop the server if this command started it and delete the sample not kept; says what
    /// was done, a line each. Each is done once, however often this is called.
    pub fn undo(&self, host: &dyn DesignHost) -> Vec<String> {
        let (started, pending) = self.with(|u| (std::mem::take(&mut u.started), u.pending.take()));
        let mut said = Vec::new();
        if let Some(path) = pending
            && std::fs::remove_file(&path).is_ok()
        {
            said.push(format!("deleted the sample not kept ({})", path.display()));
        }
        if started {
            said.push(match host.stop() {
                Ok(()) => "stopped the VoiceDesign server".to_owned(),
                Err(e) => format!(
                    "could not stop the VoiceDesign server ({e}); stop it with `docker stop {DESIGN_CONTAINER}`"
                ),
            });
        }
        said
    }
}

/// Catch Ctrl+C, SIGTERM and SIGHUP for the rest of the process: on the first, undo what
/// `interrupt` holds on the real machine and exit with 130. Registered before this returns, so
/// no signal can slip in between.
pub fn watch_signals(interrupt: Arc<Interrupt>) -> Result<(), SpeakError> {
    use tokio::signal::unix::{SignalKind, signal};
    let rt = speak::rt()?;
    let listen = |kind| {
        let _entered = rt.enter();
        signal(kind).map_err(|e| SpeakError::Runtime(format!("cannot catch signals: {e}")))
    };
    let mut int = listen(SignalKind::interrupt())?;
    let mut term = listen(SignalKind::terminate())?;
    let mut hup = listen(SignalKind::hangup())?;
    std::thread::Builder::new()
        .name("dmn-design-signals".into())
        .spawn(move || {
            rt.block_on(async {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {}
                    _ = hup.recv() => {}
                }
            });
            eprintln!("\ninterrupted");
            for line in interrupt.undo(&Docker) {
                eprintln!("{line}");
            }
            std::process::exit(130);
        })
        .map_err(|e| SpeakError::Runtime(format!("cannot start the signal thread: {e}")))?;
    Ok(())
}

/// The holders of the GPU, one line each, for [`SpeakError::LowVram`].
fn holders(processes: &[GpuProcess]) -> String {
    if processes.is_empty() {
        return "  (nvidia-smi lists no processes)".to_owned();
    }
    processes
        .iter()
        .map(|p| {
            let container = p
                .container
                .as_ref()
                .map(|c| format!(", container {c}"))
                .unwrap_or_default();
            format!("  {} MiB  pid {}  {}{container}", p.mib, p.pid, p.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Make the voice: see the module doc. Returns what to print.
pub fn design(
    cfg: &Config,
    paths: &Paths,
    args: &DesignArgs,
    host: &dyn DesignHost,
    base: &dyn TtsProvider,
    designer: &Qwen3Designer,
    interrupt: &Interrupt,
) -> Result<String, SpeakError> {
    let config = paths.config_file.display().to_string();
    speak::check_gates(cfg, paths)?;
    if cfg.speak.engine != SpeakEngine::Qwen3 {
        return Err(SpeakError::DesignNeedsQwen3 {
            engine: cfg.speak.engine.as_str(),
            config,
        });
    }
    if !cfg.speak.design_is_local() {
        return Err(SpeakError::NotLocal {
            url: cfg.speak.design_url.clone(),
            config,
        });
    }
    let name = args.name.trim();
    speak::check_name(name)?;
    if cfg.speak.own_voices().iter().any(|own| own == name) {
        return Err(SpeakError::DesignOwnName {
            name: name.to_owned(),
        });
    }
    let language = args.language.as_deref().map(str::trim);
    if let Some(code) = language
        && dmn_tts::qwen3_language(code).is_none()
    {
        return Err(SpeakError::BadLanguage {
            code: code.to_owned(),
        });
    }
    let rt = speak::rt()?;
    let mut out = String::new();

    // The server: already up (the user's own, left alone; or dmn's, left by a run that was
    // killed, stopped at the end), or started here and stopped at the end.
    let mut started = None;
    if rt.block_on(designer.healthy()) {
        if host.ours_running() {
            eprintln!(
                "the VoiceDesign server ({DESIGN_CONTAINER}) was still running from an earlier run; using it and stopping it when done"
            );
            interrupt.set_started(true);
            started = Some(Started {
                host,
                interrupt,
                stopped: false,
            });
        }
    } else {
        let free = host
            .free_vram_mib()
            .map_err(|detail| SpeakError::NoGpu { detail })?;
        if free < NEED_MIB {
            return Err(SpeakError::LowVram {
                free_mib: free,
                need_mib: NEED_MIB,
                holders: holders(&host.gpu_processes()),
            });
        }
        eprintln!(
            "starting the VoiceDesign server ({SERVICE} on {}; the first start downloads ~4.3 GB)…",
            cfg.speak.design_url
        );
        // Marked before the start, so a Ctrl+C during `compose up` still stops it.
        interrupt.set_started(true);
        started = Some(Started {
            host,
            interrupt,
            stopped: false,
        });
        host.start().map_err(SpeakError::Docker)?;
        let t = Instant::now();
        while !rt.block_on(designer.healthy()) {
            // A start that failed (too little memory for its KV cache, say) ends the container
            // within a minute: say why now, not after the whole wait.
            if let Some(why) = host.exited() {
                return Err(SpeakError::DesignExited { why });
            }
            if t.elapsed() >= args.ready_within {
                return Err(SpeakError::DesignNotReady {
                    url: cfg.speak.design_url.clone(),
                    secs: args.ready_within.as_secs(),
                });
            }
            std::thread::sleep(POLL);
        }
        tracing::info!(secs = t.elapsed().as_secs(), "VoiceDesign server ready");
    }

    // The sample.
    let req = DesignRequest {
        text: args.line.clone().unwrap_or_else(|| DEFAULT_LINE.to_owned()),
        description: args.describe.clone(),
        language: language.map(str::to_owned),
    };
    let t = Instant::now();
    let designed = rt
        .block_on(designer.design(&req))
        .map_err(|e| speak::tts_error(e, paths))?;
    if designed.secs < MIN_CLIP_SECS {
        return Err(SpeakError::DesignSampleLength {
            secs: designed.secs,
            more: "more",
        });
    }
    if designed.secs > MAX_SAMPLE_SECS {
        return Err(SpeakError::DesignSampleLength {
            secs: designed.secs,
            more: "less",
        });
    }
    out.push_str(&format!(
        "designed {:.1} s of {name:?} ({:.1} s): {:?}\n",
        designed.secs,
        t.elapsed().as_secs_f64(),
        args.describe.trim()
    ));

    // Heard (or written), then kept or not.
    let stored = paths.data_dir.join(format!("{name}.wav"));
    let pending = paths.data_dir.join(format!("{name}.wav.dmn-design"));
    let sample = args.out.clone().unwrap_or_else(|| pending.clone());
    let write = |path: &Path| -> Result<(), SpeakError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| SpeakError::WriteOut {
                path: dir.display().to_string(),
                detail: e.to_string(),
            })?;
        }
        std::fs::write(path, &designed.wav).map_err(|e| SpeakError::WriteOut {
            path: path.display().to_string(),
            detail: e.to_string(),
        })
    };
    if args.out.is_none() {
        interrupt.set_pending(Some(pending.clone()));
    }
    write(&sample)?;
    let cleanup = || {
        if args.out.is_none() {
            let _ = std::fs::remove_file(&pending);
            interrupt.set_pending(None);
        }
    };
    match &args.out {
        Some(path) => out.push_str(&format!("wrote the sample to {}\n", path.display())),
        None => {
            if let Err(e) = host.play(&sample) {
                tracing::warn!(error = %e, "cannot play the designed sample");
                out.push_str(&format!("could not play the sample ({e})\n"));
            }
        }
    }
    let keep = args.yes || host.ask(&format!("keep {name:?}? [y/N] "));
    if !keep {
        cleanup();
        if let Some(mut s) = started.take() {
            s.stop().map_err(SpeakError::Docker)?;
        }
        out.push_str("not kept: nothing enrolled\n");
        return Ok(out);
    }

    // Into the Base server, as any clip is, tagged as designed.
    let enrolled = rt
        .block_on(base.enroll_as(name, &sample, language, Consent::Designed))
        .map_err(|e| {
            cleanup();
            speak::tts_error(e, paths)
        })?;
    let stored_ok = speak::store_clip(&sample, &stored);
    cleanup();
    stored_ok?;
    if let Some(mut s) = started.take() {
        s.stop().map_err(SpeakError::Docker)?;
        out.push_str("stopped the VoiceDesign server\n");
    }
    tracing::info!(voice = %enrolled.name, clip = %stored.display(), "designed voice enrolled");
    out.push_str(&format!(
        "enrolled {:?}{} with the TTS server at {} (consent {})\nstored the sample as {}\npick it with `o` in dmn run, or set voice = {name:?} under [speak]\n",
        enrolled.name,
        if enrolled.replaced {
            " (replacing the voice of that name)"
        } else {
            ""
        },
        cfg.speak.tts_url,
        Consent::Designed.as_str(),
        stored.display()
    ));
    Ok(out)
}

/// The real machine: `nvidia-smi`, `docker compose` with the compiled-in compose file,
/// `pw-play`, and the terminal.
pub struct Docker;

impl Docker {
    /// `docker compose -f - --profile design <verb…>`, the compose file on stdin.
    fn compose(verb: &[&str]) -> Result<(), String> {
        let mut child = Command::new("docker")
            .args(["compose", "-f", "-", "--profile", "design"])
            .args(verb)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run docker compose: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(COMPOSE.as_bytes())
                .map_err(|e| format!("cannot pass the compose file to docker: {e}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|e| format!("docker compose: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        Err(format!(
            "docker compose {} failed: {}",
            verb.join(" "),
            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
        ))
    }
}

impl DesignHost for Docker {
    fn free_vram_mib(&self) -> Result<u64, String> {
        let out = Command::new("nvidia-smi")
            .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
            .output()
            .map_err(|e| format!("nvidia-smi: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "nvidia-smi: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        parse_free_mib(&String::from_utf8_lossy(&out.stdout))
            .ok_or_else(|| "nvidia-smi listed no GPU".to_owned())
    }

    fn gpu_processes(&self) -> Vec<GpuProcess> {
        let Ok(out) = Command::new("nvidia-smi")
            .args([
                "--query-compute-apps=pid,process_name,used_memory",
                "--format=csv,noheader,nounits",
            ])
            .output()
        else {
            return Vec::new();
        };
        let mut processes = parse_processes(&String::from_utf8_lossy(&out.stdout));
        let containers = Command::new("docker")
            .args(["ps", "--no-trunc", "--format", "{{.ID}}\t{{.Names}}"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        for p in &mut processes {
            let cgroup =
                std::fs::read_to_string(format!("/proc/{}/cgroup", p.pid)).unwrap_or_default();
            p.container = container_id(&cgroup).and_then(|id| {
                containers
                    .lines()
                    .filter_map(|l| l.split_once('\t'))
                    .find(|(full, _)| *full == id)
                    .map(|(_, name)| name.to_owned())
            });
        }
        processes
    }

    fn start(&self) -> Result<(), String> {
        Self::compose(&["up", "-d", SERVICE])
    }

    fn stop(&self) -> Result<(), String> {
        Self::compose(&["stop", SERVICE])
    }

    fn ours_running(&self) -> bool {
        Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{.State.Running}}",
                DESIGN_CONTAINER,
            ])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
    }

    fn exited(&self) -> Option<String> {
        let running = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{.State.Running}}",
                DESIGN_CONTAINER,
            ])
            .output()
            .ok()?;
        if String::from_utf8_lossy(&running.stdout).trim() != "false" {
            return None;
        }
        let logs = Command::new("docker")
            .args(["logs", "--tail", "400", DESIGN_CONTAINER])
            .output()
            .ok()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&logs.stdout),
            String::from_utf8_lossy(&logs.stderr)
        );
        Some(exit_reason(&text).unwrap_or_else(|| "it exited".to_owned()))
    }

    fn play(&self, path: &Path) -> Result<(), String> {
        let status = Command::new("pw-play")
            .arg(path)
            .status()
            .map_err(|e| format!("pw-play: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("pw-play exited with {status}"))
        }
    }

    fn ask(&self, question: &str) -> bool {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

/// What a vLLM-Omni server that exited while starting logged as the cause: its `ValueError`
/// (vLLM's own words, "No available memory for the cache blocks…"), else its last
/// `…Error: …` line, without colour codes or the process prefix.
fn exit_reason(logs: &str) -> Option<String> {
    let plain: Vec<String> = logs
        .lines()
        .map(|l| {
            let mut out = String::with_capacity(l.len());
            let mut chars = l.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    // An ANSI colour code: ESC [ digits ; ... m.
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        })
        .collect();
    // The error's text from the start of its word (`ValueError: …`), at most 200 characters.
    let error_in = |line: &str, marker: &str| {
        let at = line.find(marker)?;
        let word = line[..at].rfind([' ', ')', ']']).map_or(0, |i| i + 1);
        Some(
            line[word..]
                .chars()
                .take(200)
                .collect::<String>()
                .trim()
                .to_owned(),
        )
    };
    plain
        .iter()
        .rev()
        .find_map(|l| error_in(l, "ValueError: "))
        .or_else(|| plain.iter().rev().find_map(|l| error_in(l, "Error: ")))
}

/// The first GPU's free MiB from `nvidia-smi --query-gpu=memory.free`.
fn parse_free_mib(text: &str) -> Option<u64> {
    text.lines().find_map(|l| l.trim().parse().ok())
}

/// `pid, name, MiB` rows from `nvidia-smi --query-compute-apps`, largest first.
fn parse_processes(text: &str) -> Vec<GpuProcess> {
    let mut processes: Vec<GpuProcess> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let pid = fields.next()?.parse().ok()?;
            let name = fields.next()?.to_owned();
            let mib = fields.next()?.parse().ok()?;
            Some(GpuProcess {
                pid,
                name,
                mib,
                container: None,
            })
        })
        .collect();
    processes.sort_by_key(|p| std::cmp::Reverse(p.mib));
    processes
}

/// The Docker container id in a `/proc/<pid>/cgroup` (`…/docker-<id>.scope` under systemd,
/// `/docker/<id>` under cgroupfs).
fn container_id(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let path = line.rsplit(':').next()?;
        let last = path.rsplit('/').next()?;
        let id = last
            .strip_prefix("docker-")
            .and_then(|s| s.strip_suffix(".scope"))
            .or_else(|| path.contains("/docker/").then_some(last))?;
        (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_tts::testserver::{MockServer, Reply, serve};
    use std::sync::Mutex;

    /// A machine with a GPU of `free` MiB and the answer `keep` to every question.
    struct FakeHost {
        free: Result<u64, String>,
        processes: Vec<GpuProcess>,
        keep: bool,
        exited: Option<String>,
        ours: bool,
        /// A Ctrl+C at the question: `ask` undoes what this holds, as the signal thread would.
        ctrl_c_at_ask: Option<Arc<Interrupt>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeHost {
        fn new(free: u64, keep: bool) -> Self {
            Self {
                free: Ok(free),
                processes: Vec::new(),
                keep,
                exited: None,
                ours: false,
                ctrl_c_at_ask: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl DesignHost for FakeHost {
        fn free_vram_mib(&self) -> Result<u64, String> {
            self.free.clone()
        }
        fn gpu_processes(&self) -> Vec<GpuProcess> {
            self.processes.clone()
        }
        fn start(&self) -> Result<(), String> {
            self.log("start".into());
            Ok(())
        }
        fn stop(&self) -> Result<(), String> {
            self.log("stop".into());
            Ok(())
        }
        fn ours_running(&self) -> bool {
            self.ours
        }
        fn exited(&self) -> Option<String> {
            self.exited.clone()
        }
        fn play(&self, path: &Path) -> Result<(), String> {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            self.log(format!("play {} bytes", bytes.len()));
            Ok(())
        }
        fn ask(&self, question: &str) -> bool {
            self.log(format!("ask {question}"));
            if let Some(interrupt) = &self.ctrl_c_at_ask {
                let said = interrupt.undo(self);
                self.log(format!("undo: {}", said.join("; ")));
            }
            self.keep
        }
    }

    fn qwen3_cfg(base: &str, design: &str) -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.engine = SpeakEngine::Qwen3;
        cfg.speak.tts_url = base.to_owned();
        cfg.speak.design_url = design.to_owned();
        cfg.speak.voice_ref = "/d/voice.wav".into();
        (dir, paths, cfg)
    }

    fn args(yes: bool) -> DesignArgs {
        DesignArgs {
            name: "pirate".into(),
            describe: "grumpy old pirate, gravelly, slow".into(),
            line: None,
            language: Some("en".into()),
            out: None,
            yes,
            ready_within: Duration::from_secs(10),
        }
    }

    fn sample(secs: f32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        let mut w = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec).unwrap();
        for i in 0..(24_000.0 * secs) as usize {
            let v = (0.3 * (i as f32 * 0.03).sin() * f32::from(i16::MAX)) as i16;
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        buf
    }

    fn stopped() -> Reply {
        Reply::new("503 Service Unavailable", "text/plain", "starting")
    }

    /// The issue's criterion: against the test server, the design server is started, the
    /// sample rendered, played and asked about, enrolled into the Base server under the name
    /// with consent `dmn-designed`, stored, and the design server stopped.
    #[test]
    fn design_renders_asks_enrolls_with_the_designed_consent_and_stops_the_server() {
        let wav = sample(8.0);
        let design_server =
            serve(vec![stopped(), Reply::json("{}"), Reply::wav(wav.clone())]).unwrap();
        let base_server = serve(vec![
            Reply::json(r#"{"voices":[],"uploaded_voices":[{"name":"voice"}]}"#),
            Reply::json(r#"{"success":true,"voice":{"name":"pirate"}}"#),
        ])
        .unwrap();
        let (_dir, paths, cfg) = qwen3_cfg(&base_server.url, &design_server.url);
        let host = FakeHost::new(12_000, true);
        let base = dmn_tts::Qwen3Provider::new(&base_server.url);
        let designer = Qwen3Designer::new(&design_server.url);
        let out = design(
            &cfg,
            &paths,
            &args(false),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap();

        assert_eq!(
            host.calls(),
            [
                "start".to_owned(),
                format!("play {} bytes", wav.len()),
                "ask keep \"pirate\"? [y/N] ".to_owned(),
                "stop".to_owned(),
            ]
        );
        assert_eq!(
            design_server.request_lines(),
            ["GET /health", "GET /health", "POST /v1/audio/speech"]
        );
        let requests = design_server.requests();
        let (_, body) = MockServer::head_and_body(&requests[2]);
        let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(sent["task_type"], "VoiceDesign");
        assert_eq!(sent["instructions"], "grumpy old pirate, gravelly, slow");
        assert_eq!(sent["input"], DEFAULT_LINE);
        assert_eq!(sent["language"], "English");

        assert_eq!(
            base_server.request_lines(),
            ["GET /v1/audio/voices", "POST /v1/audio/voices"]
        );
        let requests = base_server.requests();
        let (_, body) = MockServer::head_and_body(&requests[1]);
        let text = String::from_utf8_lossy(body);
        assert!(text.contains("name=\"name\"\r\n\r\npirate\r\n"), "{text}");
        assert!(
            text.contains("name=\"consent\"\r\n\r\ndmn-designed\r\n"),
            "{text}"
        );
        assert!(
            body.windows(wav.len()).any(|w| w == wav),
            "the sample itself"
        );

        let stored = paths.data_dir.join("pirate.wav");
        assert_eq!(std::fs::read(&stored).unwrap(), wav);
        assert!(!paths.data_dir.join("pirate.wav.dmn-design").exists());
        assert!(!paths.config_file.exists(), "the config is not touched");
        assert!(out.contains("enrolled \"pirate\""), "{out}");
        assert!(out.contains("consent dmn-designed"), "{out}");
        assert!(out.contains("stopped the VoiceDesign server"), "{out}");
    }

    /// With too little free VRAM it refuses before starting anything and names what holds the
    /// GPU, containers by name.
    #[test]
    fn design_with_too_little_free_vram_refuses_and_names_the_processes() {
        let design_server = serve(vec![stopped()]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let mut host = FakeHost::new(3_000, true);
        host.processes = vec![
            GpuProcess {
                pid: 3391,
                name: "python".into(),
                mib: 4216,
                container: Some("chatterbox-tts-api-gpu".into()),
            },
            GpuProcess {
                pid: 99,
                name: "ollama".into(),
                mib: 15_000,
                container: None,
            },
        ];
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let err = design(
            &cfg,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SpeakError::LowVram { .. }), "{msg}");
        assert!(msg.contains("3000 MiB are free"), "{msg}");
        assert!(msg.contains("container chatterbox-tts-api-gpu"), "{msg}");
        assert!(msg.contains("pid 99  ollama"), "{msg}");
        assert!(msg.contains("dmn stops nothing itself"), "{msg}");
        assert!(host.calls().is_empty(), "nothing started, nothing stopped");
        assert!(base.enrollments().is_empty());
    }

    /// A sample not kept is deleted and nothing is enrolled; the server still stops. A server
    /// already up is used and left running.
    #[test]
    fn design_not_kept_enrolls_nothing_and_a_running_server_is_left_alone() {
        let design_server =
            serve(vec![stopped(), Reply::json("{}"), Reply::wav(sample(7.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let host = FakeHost::new(12_000, false);
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let out = design(
            &cfg,
            &paths,
            &args(false),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap();
        assert!(out.contains("not kept"), "{out}");
        assert!(base.enrollments().is_empty());
        assert_eq!(host.calls().last().map(String::as_str), Some("stop"));
        assert!(!paths.data_dir.join("pirate.wav").exists());
        assert!(!paths.data_dir.join("pirate.wav.dmn-design").exists());

        // Already running: no VRAM check, no start, no stop; `--yes` asks nothing.
        let design_server = serve(vec![Reply::json("{}"), Reply::wav(sample(7.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let mut host = FakeHost::new(0, false);
        host.free = Err("no GPU".into());
        let designer = Qwen3Designer::new(&design_server.url);
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let out = design(
            &cfg,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap();
        assert!(!out.contains("stopped"), "{out}");
        assert_eq!(
            host.calls().len(),
            1,
            "only the playback: {:?}",
            host.calls()
        );
        assert_eq!(base.enrollments()[0].consent, "dmn-designed");
        assert_eq!(base.enrollments()[0].name, "pirate");
    }

    /// The review's blocking finding on PR #131: Ctrl+C at `keep? [y/N]` must not leave the
    /// server (6.6 GB of VRAM) running or the sample behind. What the signal thread undoes
    /// there: the sample is deleted and the server stopped, each once.
    #[test]
    fn ctrl_c_at_the_question_stops_the_server_and_deletes_the_sample() {
        let design_server =
            serve(vec![stopped(), Reply::json("{}"), Reply::wav(sample(8.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let interrupt = Arc::new(Interrupt::default());
        let mut host = FakeHost::new(12_000, false);
        host.ctrl_c_at_ask = Some(Arc::clone(&interrupt));
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        design(
            &cfg,
            &paths,
            &args(false),
            &host,
            &base,
            &designer,
            &interrupt,
        )
        .unwrap();
        let pending = paths.data_dir.join("pirate.wav.dmn-design");
        let calls = host.calls();
        let at = calls.iter().position(|c| c.starts_with("undo:")).unwrap();
        assert_eq!(
            calls[at],
            format!(
                "undo: deleted the sample not kept ({}); stopped the VoiceDesign server",
                pending.display()
            )
        );
        assert_eq!(
            calls[at - 1],
            "stop",
            "stopped before the process would exit"
        );
        assert!(!pending.exists());
        assert!(base.enrollments().is_empty());
        // Nothing is left for a second signal.
        assert!(interrupt.undo(&host).is_empty());
    }

    /// Before the server is up and after it is stopped there is nothing to undo; while it
    /// starts, a signal stops it.
    #[test]
    fn a_signal_stops_the_server_only_while_this_command_holds_it() {
        let host = FakeHost::new(12_000, true);
        let interrupt = Interrupt::default();
        assert!(interrupt.undo(&host).is_empty());
        interrupt.set_started(true);
        assert_eq!(interrupt.undo(&host), ["stopped the VoiceDesign server"]);
        assert!(interrupt.undo(&host).is_empty(), "once");
        assert_eq!(host.calls(), ["stop"]);

        // A whole run leaves nothing behind for a late signal.
        let design_server =
            serve(vec![stopped(), Reply::json("{}"), Reply::wav(sample(8.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let host = FakeHost::new(12_000, true);
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let interrupt = Interrupt::default();
        design(
            &cfg,
            &paths,
            &args(false),
            &host,
            &base,
            &designer,
            &interrupt,
        )
        .unwrap();
        assert!(interrupt.undo(&host).is_empty());
        assert_eq!(host.calls().iter().filter(|c| *c == "stop").count(), 1);
    }

    /// dmn's own design container still running (an earlier run killed with `kill -9`) is
    /// taken over: used without a start, and stopped at the end.
    #[test]
    fn a_design_server_left_by_a_killed_run_is_used_and_stopped() {
        let design_server = serve(vec![Reply::json("{}"), Reply::wav(sample(7.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let mut host = FakeHost::new(0, true);
        host.ours = true;
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let out = design(
            &cfg,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap();
        assert!(out.contains("stopped the VoiceDesign server"), "{out}");
        assert_eq!(
            host.calls(),
            [
                format!("play {} bytes", sample(7.0).len()),
                "stop".to_owned()
            ]
        );
        assert_eq!(base.enrollments()[0].name, "pirate");
    }

    /// `--out` writes the sample there instead of playing it; a too-short sample is refused
    /// (and the server stopped); a name that is the user's own, Chatterbox, and a design
    /// server off this machine are refused before anything starts.
    #[test]
    fn design_out_writes_the_sample_and_the_refusals_come_first() {
        let design_server = serve(vec![Reply::json("{}"), Reply::wav(sample(8.0))]).unwrap();
        let (dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let host = FakeHost::new(12_000, true);
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let mut a = args(true);
        a.out = Some(dir.path().join("sample.wav"));
        let out = design(
            &cfg,
            &paths,
            &a,
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap();
        assert!(out.contains("wrote the sample to"), "{out}");
        assert!(dir.path().join("sample.wav").is_file());
        assert!(paths.data_dir.join("pirate.wav").is_file());
        assert!(
            host.calls().is_empty(),
            "nothing played: {:?}",
            host.calls()
        );

        let design_server =
            serve(vec![stopped(), Reply::json("{}"), Reply::wav(sample(2.0))]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let host = FakeHost::new(12_000, true);
        let designer = Qwen3Designer::new(&design_server.url);
        let err = design(
            &cfg,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, SpeakError::DesignSampleLength { .. }),
            "{err}"
        );
        assert_eq!(host.calls(), ["start", "stop"]);

        let host = FakeHost::new(12_000, true);
        let designer = Qwen3Designer::new("http://localhost:1");
        let mut own = args(true);
        own.name = "voice".into();
        let err = design(
            &cfg,
            &paths,
            &own,
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SpeakError::DesignOwnName { .. }), "{err}");
        let mut chatterbox = cfg.clone();
        chatterbox.speak.engine = SpeakEngine::Chatterbox;
        let err = design(
            &chatterbox,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SpeakError::DesignNeedsQwen3 { .. }), "{err}");
        let mut far = cfg.clone();
        far.speak.design_url = "http://gpu.lan:4125".into();
        let err = design(
            &far,
            &paths,
            &args(true),
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SpeakError::NotLocal { .. }), "{err}");
        assert!(host.calls().is_empty());
    }

    /// A design server whose container exits while starting is reported at once, with vLLM's
    /// own words for why, not after the whole wait; the stop still runs.
    #[test]
    fn design_says_why_the_server_exited_while_starting() {
        let design_server = serve(vec![stopped(), stopped()]).unwrap();
        let (_dir, paths, cfg) = qwen3_cfg("http://localhost:4124", &design_server.url);
        let mut host = FakeHost::new(12_000, true);
        host.exited = Some("ValueError: No available memory for the cache blocks.".into());
        let base = dmn_tts::MockTtsProvider::new().cross_lingual();
        let designer = Qwen3Designer::new(&design_server.url);
        let mut a = args(true);
        a.ready_within = Duration::from_secs(600);
        let t = Instant::now();
        let err = design(
            &cfg,
            &paths,
            &a,
            &host,
            &base,
            &designer,
            &Interrupt::default(),
        )
        .unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(matches!(err, SpeakError::DesignExited { .. }), "{err}");
        assert!(err.to_string().contains("No available memory"), "{err}");
        assert_eq!(host.calls(), ["start", "stop"]);

        let logs = "\u{1b}[0;36m(StageEngineCoreProc_stage0_replica0 pid=188)\u{1b}[0;0m \u{1b}[31mERROR\u{1b}[0m [x.py:258] ValueError: No available memory for the cache blocks. Try increasing `gpu_memory_utilization`\n(APIServer pid=1) RuntimeError: Orchestrator initialization failed: Engine core initialization failed.\n";
        assert_eq!(
            exit_reason(logs).as_deref(),
            Some(
                "ValueError: No available memory for the cache blocks. Try increasing `gpu_memory_utilization`"
            )
        );
        assert_eq!(
            exit_reason("boot\n(APIServer pid=1) RuntimeError: it broke\n").as_deref(),
            Some("RuntimeError: it broke")
        );
        assert_eq!(exit_reason("all fine\n"), None);
    }

    #[test]
    fn design_parses_nvidia_smi_and_finds_a_processs_container() {
        assert_eq!(parse_free_mib("6843\n"), Some(6843));
        assert_eq!(parse_free_mib("garbage\n"), None);
        let ps = parse_processes(
            "3391, python, 4216\n176245, VLLM::StageEngineCoreProc_stage0_replica0_DP0, 5290\nbad line\n",
        );
        assert_eq!(
            ps.iter().map(|p| (p.pid, p.mib)).collect::<Vec<_>>(),
            [(176_245, 5290), (3391, 4216)]
        );
        let id = "a".repeat(64);
        assert_eq!(
            container_id(&format!("0::/system.slice/docker-{id}.scope\n")),
            Some(id.clone())
        );
        assert_eq!(container_id(&format!("12:memory:/docker/{id}\n")), Some(id));
        assert_eq!(container_id("0::/user.slice/user-1000.slice\n"), None);
        assert!(
            COMPOSE.contains(SERVICE),
            "the compiled-in compose file defines the service"
        );
        assert!(COMPOSE.contains(dmn_tts::QWEN3_DESIGN_MODEL));
        assert!(COMPOSE.contains(&format!("image: {IMAGE}")));
        assert!(COMPOSE.contains(&format!("container_name: {BASE_CONTAINER}\n")));
    }
}
