//! `t` in `dmn run` (#109): the user's last turn, translated into `[speak].target` by the
//! configured LLM, synthesized in their enrolled voice by the TTS server on this machine, and
//! played into the `dmn-voice` virtual microphone (#108) the call has selected.
//!
//! [`Speaker`] owns one worker thread with its own current-thread tokio runtime, so a press never
//! waits on the pipeline thread or the suggester: the translation is one request on the `[llm]`
//! provider (`think: false` included) with `[speak].model` as the model when it is set (#113: a
//! small translator, so the answer model need not be loaded for it), run whether interview
//! assistance is on or off. One press at a time: the pipeline sends a job only while
//! the session has none in flight, and the worker does the three steps in order, dispatching
//! [`Action::SpeakTranslated`] between them and [`Action::SpeakDone`] or
//! [`Action::SpeakFailed`] at the end. A failure is a status notice, never a panic.
//!
//! **Node lifetime.** The worker opens `dmn-voice` when it starts and keeps that one node for the
//! session, so the call can select it as its microphone before the first press and keeps it
//! selected between presses (#108: a call can only select a node that is there; one that came
//! and went with each press would drop out of the call's settings and fall back to the default
//! mic). If it cannot be opened (no PipeWire), the next press tries again and says so. A
//! playback nothing pulled is reported, and the node is closed and opened again, so speech
//! nobody heard is not left in its ring to play at the next press.
//!
//! **Warm-up (#113).** At start the worker loads the translator (as the suggester warms its
//! model) and, on a thread of its own so a first press never waits for it, asks the TTS server
//! for one word whose audio is thrown away: the first request after the server starts pays for
//! CUDA's warm-up (8.9 s measured, against 1.9 s warm).
//!
//! **Nothing linked (#113).** A call that selected `dmn-voice` loses it when dmn restarts: the
//! node is a new one, and PipeWire relinks only a stream that names its target by name in its
//! own properties (`pw-record --target dmn-voice` does), and only if no other stream came or went
//! meanwhile; Discord's, which asks not to be moved, and a stream moved by hand, which the
//! session manager keeps by serial, never. So a moment
//! after start, if nothing is linked to the node, the status line says [`NOT_LINKED`] for 5 s
//! (and the log says why); a press that nothing heard says the same.
//!
//! **Echo window.** When PipeWire first pulls a press's speech the worker sends
//! [`Window::From`], and when the last sample has played [`Window::Until`]; the pipeline turns
//! them into audio time for the echo guard, which then drops what either lane heard meanwhile.
//!
//! **Cancellation.** [`Speaker::stop`] (on `q`) cancels the translation and the wait for the TTS
//! server's first bytes, stops feeding at the next chunk, and joins the thread.
//!
//! **Pass-through (#114).** The node is opened with the session's microphone passing through
//! it (`[audio].mic`, the lane whisper hears), on from the start in pass-through mode and shut
//! in translate mode; [`Speaker::set_passthrough`] flips it on `l`, through the worker, which
//! also applies the current mode to a node it opens again after a failed playback. The gate's
//! fade and its shutting while speech plays are the node's own (`dmn_audio::Mixer`). The queue
//! of turns translate mode speaks is the pipeline's (`Speaking`): the worker still takes one
//! job at a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_audio::{AudioError, CaptureTarget, Passthrough, VMIC_NODE_NAME, VmicState};
use dmn_core::config::Config;
use dmn_core::{Paths, wire};
use dmn_llm::{CancellationToken, LlmError, Provider};
use dmn_session::{Action, SessionHub, SpeakLanguage, VoiceMode};
use dmn_tts::{PcmStream, SpeakRequest, TtsError, TtsProvider, Tuning};

use crate::speak::{self, DRAIN_MARGIN, SpeakError, SpeechSink};

/// Opens the sink the speech plays into: `dmn-voice` for a session, a test's sink in tests.
/// `Err` is the reason, for the status line.
pub type OpenSink = Box<dyn FnMut() -> Result<Box<dyn SpeechSink + Send>, String> + Send>;

/// How often the worker looks at the playback while it drains, and at the stop flag.
const DRAIN_TICK: Duration = Duration::from_millis(50);
/// #113: how long after opening `dmn-voice` the worker looks whether anything is linked to it.
/// The session manager relinks a stream that names it within a few hundred milliseconds.
pub const LINK_GRACE: Duration = Duration::from_secs(2);
/// #113: what the status line says when nothing is linked to `dmn-voice`, at start or after a
/// press nobody heard. Discord is the call app that loses it across a restart.
pub const NOT_LINKED: &str = "Discord: re-select dmn-voice as the microphone";
/// #113: the start-up notice's id: no press has it (presses count from 1).
pub const STARTUP_NOTICE: u64 = 0;
/// #113: what the TTS warm-up says; its audio is thrown away.
pub const WARM_UP_TEXT: &str = "OK.";

/// Everything a session's speaker needs, built and checked by [`setup`] before the run starts.
pub struct SpeakSetup {
    /// The translator: `[llm]` with `[speak].model` as the model (#113).
    pub provider: Arc<dyn Provider>,
    /// Its model's name, for the log.
    pub model: String,
    pub tts: Arc<dyn TtsProvider>,
    pub open_sink: OpenSink,
    /// The language spoken at start: `[speak].target` with its voice.
    pub language: Language,
    /// #115: every language the run can speak (`[speak].languages`, checked against the
    /// server's library), in the config's order; the picker lists them.
    pub languages: Vec<Language>,
    /// The translation's budget: the fast phase's.
    pub timeout: Duration,
    /// #113: `[speak]`'s TTS knobs, sent with every speech request.
    pub tuning: Tuning,
    /// #113: warm the TTS server up at start (off in tests, whose mock would record it).
    pub warm_tts: bool,
    /// #114: whether the microphone passes through the node from the start (`[speak].mode`).
    pub passthrough: bool,
}

/// One language a press can speak: the code the TTS request and the check carry (`es`), the
/// name the model reads ("Spanish (es)"), and the voice enrolled for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Language {
    pub code: String,
    pub name: String,
    pub voice: String,
}

impl Language {
    /// `code` with the name whisper has for it (the code itself for one it does not know).
    pub fn new(code: &str, voice: &str) -> Self {
        Self {
            code: code.to_owned(),
            name: target_name(code).unwrap_or_else(|| code.to_owned()),
            voice: voice.to_owned(),
        }
    }

    /// The session's view of it (#115).
    pub fn listed(&self) -> SpeakLanguage {
        SpeakLanguage {
            code: self.code.clone(),
            voice: self.voice.clone(),
        }
    }
}

/// When a press's speech played, on the wall clock, for the echo guard's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// PipeWire pulled the first sample.
    From(Instant),
    /// The last sample has played (or the playback stopped).
    Until(Instant),
}

struct Job {
    id: u64,
    text: String,
    pressed: Instant,
}

/// What the pipeline sends the worker.
enum Msg {
    Speak(Job),
    /// #114: the microphone's gate, open (pass-through mode) or shut (translate mode).
    Passthrough(bool),
    /// #115: the language (and voice) every job from here on speaks; one in flight is done
    /// before this is read, so it finishes in the language it started with.
    Language(Language),
}

/// The worker that speaks `t`'s presses. See the module doc.
pub struct Speaker {
    msgs: Option<crossbeam_channel::Sender<Msg>>,
    windows: crossbeam_channel::Receiver<Window>,
    stop: Arc<AtomicBool>,
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl Speaker {
    /// Start the worker: it opens the sink at once (so `dmn-voice` is there for the call to
    /// select) and loads the model, then waits for presses.
    pub fn start(setup: SpeakSetup, hub: Arc<SessionHub>) -> Result<Self, SpeakError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SpeakError::Runtime(e.to_string()))?;
        // One job is in flight at most: the pipeline sends the next only when it is done. The
        // mode switch (#114) rides the same channel so it is applied in order with the jobs.
        let (msgs, msgs_rx) = crossbeam_channel::unbounded();
        let (windows_tx, windows) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let passthrough = setup.passthrough;
        let worker = Worker {
            setup,
            hub,
            rt,
            sink: None,
            windows: windows_tx,
            stop: Arc::clone(&stop),
            cancel: cancel.clone(),
            passthrough,
        };
        let handle = std::thread::Builder::new()
            .name("dmn-speak".to_owned())
            .spawn(move || worker.run(&msgs_rx))
            .map_err(|e| SpeakError::Runtime(format!("cannot spawn the speak thread: {e}")))?;
        Ok(Self {
            msgs: Some(msgs),
            windows,
            stop,
            cancel,
            handle: Some(handle),
        })
    }

    /// Speak `text` (the user's turn) as press `id`. False when the worker cannot take it.
    pub fn speak(&self, id: u64, text: String) -> bool {
        self.msgs.as_ref().is_some_and(|msgs| {
            msgs.send(Msg::Speak(Job {
                id,
                text,
                pressed: Instant::now(),
            }))
            .is_ok()
        })
    }

    /// #114: let the microphone through `dmn-voice` (pass-through mode) or not (translate
    /// mode), from the node's next buffer; applied after a job in flight, during which the
    /// microphone is out anyway.
    pub fn set_passthrough(&self, on: bool) {
        if let Some(msgs) = &self.msgs {
            let _ = msgs.send(Msg::Passthrough(on));
        }
    }

    /// #115: speak `language` from the next job on. A job sent before this call finishes in
    /// its own language: the worker reads the channel in order, one job at a time.
    pub fn set_language(&self, language: Language) {
        if let Some(msgs) = &self.msgs {
            let _ = msgs.send(Msg::Language(language));
        }
    }

    /// The playback windows reported since the last call.
    pub fn windows(&self) -> Vec<Window> {
        self.windows.try_iter().collect()
    }

    /// Cancel what is in flight and join the worker.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.cancel.cancel();
        self.msgs = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        self.shut_down();
    }
}

struct Worker {
    setup: SpeakSetup,
    hub: Arc<SessionHub>,
    rt: tokio::runtime::Runtime,
    sink: Option<Box<dyn SpeechSink + Send>>,
    windows: crossbeam_channel::Sender<Window>,
    stop: Arc<AtomicBool>,
    cancel: CancellationToken,
    /// #114: the microphone's gate as the mode has it now, applied to every sink opened.
    passthrough: bool,
}

impl Worker {
    fn run(mut self, msgs: &crossbeam_channel::Receiver<Msg>) {
        self.open_sink();
        let opened = Instant::now();
        // #113: the TTS server's first request pays for its warm-up; on its own thread, so a
        // first press never queues behind it.
        if self.setup.warm_tts {
            self.warm_tts();
        }
        // The model loads before the first press needs it, not during it (a cold model takes
        // longer than the translation's budget). A failure shows on that press.
        let provider = Arc::clone(&self.setup.provider);
        let t = Instant::now();
        match self.rt.block_on(provider.warm_up()) {
            Ok(()) => tracing::info!(
                model = %self.setup.model,
                ms = t.elapsed().as_millis() as u64,
                "speak: translator loaded"
            ),
            Err(e) => tracing::warn!(error = %e, "speak: the model's warm-up failed"),
        }
        // #113: whether the call still has `dmn-voice`, once the session manager has had its
        // moment to relink it; a press before then answers that itself.
        let mut look_at = Some(opened + LINK_GRACE);
        loop {
            let next = match look_at {
                Some(at) => msgs.recv_deadline(at).map_err(|e| e.is_timeout()),
                None => msgs.recv().map_err(|_| false),
            };
            let job = match next {
                Ok(Msg::Speak(job)) => job,
                Ok(Msg::Passthrough(on)) => {
                    self.passthrough = on;
                    if let Some(sink) = self.sink.as_mut() {
                        sink.set_passthrough(on);
                    }
                    continue;
                }
                Ok(Msg::Language(language)) => {
                    tracing::info!(code = %language.code, voice = %language.voice, "speak: language from the next press");
                    self.setup.language = language;
                    continue;
                }
                Err(true) => {
                    look_at = None;
                    self.check_linked();
                    continue;
                }
                Err(false) => break,
            };
            look_at = None;
            if self.stopped() {
                break;
            }
            let id = job.id;
            match self.speak(job) {
                Ok(()) => self.hub.dispatch(Action::SpeakDone { id }),
                Err(reason) => self.hub.dispatch(Action::SpeakFailed { id, reason }),
            }
        }
        // The node goes first; a synthesis stopped while it waited for the server leaves a
        // blocking wait on the runtime's pool, which is not waited for.
        let Worker { sink, rt, .. } = self;
        drop(sink);
        rt.shutdown_background();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// #113: one word to the TTS server, its audio thrown away, on a thread of its own that
    /// nothing waits for. Logs how long the server took; a failure is only logged (the first
    /// press says it).
    fn warm_tts(&self) {
        let tts = Arc::clone(&self.setup.tts);
        let req = SpeakRequest::new(WARM_UP_TEXT, self.setup.language.voice.clone())
            .with_language(self.setup.language.code.clone())
            .with_tuning(self.setup.tuning);
        let stop = Arc::clone(&self.stop);
        let spawned = std::thread::Builder::new()
            .name("dmn-speak-warm".to_owned())
            .spawn(move || {
                let t = Instant::now();
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        return tracing::warn!(error = %e, "speak: no runtime for the TTS warm-up");
                    }
                };
                let warmed = rt.block_on(tts.synthesize(req)).map(|stream| {
                    let mut samples = 0usize;
                    for chunk in stream {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        samples += chunk.map(|c| c.samples.len()).unwrap_or(0);
                    }
                    samples
                });
                match warmed {
                    Ok(samples) => tracing::info!(
                        ms = t.elapsed().as_millis() as u64,
                        samples,
                        "speak: TTS server warmed up (audio discarded)"
                    ),
                    Err(e) => tracing::warn!(
                        ms = t.elapsed().as_millis() as u64,
                        error = %e,
                        "speak: the TTS warm-up failed"
                    ),
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "speak: cannot spawn the TTS warm-up");
        }
    }

    /// #113: nothing linked to `dmn-voice` a moment after start: the call does not have it
    /// (after a dmn restart, Discord keeps the old node's selection), so say so before the
    /// first press finds out.
    fn check_linked(&self) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        if sink.linked() == Some(false) {
            tracing::warn!(
                "speak: nothing is linked to {VMIC_NODE_NAME}; a call that had it before a restart needs it selected again"
            );
            self.hub.dispatch(Action::SpeakFailed {
                id: STARTUP_NOTICE,
                reason: NOT_LINKED.to_owned(),
            });
        }
    }

    fn open_sink(&mut self) -> Option<String> {
        match (self.setup.open_sink)() {
            Ok(mut sink) => {
                sink.set_passthrough(self.passthrough);
                self.sink = Some(sink);
                None
            }
            Err(why) => {
                tracing::warn!(error = %why, "speak: cannot open {VMIC_NODE_NAME}");
                Some(why)
            }
        }
    }

    /// One press: translate, synthesize, play, in the language set when it starts (#115: a
    /// language chosen meanwhile waits for the next press). `Err` is the status line's reason.
    fn speak(&mut self, job: Job) -> Result<(), String> {
        let Job { id, text, pressed } = job;
        let language = self.setup.language.clone();
        let provider = Arc::clone(&self.setup.provider);
        let translated = self
            .rt
            .block_on(dmn_llm::translate(
                &*provider,
                &text,
                &language.name,
                &language.code,
                self.setup.timeout,
                self.cancel.child_token(),
            ))
            .map_err(|e| {
                tracing::warn!(id, error = %e, "speak: translation failed");
                dmn_llm::translate::failure_text(&e)
            })?;
        let translated_ms = pressed.elapsed().as_millis() as u64;
        self.hub.dispatch(Action::SpeakTranslated {
            id,
            text: translated.text.clone(),
        });
        if self.stopped() {
            return Ok(());
        }
        if self.sink.is_none()
            && let Some(why) = self.open_sink()
        {
            return Err(format!("no {VMIC_NODE_NAME}: {why}"));
        }
        let req = SpeakRequest::new(translated.text, language.voice)
            .with_language(language.code)
            .with_tuning(self.setup.tuning);
        let tts = &self.setup.tts;
        let cancel = self.cancel.clone();
        let stream = self
            .rt
            .block_on(async {
                tokio::select! {
                    r = tts.synthesize(req) => r.map_err(Some),
                    () = cancel.cancelled() => Err(None),
                }
            })
            .map_err(|e| match e {
                Some(e) => {
                    tracing::warn!(id, error = %e, "speak: synthesis failed");
                    tts_failure(&e)
                }
                None => "stopped".to_owned(),
            })?;
        let played = self.play(stream);
        let done_ms = pressed.elapsed().as_millis() as u64;
        match &played {
            Ok(first) => tracing::info!(
                id,
                translated_ms,
                first_audio_ms =
                    first.map(|t| t.saturating_duration_since(pressed).as_millis() as u64),
                done_ms,
                "speak: spoke into {VMIC_NODE_NAME}"
            ),
            Err(why) => {
                tracing::warn!(id, translated_ms, done_ms, error = %why, "speak: playback failed")
            }
        }
        played.map(|_| ())
    }

    /// Feed the speech into the sink as it arrives, then wait for its last sample to play.
    /// Returns when PipeWire pulled the first of it. Speech nobody pulled is an error, and the
    /// node is opened afresh so it does not play at the next press.
    fn play(&mut self, stream: PcmStream) -> Result<Option<Instant>, String> {
        let Some(sink) = self.sink.as_mut() else {
            return Err(format!("no {VMIC_NODE_NAME}"));
        };
        let windows = &self.windows;
        let stop = &self.stop;
        let (_, base, _, _) = sink.stats();
        let mut first: Option<Instant> = None;
        let mut started = |sink: &dyn SpeechSink| {
            if first.is_none() && sink.stats().1 > base {
                let now = Instant::now();
                first = Some(now);
                let _ = windows.send(Window::From(now));
            }
        };
        let fed = (|| {
            for chunk in stream {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let chunk = chunk.map_err(|e| tts_failure(&e))?;
                sink.write(&chunk.samples, chunk.sample_rate)
                    .map_err(|e| playback_failure(&e))?;
                started(sink.as_ref());
            }
            Ok(())
        })();
        let drained = fed.and_then(|()| {
            let rate = sink.rate().max(1);
            let (written, pulled, _, _) = sink.stats();
            let queued =
                Duration::from_secs_f64(written.saturating_sub(pulled) as f64 / f64::from(rate));
            let deadline = Instant::now() + queued + DRAIN_MARGIN;
            loop {
                let done = sink.drain(DRAIN_TICK).map_err(|e| playback_failure(&e))?;
                started(sink.as_ref());
                if done {
                    return Ok(true);
                }
                if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Ok(false);
                }
            }
        });
        if first.is_some() {
            let _ = windows.send(Window::Until(Instant::now()));
        }
        let unheard = match drained {
            Ok(true) => return Ok(first),
            Ok(false) if self.stopped() => return Ok(first),
            Ok(false) => {
                tracing::warn!(
                    "speak: nothing pulled from {VMIC_NODE_NAME}; is it the call's microphone?"
                );
                NOT_LINKED.to_owned()
            }
            Err(why) => why,
        };
        // Whatever is still queued would play at the next press: start from an empty node.
        self.sink = None;
        self.open_sink();
        Err(unheard)
    }
}

/// A TTS failure in the status line's words.
fn tts_failure(e: &TtsError) -> String {
    let why = match e {
        TtsError::Unreachable { .. } => "server down".to_owned(),
        TtsError::UnknownVoice { .. } => "voice not enrolled".to_owned(),
        TtsError::HttpStatus { status, .. } => format!("HTTP {status}"),
        TtsError::Decode { .. } | TtsError::BodyRead { .. } => "bad audio".to_owned(),
        other => other.to_string().chars().take(40).collect(),
    };
    format!("tts failed: {why}")
}

/// A playback failure in the status line's words: nothing pulling a full ring is the one the
/// user can fix.
fn playback_failure(e: &AudioError) -> String {
    let text = e.to_string();
    if text.contains("nothing pulled") {
        NOT_LINKED.to_owned()
    } else {
        format!(
            "playback failed: {}",
            text.chars().take(40).collect::<String>()
        )
    }
}

/// `es` as the model should read it: "Spanish (es)". `None` for a code whisper does not know.
pub fn target_name(code: &str) -> Option<String> {
    let name = dmn_stt::Language::name_of(code)?;
    let mut chars = name.chars();
    let first = chars.next()?;
    Some(format!(
        "{}{} ({code})",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Check, before the run starts, everything a press would need and could not fix mid-call:
/// speak is on and the TTS server is on this machine, a voice is enrolled and the server has
/// it, and that voice speaks `[speak].target` (#109: a voice speaks only the language it was
/// enrolled with). #115: with `[speak].languages` listed, every entry is checked the same way
/// (its voice is in the library, enrolled for its code), a missing one naming the enroll
/// command. Returns the languages in the config's order, the start-up `target` among them.
pub fn check(
    cfg: &Config,
    paths: &Paths,
    tts: &dyn TtsProvider,
) -> Result<Vec<Language>, SpeakError> {
    let config = paths.config_file.display().to_string();
    speak::check_gates(cfg, paths)?;
    let listed = !cfg.speak.languages.is_empty();
    if !listed && cfg.speak.voice_ref.is_empty() {
        return Err(SpeakError::NotEnrolled { config });
    }
    let target = cfg.speak.target.clone();
    if target_name(&target).is_none() {
        return Err(SpeakError::UnknownTarget { target, config });
    }
    let entries = cfg.speak.voices();
    for entry in &entries {
        if target_name(&entry.code).is_none() {
            return Err(SpeakError::UnknownLanguage {
                code: entry.code.clone(),
                config,
            });
        }
    }
    let languages: Vec<Language> = entries
        .iter()
        .map(|l| Language::new(&l.code, &l.voice))
        .collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SpeakError::Runtime(e.to_string()))?;
    let Some(voices) = rt
        .block_on(tts.voices())
        .map_err(|e| speak::tts_error(e, paths))?
    else {
        tracing::warn!("the TTS server lists no voices; not checking the voices' languages");
        return Ok(languages);
    };
    for language in &languages {
        let (code, voice) = (language.code.clone(), language.voice.clone());
        // The clip the fix would re-upload: `voice_ref`'s for the one pair, the stored copy
        // `enroll` keeps for a listed voice.
        let clip = if listed {
            paths
                .data_dir
                .join(format!("{voice}.wav"))
                .display()
                .to_string()
        } else {
            cfg.speak.voice_ref.clone()
        };
        let Some(enrolled) = voices.iter().find(|v| v.answers_to(&voice)) else {
            return Err(if listed {
                SpeakError::LanguageVoiceGone {
                    code,
                    voice,
                    config,
                }
            } else {
                SpeakError::VoiceGone { voice, clip }
            });
        };
        match enrolled.language.as_deref() {
            Some(speaks) if speaks != code => {
                return Err(if listed {
                    SpeakError::LanguageVoiceMismatch {
                        code,
                        speaks: speaks.to_owned(),
                        voice,
                        clip,
                    }
                } else {
                    SpeakError::LanguageMismatch {
                        target: code,
                        speaks: speaks.to_owned(),
                        voice,
                        clip,
                        config,
                    }
                });
            }
            Some(_) => {}
            None => {
                tracing::warn!(voice = %voice, "the TTS server does not say which language the voice speaks");
            }
        }
    }
    Ok(languages)
}

/// The session's speaker for `dmn run`, after [`check`]: the translator (`[llm]` with
/// `[speak].model`, #113), the TTS server's streaming route, and `dmn-voice` with the
/// session's microphone `mic` (`[audio].mic` or `--mic`; `"default"` is PipeWire's default
/// source) passing through it (#114). `None` opens the node without a pass-through, which the
/// live tests do so that none of them captures the real microphone.
pub fn setup(cfg: &Config, paths: &Paths, mic: Option<&str>) -> Result<SpeakSetup, SpeakError> {
    let tts = dmn_tts::ChatterboxProvider::new(&cfg.speak.tts_url).streaming();
    let languages = check(cfg, paths, &tts)?;
    let provider = wire::provider(&cfg.translator_llm())
        .map_err(|e: LlmError| SpeakError::Llm(e.to_string()))?;
    let target = cfg.speak.target.clone();
    // `check` passed, so the target is listed (the config refuses one that is not).
    let language = languages
        .iter()
        .find(|l| l.code == target)
        .cloned()
        .ok_or_else(|| SpeakError::UnknownTarget {
            target: target.clone(),
            config: paths.config_file.display().to_string(),
        })?;
    let passthrough = cfg.speak.mode == VoiceMode::Passthrough;
    let tap = mic.map(|mic| Passthrough {
        target: match CaptureTarget::mic(mic) {
            CaptureTarget::Mic(name) => name,
            _ => None,
        },
        on: passthrough,
    });
    Ok(SpeakSetup {
        provider,
        model: cfg.translator_model().to_owned(),
        tts: Arc::new(tts),
        open_sink: Box::new(move || open_dmn_voice(tap.clone())),
        language,
        languages,
        timeout: Duration::from_millis(cfg.llm.fast_timeout_ms),
        tuning: speak::tuning(cfg),
        warm_tts: true,
        passthrough,
    })
}

/// `dmn-voice` on its own PipeWire connection, which lives as long as the node, with the
/// microphone passing through it when `passthrough` says (#114).
/// `Err` is short: it follows `no dmn-voice: ` on the status line, and the full error is logged.
fn open_dmn_voice(passthrough: Option<Passthrough>) -> Result<Box<dyn SpeechSink + Send>, String> {
    let (ctx, mic) = speak::open_vmic_with(passthrough).map_err(|e| {
        tracing::warn!(error = %e, "speak: cannot open {VMIC_NODE_NAME}");
        match e {
            SpeakError::NoPipeWire { .. } => "PipeWire is not running".to_owned(),
            other => other.to_string().chars().take(40).collect(),
        }
    })?;
    if let Some(why) = mic.passthrough_error() {
        tracing::warn!(error = %why, "speak: the microphone does not pass through {VMIC_NODE_NAME}");
    }
    Ok(Box::new(SessionMic { mic, _ctx: ctx }))
}

/// The session's `dmn-voice` and the connection it lives on; the node goes before the context.
struct SessionMic {
    mic: dmn_audio::VirtualMic,
    _ctx: Arc<dmn_audio::PipeWireContext>,
}

impl SpeechSink for SessionMic {
    fn linked(&self) -> Option<bool> {
        Some(self.mic.status().state() == VmicState::Streaming)
    }

    fn rate(&self) -> u32 {
        self.mic.rate()
    }

    fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
        self.mic.write(samples, sample_rate)
    }

    fn drain(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        self.mic.drain(timeout)
    }

    fn stats(&self) -> (u64, u64, u64, Option<Instant>) {
        let s = self.mic.stats();
        (s.written, s.pulled, s.underruns, s.first_pull)
    }

    fn set_passthrough(&mut self, on: bool) {
        self.mic.set_passthrough(on);
    }
}

/// What reached a [`MemorySink`]: each chunk's sample count and rate, in order, and whether
/// the microphone's gate was set open at the time (#114: it never is while speech is written
/// in translate mode).
#[cfg(test)]
pub type Fed = Arc<std::sync::Mutex<Vec<(usize, u32, bool)>>>;

/// A sink standing in for `dmn-voice` in tests: keeps every chunk, and "plays" all it has at the
/// first `drain` (or never, for a call that has not selected it).
#[cfg(test)]
pub struct MemorySink {
    pub rate: u32,
    pub log: Fed,
    pub plays: bool,
    written: u64,
    pulled: u64,
    /// #114: the gate as last set; shared so a test can read it.
    pub passthrough: Arc<AtomicBool>,
}

#[cfg(test)]
impl MemorySink {
    pub fn new(plays: bool) -> (Self, Fed) {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                rate: 48_000,
                log: Arc::clone(&log),
                plays,
                written: 0,
                pulled: 0,
                passthrough: Arc::new(AtomicBool::new(true)),
            },
            log,
        )
    }
}

#[cfg(test)]
impl SpeechSink for MemorySink {
    fn linked(&self) -> Option<bool> {
        Some(self.plays)
    }

    fn rate(&self) -> u32 {
        self.rate
    }

    fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                samples.len(),
                sample_rate,
                self.passthrough.load(Ordering::Relaxed),
            ));
        self.written += samples.len() as u64 * u64::from(self.rate) / u64::from(sample_rate);
        Ok(())
    }

    fn drain(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        if self.plays {
            self.pulled = self.written;
        } else {
            // Nobody pulls: waits out the timeout, as `dmn-voice` would.
            std::thread::sleep(timeout);
        }
        Ok(self.plays)
    }

    fn stats(&self) -> (u64, u64, u64, Option<Instant>) {
        (self.written, self.pulled, 0, None)
    }

    fn set_passthrough(&mut self, on: bool) {
        self.passthrough.store(on, Ordering::Relaxed);
    }
}

/// A speaker setup over a mock model and a mock TTS server, playing into a [`MemorySink`]
/// (the first one opened; any later one plays at once), and the log of what reached that sink.
#[cfg(test)]
pub fn setup_with(
    llm: dmn_llm::MockProvider,
    tts: dmn_tts::MockTtsProvider,
    plays: bool,
) -> (SpeakSetup, Fed) {
    setup_on(Arc::new(llm), tts, plays)
}

/// [`setup_with`] over any provider (#113: a real Ollama client on a stand-in server).
#[cfg(test)]
pub fn setup_on(
    llm: Arc<dyn Provider>,
    tts: dmn_tts::MockTtsProvider,
    plays: bool,
) -> (SpeakSetup, Fed) {
    let (setup, log, _) = setup_sink(llm, tts, plays);
    (setup, log)
}

/// [`setup_on`], with the first sink's microphone gate too (#114).
#[cfg(test)]
pub fn setup_sink(
    llm: Arc<dyn Provider>,
    tts: dmn_tts::MockTtsProvider,
    plays: bool,
) -> (SpeakSetup, Fed, Arc<AtomicBool>) {
    let (sink, log) = MemorySink::new(plays);
    let gate = Arc::clone(&sink.passthrough);
    let mut sink = Some(sink);
    let open: OpenSink = Box::new(move || match sink.take() {
        Some(s) => Ok(Box::new(s) as Box<dyn SpeechSink + Send>),
        None => {
            let (s, _) = MemorySink::new(true);
            Ok(Box::new(s))
        }
    });
    (
        SpeakSetup {
            provider: llm,
            model: "mock".into(),
            tts: Arc::new(tts),
            open_sink: open,
            language: Language::new("es", "me"),
            // #115: two more, for the picker; `de`'s voice is named as `enroll` would name it.
            languages: vec![
                Language::new("es", "me"),
                Language::new("de", "voice-de"),
                Language::new("ru", "voice-ru"),
            ],
            timeout: Duration::from_secs(4),
            tuning: Tuning::default(),
            warm_tts: false,
            passthrough: true,
        },
        log,
        gate,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dmn_llm::MockProvider;
    use dmn_llm::mock::Step;
    use dmn_session::Session;
    use dmn_tts::MockTtsProvider;

    const SAID: &str = "I'd start with a token bucket per API key.";
    const SPANISH: &str = "Empezaría con un token bucket por clave de API.";

    fn cfg(target: &str) -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.target = target.into();
        cfg.speak.voice_ref = "/data/me.wav".into();
        (dir, paths, cfg)
    }

    /// The start-up check: the enrolled voice must speak `target`; a mismatch names both and
    /// the command that fixes it.
    #[test]
    fn the_enrolled_voice_must_speak_the_target_language() {
        let (_dir, paths, cfg) = cfg("es");
        let ok = MockTtsProvider::new().with_voices(Some(&[("me", Some("es"))]));
        assert_eq!(
            check(&cfg, &paths, &ok).unwrap(),
            [Language::new("es", "me")],
            "no list: the one target + voice_ref pair"
        );

        let english = MockTtsProvider::new().with_voices(Some(&[("me", Some("en"))]));
        let err = check(&cfg, &paths, &english).unwrap_err();
        assert!(matches!(err, SpeakError::LanguageMismatch { .. }), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("[speak].target = \"es\"")
                && msg.contains("the enrolled voice \"me\" speaks \"en\""),
            "{msg}"
        );
        assert!(
            msg.contains("re-enroll with --language es: dmn speak enroll --wav /data/me.wav --name me --language es"),
            "{msg}"
        );

        let gone = MockTtsProvider::new().with_voices(Some(&[("someone", Some("es"))]));
        let err = check(&cfg, &paths, &gone).unwrap_err();
        assert!(matches!(err, SpeakError::VoiceGone { .. }), "{err}");

        // A server with no library cannot be asked; the run goes on.
        let no_library = MockTtsProvider::new().with_voices(None);
        assert_eq!(
            check(&cfg, &paths, &no_library).unwrap(),
            [Language::new("es", "me")]
        );
    }

    /// #115: with `[speak].languages` listed, every entry's voice must be in the library,
    /// enrolled for its code; a missing or mismatched one names the entry and the enroll
    /// command, and `voice_ref` is not needed.
    #[test]
    fn every_listed_voice_must_be_in_the_library_in_its_language() {
        let (_dir, paths, mut cfg) = cfg("de");
        cfg.speak.voice_ref.clear();
        cfg.speak.languages = vec![
            SpeakLanguage {
                code: "de".into(),
                voice: "voice-de".into(),
            },
            SpeakLanguage {
                code: "es".into(),
                voice: "voice-es".into(),
            },
        ];
        let all = MockTtsProvider::new()
            .with_voices(Some(&[("voice-de", Some("de")), ("voice-es", Some("es"))]));
        assert_eq!(
            check(&cfg, &paths, &all).unwrap(),
            [
                Language::new("de", "voice-de"),
                Language::new("es", "voice-es")
            ]
        );

        let missing = MockTtsProvider::new().with_voices(Some(&[("voice-de", Some("de"))]));
        let err = check(&cfg, &paths, &missing).unwrap_err();
        assert!(matches!(err, SpeakError::LanguageVoiceGone { .. }), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains("names the voice \"voice-es\" for \"es\"")
                && msg.contains("dmn speak enroll --wav <clip> --language es --name voice-es"),
            "{msg}"
        );

        let wrong = MockTtsProvider::new()
            .with_voices(Some(&[("voice-de", Some("de")), ("voice-es", Some("en"))]));
        let err = check(&cfg, &paths, &wrong).unwrap_err();
        assert!(
            matches!(err, SpeakError::LanguageVoiceMismatch { .. }),
            "{err}"
        );
        let msg = err.to_string();
        let clip = paths.data_dir.join("voice-es.wav").display().to_string();
        assert!(
            msg.contains("says the voice \"voice-es\" speaks \"es\"")
                && msg.contains("enrolled for \"en\"")
                && msg.contains(&format!(
                    "dmn speak enroll --wav {clip} --language es --name voice-es"
                )),
            "{msg}"
        );

        // A code whisper does not know, anywhere in the list, is refused before the server
        // is asked.
        cfg.speak.languages.push(SpeakLanguage {
            code: "xx".into(),
            voice: "v".into(),
        });
        let err = check(&cfg, &paths, &all).unwrap_err();
        assert!(matches!(err, SpeakError::UnknownLanguage { .. }), "{err}");
        assert!(err.to_string().contains("\"xx\""), "{err}");
    }

    #[test]
    fn the_check_refuses_what_a_press_could_not_fix() {
        let mock = MockTtsProvider::new().with_voices(Some(&[("me", Some("es"))]));
        let (_dir, paths, mut c) = cfg("es");
        c.speak.voice_ref.clear();
        assert!(matches!(
            check(&c, &paths, &mock),
            Err(SpeakError::NotEnrolled { .. })
        ));
        let (_dir, paths, c) = cfg("xx");
        let err = check(&c, &paths, &mock).unwrap_err();
        assert!(matches!(err, SpeakError::UnknownTarget { .. }), "{err}");
        let (_dir, paths, mut c) = cfg("es");
        c.speak.tts_url = "http://gpu-box.lan:4123".into();
        assert!(matches!(
            check(&c, &paths, &mock),
            Err(SpeakError::NotLocal { .. })
        ));
        let url = dmn_tts::testserver::closed_port_url().unwrap();
        let (_dir, paths, mut c) = cfg("es");
        c.speak.tts_url = url.clone();
        let err = check(&c, &paths, &dmn_tts::ChatterboxProvider::new(&url)).unwrap_err();
        assert!(matches!(err, SpeakError::ServerDown { .. }), "{err}");
    }

    #[test]
    fn a_target_is_named_for_the_model() {
        assert_eq!(target_name("es").as_deref(), Some("Spanish (es)"));
        assert_eq!(target_name("de").as_deref(), Some("German (de)"));
        assert_eq!(target_name("xx"), None);
    }

    /// Wait until the session has no press in flight (or 5 s).
    pub(crate) fn settled(hub: &SessionHub) -> Session {
        let t = Instant::now();
        loop {
            let s = hub.snapshot();
            if s.speech.is_none() || t.elapsed() > Duration::from_secs(5) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_press_translates_then_synthesizes_the_translation_then_plays_it() {
        let llm = MockProvider::new(vec![Step::Text(SPANISH.into()), Step::Done]);
        let tts = MockTtsProvider::new();
        let (setup, played) = setup_with(llm.clone(), tts.clone(), true);
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 1 });
        assert!(speaker.speak(1, SAID.into()));
        let s = settled(&hub);
        assert_eq!(s.speech, None);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);

        let asked = llm.requests();
        assert_eq!(asked.len(), 1, "one translate request");
        assert_eq!(asked[0].messages[1].content, SAID);
        assert!(asked[0].messages[0].content.contains("into Spanish (es)"));
        let spoken = tts.requests();
        assert_eq!(spoken.len(), 1, "one synthesize request");
        assert_eq!(spoken[0].text, SPANISH);
        assert_eq!(spoken[0].voice, "me");
        assert_eq!(spoken[0].language.as_deref(), Some("es"));
        let fed: usize = played.lock().unwrap().iter().map(|(n, ..)| n).sum();
        assert_eq!(fed, 12_000, "the mock's 0.5 s at 24 kHz reached the sink");
        let windows = speaker.windows();
        assert!(
            matches!(windows.as_slice(), [Window::From(a), Window::Until(b)] if a <= b),
            "{windows:?}"
        );
        speaker.stop();
    }

    /// #115: the issue's criterion at the worker: a language set between two presses applies
    /// to the second (its translate request names the new language, its synthesize request
    /// the new voice and code); one set while a press is in flight waits for the next.
    #[test]
    fn a_language_set_between_presses_applies_to_the_next_one_and_never_to_one_in_flight() {
        const GERMAN: &str = "Ich würde mit einem Token-Bucket pro API-Schlüssel anfangen.";
        let german = || vec![Step::Text(GERMAN.into()), Step::Done];
        let llm = MockProvider::new(vec![Step::Text(SPANISH.into()), Step::Done])
            .then(german())
            .then(german())
            .with_delay(Duration::from_millis(200));
        let tts = MockTtsProvider::new();
        let (setup, _) = setup_with(llm.clone(), tts.clone(), true);
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        // Press 1 in Spanish; German is chosen while it translates.
        hub.dispatch(Action::SpeakStarted { id: 1 });
        assert!(speaker.speak(1, SAID.into()));
        std::thread::sleep(Duration::from_millis(50));
        speaker.set_language(Language::new("de", "voice-de"));
        let s = settled(&hub);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);
        // Press 2 and 3 in German.
        hub.dispatch(Action::SpeakStarted { id: 2 });
        assert!(speaker.speak(2, SAID.into()));
        let s = settled(&hub);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);
        speaker.set_language(Language::new("ru", "voice-ru"));
        speaker.set_language(Language::new("de", "voice-de"));
        hub.dispatch(Action::SpeakStarted { id: 3 });
        assert!(speaker.speak(3, SAID.into()));
        let s = settled(&hub);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);

        let asked = llm.requests();
        assert_eq!(asked.len(), 3);
        assert!(asked[0].messages[0].content.contains("into Spanish (es)"));
        assert!(
            asked[1].messages[0].content.contains("into German (de)"),
            "{}",
            asked[1].messages[0].content
        );
        assert!(asked[2].messages[0].content.contains("into German (de)"));
        let spoken = tts.requests();
        let what: Vec<(&str, &str, Option<&str>)> = spoken
            .iter()
            .map(|r| (r.text.as_str(), r.voice.as_str(), r.language.as_deref()))
            .collect();
        assert_eq!(
            what,
            [
                (SPANISH, "me", Some("es")),
                (GERMAN, "voice-de", Some("de")),
                (GERMAN, "voice-de", Some("de")),
            ],
            "the first press kept Spanish and its voice; the next two took German"
        );
        speaker.stop();
    }

    /// #113: at start the TTS server gets one word, whose audio goes nowhere, on a thread of
    /// its own: a press made at once is not held up by it, and its request carries `[speak]`'s
    /// knobs as the warm-up's does.
    #[test]
    fn the_tts_server_is_warmed_up_at_start_and_a_first_press_does_not_wait_for_it() {
        let llm = MockProvider::new(vec![Step::Text(SPANISH.into()), Step::Done]);
        let tts = MockTtsProvider::new();
        let (mut setup, played) = setup_with(llm, tts.clone(), true);
        setup.warm_tts = true;
        setup.tuning = Tuning {
            temperature: Some(0.6),
            cfg_weight: Some(0.4),
            exaggeration: None,
        };
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 1 });
        assert!(speaker.speak(1, SAID.into()));
        let s = settled(&hub);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);
        let t = Instant::now();
        while tts.requests().len() < 2 && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let spoken = tts.requests();
        assert_eq!(spoken.len(), 2, "the warm-up and the press: {spoken:?}");
        let warm = spoken
            .iter()
            .find(|r| r.text == WARM_UP_TEXT)
            .expect("a warm-up");
        assert_eq!(
            (warm.voice.as_str(), warm.language.as_deref()),
            ("me", Some("es"))
        );
        let press = spoken
            .iter()
            .find(|r| r.text == SPANISH)
            .expect("the press");
        for req in [warm, press] {
            assert_eq!(req.tuning.temperature, Some(0.6));
            assert_eq!(req.tuning.cfg_weight, Some(0.4));
            assert_eq!(req.tuning.exaggeration, None);
        }
        let fed: usize = played.lock().unwrap().iter().map(|(n, ..)| n).sum();
        assert_eq!(
            fed, 12_000,
            "only the press reached the sink, not the warm-up"
        );
        speaker.stop();
    }

    /// #113: a moment after start, nothing linked to `dmn-voice` (a call that had it before
    /// a restart) is said on the status line, under no press's id; a linked one says nothing.
    #[test]
    fn nothing_linked_a_moment_after_start_is_said_once() {
        let started = |plays: bool| {
            let llm = MockProvider::new(vec![Step::Done]);
            let (setup, _) = setup_with(llm, MockTtsProvider::new(), plays);
            let hub = Arc::new(SessionHub::default());
            let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
            (hub, speaker)
        };
        let (unheard, a) = started(false);
        let (heard, b) = started(true);
        std::thread::sleep(LINK_GRACE + Duration::from_millis(300));
        let notice = unheard.snapshot().speak_failed.expect("a notice");
        assert_eq!(
            (notice.id, notice.reason.as_str()),
            (STARTUP_NOTICE, NOT_LINKED)
        );
        assert_eq!(heard.snapshot().speak_failed, None);
        a.stop();
        b.stop();
    }

    #[test]
    fn failures_end_the_press_with_a_reason_never_a_panic() {
        // The model fails: nothing is synthesized.
        let llm = MockProvider::new(vec![Step::Error("model not found".into())]);
        let tts = MockTtsProvider::new();
        let (setup, _) = setup_with(llm, tts.clone(), true);
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 1 });
        speaker.speak(1, SAID.into());
        let s = settled(&hub);
        assert_eq!(
            s.speak_failed.map(|n| n.reason).as_deref(),
            Some("translate failed: model not found")
        );
        assert!(tts.requests().is_empty());
        speaker.stop();

        // The TTS server's answer is not audio.
        let llm = MockProvider::new(vec![Step::Text(SPANISH.into()), Step::Done]);
        let (setup, _) = setup_with(llm, MockTtsProvider::with_wav(b"nope".to_vec()), true);
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 2 });
        speaker.speak(2, SAID.into());
        let s = settled(&hub);
        assert_eq!(
            s.speak_failed.map(|n| n.reason).as_deref(),
            Some("tts failed: bad audio")
        );
        speaker.stop();

        // Nothing pulls dmn-voice: said, and the next press plays into a fresh node.
        let llm = MockProvider::new(vec![Step::Text(SPANISH.into()), Step::Done]);
        let (setup, _) = setup_with(llm, MockTtsProvider::new(), false);
        let hub = Arc::new(SessionHub::default());
        let speaker = Speaker::start(setup, Arc::clone(&hub)).unwrap();
        hub.dispatch(Action::SpeakStarted { id: 3 });
        speaker.speak(3, SAID.into());
        let s = settled(&hub);
        assert_eq!(
            s.speak_failed.map(|n| n.reason).as_deref(),
            Some(NOT_LINKED)
        );
        assert!(speaker.windows().is_empty(), "nothing played: no window");
        hub.dispatch(Action::SpeakStarted { id: 4 });
        speaker.speak(4, SAID.into());
        let s = settled(&hub);
        assert_eq!(s.speak_failed, None, "{:?}", s.speak_failed);
        speaker.stop();
    }
}
