//! `dmn run`: the copilot in a terminal beside the call (SPEC § UI, Terminal MVP).
//!
//! The pipeline ([`crate::pipeline`], the same one `dmn replay` runs) works on its own thread
//! and dispatches everything into the session; this thread redraws the TUI from the session's
//! `watch` channel whenever it changes and turns keys into [`Controls`] calls: `p` pin / unpin,
//! `r` regenerate, `m` mute the mic, `s` coding solutions hints ↔ full (#50), Ctrl+Shift+A
//! interview assistance on ↔ off (#79, a chord so it is not hit by accident), Enter ask about
//! what the asking lane last said (the interviewer, by default) when the detector missed the
//! question (#96), `←` / `→` step back to an earlier question and forward again, `↑` / `↓` and
//! PgUp / PgDn scroll an open section taller than its rows (#94), `q` (or Ctrl-C) quit. Nothing
//! else writes to the terminal while the TUI is up: the fast phase is [`Output::Quiet`], warnings go to the log file, and
//! errors show only as the status dots. The terminal is restored on the way out, on an error,
//! and (through ratatui's panic hook) on a panic.
//!
//! With no flags it captures the call live (#16): the mic as You and the call's app output as the
//! Interviewer, from `[audio].mic` / `[audio].app_stream` or `--mic` / `--app` (`dmn devices`
//! lists the names), until `q`. `--replay <interviewer.wav> <candidate.wav>` feeds the same
//! pipeline from files at 1× instead (#15), and `--headless` runs a replay with no terminal and
//! prints how long each scripted question took to reach the screen (#15's latency criterion).

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use dmn_audio::CaptureTarget;
use dmn_core::wire::AudioClock;
use dmn_core::{Config, Paths};
use dmn_session::{CodingSolutions, Section, Session};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use tokio::sync::watch;

use crate::latency::{self, Screen, Seen};
use crate::pipeline::{
    Controls, Found, Input, Observer, Options, Partials, Pipeline, Quiet, Report,
};
use crate::replay;
use crate::suggest::{Output, Policy};
use crate::tui;

/// SPEC § UI: the Approach is visible within 2.0 s of the question ending.
pub const APPROACH_BUDGET_S: f64 = 2.0;

/// How often the TUI looks for keys and session changes.
const FRAME: Duration = Duration::from_millis(33);
/// How often `--headless` samples the session: the resolution of the latency it reports.
const SAMPLE: Duration = Duration::from_millis(10);

pub struct Args {
    /// `--replay <interviewer.wav> <candidate.wav>`; `None` captures live.
    pub replay: Option<(PathBuf, PathBuf)>,
    /// Live: the mic's `node.name` (or "default"), over `[audio].mic`.
    pub mic: Option<String>,
    /// Live: the call app's `application.name` ("" for all system audio), over
    /// `[audio].app_stream`.
    pub app: Option<String>,
    pub model: Option<String>,
    pub threads: Option<usize>,
    pub vad_close_ms: Option<u16>,
    /// `--vad`, over `[stt].vad` (#62).
    pub vad: Option<dmn_core::config::Vad>,
    /// `--device` (#61), over `[stt].device`.
    pub device: Option<dmn_core::config::SttDevice>,
    /// `--language` (#77), over `[stt].language`.
    pub language: Option<dmn_core::config::SttLanguage>,
    /// `--question-source` (#90), over `[detect].question_source`.
    pub question_source: Option<dmn_core::config::QuestionSource>,
    pub no_store: bool,
    pub headless: bool,
    /// Live: stop after this many seconds, as if `q` had been pressed.
    pub duration: Option<u64>,
    /// `--solutions <hints|full>`: coding solutions for this run, over `[ui].coding_solutions`.
    pub solutions: Option<CodingSolutions>,
    /// `--no-assist` (#79): start with interview assistance off, over `[assist].enabled`.
    pub no_assist: bool,
}

/// A key the TUI acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Pin,
    Regenerate,
    Mute,
    /// `s`: coding solutions hints ↔ full (#50).
    Solutions,
    /// `1`–`6`: open that section under the Approach, or close it (#56).
    Section(Section),
    /// `w`: Swap the STAR block to the next matched story (#58). `s` was taken by coding
    /// solutions; `w` is the other letter of "swap" that is free and sits beside `q`/`r`.
    Swap,
    /// Ctrl+Shift+A: interview assistance on ↔ off (#79). A chord, not a letter, so a stray
    /// key mid-call cannot switch the Approach off. Terminals without the kitty keyboard
    /// protocol (WezTerm's default, tmux) send it as Ctrl+A, so that counts too.
    Assist,
    /// Enter (#96): send what the asking lane last said to the model as the question, for when
    /// the detector missed it. The lane follows `question_source`: the interviewer by default,
    /// the mic under `you`, whichever spoke last under `both`. `r` only reaches a question
    /// already on screen.
    Ask,
    /// `←` (#94): show the question before the one on screen.
    Earlier,
    /// `→` (#94): show the question after it; from the newest earlier one, the live question.
    Later,
    /// `↑ ↓ PgUp PgDn` (#94): scroll the open section, a row or the rows visible.
    Scroll(tui::Scroll),
    Quit,
}

/// Map a terminal key event to a [`Key`]. Only presses count (terminals that report releases
/// would otherwise act twice).
pub fn key(ev: KeyEvent) -> Option<Key> {
    if ev.kind != KeyEventKind::Press {
        return None;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        return match ev.code {
            KeyCode::Char('c') => Some(Key::Quit),
            KeyCode::Char('a' | 'A') => Some(Key::Assist),
            _ => None,
        };
    }
    match ev.code {
        KeyCode::Char('p' | 'P') => Some(Key::Pin),
        KeyCode::Char('r' | 'R') => Some(Key::Regenerate),
        KeyCode::Char('m' | 'M') => Some(Key::Mute),
        KeyCode::Char('s' | 'S') => Some(Key::Solutions),
        KeyCode::Char('w' | 'W') => Some(Key::Swap),
        // SPEC § Terminal MVP: `1`–`6` toggle one section, in the fixed on-screen order.
        KeyCode::Char(c @ '1'..='6') => c
            .to_digit(10)
            .and_then(|d| Section::ALL.get(usize::try_from(d).ok()? - 1))
            .map(|s| Key::Section(*s)),
        KeyCode::Enter => Some(Key::Ask),
        KeyCode::Left => Some(Key::Earlier),
        KeyCode::Right => Some(Key::Later),
        KeyCode::Up => Some(Key::Scroll(tui::Scroll::Up)),
        KeyCode::Down => Some(Key::Scroll(tui::Scroll::Down)),
        KeyCode::PageUp => Some(Key::Scroll(tui::Scroll::PageUp)),
        KeyCode::PageDown => Some(Key::Scroll(tui::Scroll::PageDown)),
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Key::Quit),
        _ => None,
    }
}

/// Act on one key on a pane of `pane`, which decides how far a scroll key goes (#94). Returns
/// `true` when the TUI should close.
pub fn press(k: Key, controls: &Controls, pane: Rect) -> bool {
    match k {
        Key::Pin => controls.toggle_pin(),
        Key::Regenerate => controls.regenerate(),
        Key::Mute => controls.toggle_mute(),
        Key::Solutions => controls.toggle_coding_solutions(),
        Key::Section(s) => controls.toggle_section(s),
        Key::Swap => controls.swap_story(),
        Key::Assist => controls.toggle_assist(),
        Key::Ask => controls.ask(),
        Key::Earlier => controls.earlier(),
        Key::Later => controls.later(),
        Key::Scroll(by) => controls.scroll(|session| tui::scroll_to(session, pane, by)),
        Key::Quit => {
            controls.quit();
            return true;
        }
    }
    false
}

/// The live input from the config and the `--mic` / `--app` overrides.
pub fn live_input(cfg: &Config, args: &Args) -> Result<Input, String> {
    if !cfg.audio.system_audio && args.app.is_none() {
        return Err(
            "[audio].system_audio = false, but dmn run needs the call's audio for the \
                    interviewer: set it to true, or name the call's app with --app"
                .into(),
        );
    }
    Ok(Input::Live {
        mic: CaptureTarget::mic(args.mic.as_deref().unwrap_or(&cfg.audio.mic)),
        interviewer: CaptureTarget::interviewer(
            args.app.as_deref().unwrap_or(&cfg.audio.app_stream),
        ),
    })
}

pub fn run(cfg: &Config, paths: &Paths, args: &Args) -> Result<(), Box<dyn Error>> {
    if args.headless && args.replay.is_none() && args.duration.is_none() {
        return Err("--headless needs --replay, or --duration for a live capture".into());
    }
    // SPEC § Providers: say in words when transcript text leaves the machine. The TUI keeps it
    // on the status line; with no terminal it goes to stderr before anything is sent.
    if args.headless
        && let Some(notice) = cfg.llm.privacy_notice()
    {
        eprintln!("run: {notice}");
    }
    let (input, questions) = match args.replay.clone() {
        Some((interviewer, candidate)) => {
            let questions = interviewer.parent().map(|d| d.join("questions.json"));
            (
                Input::Files {
                    interviewer,
                    candidate,
                },
                questions,
            )
        }
        None => (live_input(cfg, args)?, None),
    };
    let pipeline = Pipeline::start(
        cfg,
        paths,
        &Options {
            input,
            // #15: `--replay` runs at 1×, like the call it stands in for.
            speed: 1.0,
            model: args.model.clone(),
            threads: args.threads,
            vad_close_ms: args.vad_close_ms,
            vad: args.vad,
            device: args.device,
            language: args.language.clone(),
            partials: Partials::Auto,
            spec_literal_detector: false,
            question_source: args.question_source,
            assist: args.no_assist.then_some(false),
            no_llm: false,
            no_store: args.no_store,
            output: Output::Quiet,
            // SPEC § Threading rules: a newer unpinned question cancels the one in flight.
            policy: Policy::Supersede,
            coding_solutions: args.solutions,
        },
    )?;
    let audio = pipeline.info().audio;
    let live = pipeline.info().live;
    if args.headless {
        // With no screen, the status line's whisper device goes to stdout (#61).
        println!("whisper      {}", pipeline.info().stt_device);
    }
    if let Some(secs) = args.duration {
        let controls = pipeline.controls();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            controls.quit();
        });
    }
    let report = if live && args.headless {
        // A live capture with no terminal, until --duration: the session log holds the timing.
        let (report, Quiet) = join(spawn(pipeline, Quiet, false))?;
        replay::print_summary(&report, report.wall);
        replay::print_downstream(&report, paths);
        if let Some(id) = report.session.and_then(|s| s.session_id) {
            println!("latency      dmn bench --live --session {id}");
        }
        report
    } else if args.headless {
        let (report, rows) = headless(pipeline, questions.as_deref())?;
        replay::print_summary(&report, audio);
        replay::print_downstream(&report, paths);
        match rows {
            Some(rows) => print!("\n{}", latency::render(&rows, APPROACH_BUDGET_S)),
            None => {
                println!("\nno questions.json next to the interviewer file: latency not measured")
            }
        }
        report
    } else {
        let report = interactive(pipeline)?;
        // At 1× the audio delivered is the wall time, up to the file's length: `q` can end the
        // run early, and after the audio ends the TUI stays up until `q`. A live call's audio is
        // its wall time.
        let delivered = if live {
            report.wall
        } else {
            audio.min(report.wall)
        };
        replay::print_summary(&report, delivered);
        replay::print_downstream(&report, paths);
        if live {
            if let Some(id) = report.session.and_then(|s| s.session_id) {
                println!("latency      dmn bench --live --session {id}");
            }
        } else if report.stopped {
            println!("stopped      with q before the audio ended");
        }
        report
    };
    if !report.failures.is_empty() {
        return Err(report.failures.join("; ").into());
    }
    Ok(())
}

type Worker<T> = JoinHandle<(Result<Report, String>, T)>;

/// Run the pipeline on its own thread with `observer`; `keep_alive` keeps it serving keys after
/// the audio ends, until quit.
fn spawn<O: Observer + Send + 'static>(
    mut pipeline: Pipeline,
    mut observer: O,
    keep_alive: bool,
) -> Worker<O> {
    std::thread::spawn(move || {
        pipeline.run(&mut observer, keep_alive);
        let report = pipeline.finish(&mut observer).map_err(|e| e.to_string());
        (report, observer)
    })
}

fn join<T>(worker: Worker<T>) -> Result<(Report, T), Box<dyn Error>> {
    let (report, extra) = worker.join().map_err(|_| "pipeline thread panicked")?;
    Ok((report?, extra))
}

/// Restores the terminal however the TUI loop ends.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

fn interactive(pipeline: Pipeline) -> Result<Report, Box<dyn Error>> {
    let controls = pipeline.controls();
    let mut session = pipeline.hub().watch();
    let worker = spawn(pipeline, Quiet, true);
    let ui = (|| -> Result<(), Box<dyn Error>> {
        let mut terminal = ratatui::try_init()?;
        let _restore = Restore;
        ui_loop(&mut terminal, &mut session, &controls)
    })();
    // However the UI ended, stop the pipeline so the session is closed and stored.
    controls.quit();
    let (report, Quiet) = join(worker)?;
    ui?;
    Ok(report)
}

fn ui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    session: &mut watch::Receiver<Session>,
    controls: &Controls,
) -> Result<(), Box<dyn Error>> {
    let mut dirty = true;
    loop {
        // `--duration` (or anything else) stopped the run: close as `q` would.
        if controls.is_stopped() {
            return Ok(());
        }
        if dirty || session.has_changed().unwrap_or(false) {
            // Clone out of the channel so the hub is never blocked behind a draw.
            let s = session.borrow_and_update().clone();
            terminal.draw(|f| tui::draw(f, &s))?;
            dirty = false;
        }
        if !event::poll(FRAME)? {
            continue;
        }
        match event::read()? {
            Event::Key(ev) => {
                if let Some(k) = key(ev) {
                    let pane = terminal.size()?;
                    if press(k, controls, Rect::new(0, 0, pane.width, pane.height)) {
                        return Ok(());
                    }
                    dirty = true;
                }
            }
            Event::Resize(..) => dirty = true,
            _ => {}
        }
    }
}

/// Records, on the pipeline thread, when each candidate's final arrived and when it was emitted.
struct Emitted {
    clock: AudioClock,
    seen: BTreeMap<u64, Seen>,
    /// The last candidate recorded: the one a continuation replaces.
    last: Option<u64>,
}

impl Observer for Emitted {
    fn candidate(&mut self, id: u64, found: &Found) {
        // #63: a continuation replaces the first part of its question on screen, so the question
        // is on screen when the whole of it is. Pairing the scripted question with the first
        // part would time a half question, often before the interviewer finished asking.
        if found.continues
            && let Some(prev) = self.last
        {
            self.seen.remove(&prev);
        }
        self.last = Some(id);
        let now = self.clock.now();
        self.seen.insert(
            id,
            Seen {
                start: found.candidate.start.as_secs_f64(),
                end: found.candidate.end.as_secs_f64(),
                final_at: now - found.latency.as_secs_f64(),
                emitted_at: now,
                ..Seen::default()
            },
        );
    }
}

/// What `--headless` measured: one row per scripted question, when the fixture has a
/// `questions.json`.
type Timed = Option<Vec<latency::Row>>;

/// The TUI's pipeline with no terminal: sample the session every 10 ms and time each scripted
/// question from its end to the screen.
fn headless(
    pipeline: Pipeline,
    questions: Option<&std::path::Path>,
) -> Result<(Report, Timed), Box<dyn Error>> {
    // The audio clock the session log's timing marks use, so `dmn bench --live` reads the same
    // times back from the stored session.
    let clock = pipeline.audio_clock();
    let mut session = pipeline.hub().watch();
    let worker = spawn(
        pipeline,
        Emitted {
            clock: clock.clone(),
            seen: BTreeMap::new(),
            last: None,
        },
        false,
    );
    let mut screen = Screen::default();
    while !worker.is_finished() {
        if session.has_changed().unwrap_or(false) {
            let now = clock.now();
            screen.observe(&session.borrow_and_update(), now);
        }
        std::thread::sleep(SAMPLE);
    }
    // The worker can finish between a sample and the next `is_finished` check; take the last
    // state so a reply that landed in that gap is not reported as missed (its time is then
    // at most one sample late).
    let now = clock.now();
    screen.observe(&session.borrow_and_update(), now);
    let (report, mut emitted) = join(worker)?;
    screen.apply(&mut emitted.seen);
    let rows = match questions.filter(|p| p.is_file()) {
        Some(path) => Some(latency::pair(&latency::load_scripted(path)?, &emitted.seen)),
        None => None,
    };
    Ok((report, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Command;
    use dmn_session::{Action, Health};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The pane every test here draws and presses its keys on: the scroll keys go as far as it
    /// has rows (#94).
    const PANE: Rect = Rect::new(0, 0, 100, 30);

    fn press(k: Key, controls: &Controls) -> bool {
        super::press(k, controls, PANE)
    }

    fn press_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// The key an arrow or a page key maps to.
    fn named(code: KeyCode) -> Key {
        key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
    }

    fn screen(session: &Session) -> String {
        let mut t = Terminal::new(TestBackend::new(PANE.width, PANE.height)).unwrap();
        t.draw(|f| tui::draw(f, session)).unwrap();
        let buf = t.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn question(candidate: u64, text: &str) -> Vec<Action> {
        vec![
            Action::Question {
                candidate,
                text: text.into(),
                kind: "system_design".into(),
            },
            Action::ApproachLine {
                candidate,
                index: 0,
                text: format!("first step for {text}"),
            },
        ]
    }

    #[test]
    fn keys_map_to_actions_and_only_presses_count() {
        assert_eq!(key(press_key('p')), Some(Key::Pin));
        assert_eq!(key(press_key('r')), Some(Key::Regenerate));
        assert_eq!(key(press_key('m')), Some(Key::Mute));
        assert_eq!(key(press_key('s')), Some(Key::Solutions));
        assert_eq!(key(press_key('a')), None, "a plain a does nothing (#79)");
        assert_eq!(key(press_key('A')), None);
        for (code, mods) in [
            ('A', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            ('a', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            // Without the kitty keyboard protocol Ctrl+Shift+A arrives as Ctrl+A.
            ('a', KeyModifiers::CONTROL),
        ] {
            let ev = KeyEvent::new(KeyCode::Char(code), mods);
            assert_eq!(key(ev), Some(Key::Assist), "{mods:?} {code}");
        }
        assert_eq!(key(press_key('q')), Some(Key::Quit));
        assert_eq!(
            key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Key::Quit)
        );
        assert_eq!(
            key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Quit)
        );
        assert_eq!(key(press_key('x')), None);
        let mut release = press_key('p');
        release.kind = KeyEventKind::Release;
        assert_eq!(key(release), None);
        // #96: Enter asks; its release, on a terminal that reports one, does not ask again.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key(enter), Some(Key::Ask));
        let mut release = enter;
        release.kind = KeyEventKind::Release;
        assert_eq!(key(release), None);
    }

    #[test]
    fn pin_blocks_replacement_and_shows_the_new_badge_then_unpin_shows_the_newest() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        for a in question(1, "Design a rate limiter") {
            hub.dispatch(a);
        }
        assert!(screen(&hub.snapshot()).contains("LIVE QUESTION"));

        assert!(!press(key(press_key('p')).unwrap(), &controls));
        for a in question(2, "Design a URL shortener") {
            hub.dispatch(a);
        }
        let pinned = screen(&hub.snapshot());
        assert!(pinned.contains("PINNED QUESTION"), "{pinned}");
        assert!(pinned.contains("1 new"), "{pinned}");
        assert!(
            pinned.contains("Design a rate limiter"),
            "not replaced:\n{pinned}"
        );
        assert!(!pinned.contains("URL shortener"), "{pinned}");

        press(Key::Pin, &controls);
        let unpinned = screen(&hub.snapshot());
        assert!(unpinned.contains("LIVE QUESTION"), "{unpinned}");
        assert!(unpinned.contains("Design a URL shortener"), "{unpinned}");
        assert!(!unpinned.contains("1 new"), "{unpinned}");
    }

    #[test]
    fn a_turns_assistance_off_to_the_transcript_alone_and_on_again() {
        // #79: the screen switches on the key; the pipeline hears about it on its channel.
        let (controls, commands) = Controls::for_test();
        let hub = controls.hub();
        for a in question(1, "Design a rate limiter") {
            hub.dispatch(a);
        }
        assert!(
            !press(
                key(KeyEvent::new(
                    KeyCode::Char('A'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ))
                .unwrap(),
                &controls
            ),
            "a does not quit"
        );
        let off = screen(&hub.snapshot());
        assert!(
            !off.contains("LIVE QUESTION") && !off.contains("APPROACH"),
            "{off}"
        );
        assert!(off.contains("assist off"), "{off}");
        assert_eq!(commands.try_recv(), Ok(Command::Assist { on: false }));

        press(Key::Assist, &controls);
        let on = screen(&hub.snapshot());
        assert!(
            on.contains("LIVE QUESTION") && on.contains("Design a rate limiter"),
            "{on}"
        );
        assert!(!on.contains("assist off"), "{on}");
        assert_eq!(commands.try_recv(), Ok(Command::Assist { on: true }));
    }

    #[test]
    fn mute_shows_on_the_mic_dot_and_quit_closes() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        assert!(!press(key(press_key('m')).unwrap(), &controls));
        assert_eq!(hub.snapshot().status.mic, Health::Muted);
        assert!(screen(&hub.snapshot()).contains("mic muted"));
        press(Key::Mute, &controls);
        assert_eq!(hub.snapshot().status.mic, Health::Ok);

        assert!(
            press(key(press_key('q')).unwrap(), &controls),
            "q closes the TUI"
        );
        assert!(controls.is_stopped());
    }

    #[test]
    fn s_toggles_coding_solutions_and_the_footer_follows_on_a_coding_question() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "Reverse a linked list in place.".into(),
            kind: "coding".into(),
        });
        let status_line = |s: &str| s.lines().last().unwrap_or_default().to_owned();
        assert_eq!(hub.snapshot().coding_solutions, CodingSolutions::Hints);
        assert!(status_line(&screen(&hub.snapshot())).contains("s: hints"));

        assert!(!press(key(press_key('s')).unwrap(), &controls));
        assert_eq!(hub.snapshot().coding_solutions, CodingSolutions::Full);
        assert!(status_line(&screen(&hub.snapshot())).contains("s: full"));

        press(Key::Solutions, &controls);
        assert_eq!(hub.snapshot().coding_solutions, CodingSolutions::Hints);
    }

    #[test]
    fn r_goes_to_the_pipeline_thread() {
        let (controls, commands) = Controls::for_test();
        assert!(!press(key(press_key('r')).unwrap(), &controls));
        assert_eq!(
            commands.try_recv(),
            Ok(crate::pipeline::Command::Regenerate)
        );
    }

    #[test]
    fn enter_goes_to_the_pipeline_thread_and_the_screen_waits_for_its_answer() {
        // #96: the pipeline owns the transcript the turn is cut from, so the key only asks it;
        // the screen changes when the candidate, and then its reply, reach the session.
        let (controls, commands) = Controls::for_test();
        let hub = controls.hub();
        for a in question(1, "Design a rate limiter") {
            hub.dispatch(a);
        }
        let before = screen(&hub.snapshot());
        assert!(before.contains("⏎ ask"), "the footer offers it:\n{before}");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            !press(key(enter).unwrap(), &controls),
            "Enter does not quit"
        );
        assert_eq!(commands.try_recv(), Ok(Command::Ask));
        assert!(commands.try_recv().is_err(), "one press, one command");
        assert_eq!(screen(&hub.snapshot()), before);
    }

    /// Three questions shown one after the other, the first with a focus line of its own.
    fn three_questions(hub: &dmn_session::SessionHub) {
        for (candidate, text) in [
            (1, "Design a rate limiter"),
            (2, "Design a URL shortener"),
            (3, "Design a job queue"),
        ] {
            for a in question(candidate, text) {
                hub.dispatch(a);
            }
            if candidate == 1 {
                hub.dispatch(Action::Focus {
                    candidate,
                    text: "limits, bursts".into(),
                });
            }
        }
    }

    fn status_line(screen: &str) -> &str {
        screen.lines().last().unwrap_or_default()
    }

    #[test]
    fn the_arrows_and_page_keys_map_to_browsing_and_scrolling() {
        assert_eq!(named(KeyCode::Left), Key::Earlier);
        assert_eq!(named(KeyCode::Right), Key::Later);
        assert_eq!(named(KeyCode::Up), Key::Scroll(tui::Scroll::Up));
        assert_eq!(named(KeyCode::Down), Key::Scroll(tui::Scroll::Down));
        assert_eq!(named(KeyCode::PageUp), Key::Scroll(tui::Scroll::PageUp));
        assert_eq!(named(KeyCode::PageDown), Key::Scroll(tui::Scroll::PageDown));
        let mut release = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(key(release), None);
    }

    #[test]
    fn left_twice_shows_the_first_question_and_right_twice_returns_to_live() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        three_questions(hub);
        let live = screen(&hub.snapshot());
        assert!(live.contains("LIVE QUESTION"), "{live}");
        assert!(live.contains("Design a job queue"), "{live}");
        assert!(
            status_line(&live).contains("← earlier · ⏎ ask"),
            "the footer names the key once there is an earlier question:\n{live}"
        );

        assert!(!press(named(KeyCode::Left), &controls), "← does not quit");
        let second = screen(&hub.snapshot());
        assert!(second.contains("QUESTION 2 of 3"), "{second}");
        assert!(!second.contains("LIVE QUESTION"), "{second}");
        assert!(
            second.contains("first step for Design a URL shortener"),
            "{second}"
        );
        assert!(!second.contains("job queue"), "{second}");
        assert!(
            status_line(&second).contains("← earlier · → live"),
            "{second}"
        );

        press(named(KeyCode::Left), &controls);
        let first = screen(&hub.snapshot());
        assert!(first.contains("QUESTION 1 of 3"), "{first}");
        assert!(first.contains("Design a rate limiter"), "{first}");
        assert!(first.contains("Focus: limits, bursts"), "its own:\n{first}");
        assert!(
            first.contains("first step for Design a rate limiter"),
            "{first}"
        );
        assert!(status_line(&first).contains("→ later"), "{first}");
        assert!(!status_line(&first).contains("← earlier"), "{first}");

        // Nothing is before the first question.
        press(named(KeyCode::Left), &controls);
        assert_eq!(screen(&hub.snapshot()), first);

        press(named(KeyCode::Right), &controls);
        assert_eq!(screen(&hub.snapshot()), second);
        press(named(KeyCode::Right), &controls);
        assert_eq!(screen(&hub.snapshot()), live);
        // Nor anything after the live one.
        press(named(KeyCode::Right), &controls);
        assert_eq!(screen(&hub.snapshot()), live);
    }

    #[test]
    fn a_question_behind_an_earlier_one_changes_only_the_count_until_the_user_returns() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        three_questions(hub);
        press(named(KeyCode::Left), &controls);
        let before = screen(&hub.snapshot());
        assert!(!status_line(&before).contains("new"), "{before}");

        for a in question(4, "Design a search index") {
            hub.dispatch(a);
        }
        let behind = screen(&hub.snapshot());
        assert!(status_line(&behind).contains("1 new · "), "{behind}");
        // The status line apart, the screen is what it was: `2 of 3` is now `2 of 4`.
        let above = |s: &str| {
            s.lines()
                .take(usize::from(PANE.height) - 1)
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            above(&behind),
            above(&before).replace("QUESTION 2 of 3", "QUESTION 2 of 4")
        );
        assert!(!behind.contains("search index"), "{behind}");

        press(named(KeyCode::Right), &controls);
        press(named(KeyCode::Right), &controls);
        let back = screen(&hub.snapshot());
        assert!(back.contains("LIVE QUESTION"), "{back}");
        assert!(
            back.contains("first step for Design a search index"),
            "with its Approach:\n{back}"
        );
        assert!(!status_line(&back).contains("new"), "{back}");
    }

    #[test]
    fn an_earlier_question_is_read_only_and_enter_still_asks() {
        use dmn_session::StarCards;
        let (controls, commands) = Controls::for_test();
        let hub = controls.hub();
        for a in question(1, "Design a rate limiter") {
            hub.dispatch(a);
        }
        // The live question is one `w` and `r` would act on: STAR cards from two stories.
        hub.dispatch(Action::Question {
            candidate: 2,
            text: "Tell me about a time you disagreed with a decision.".into(),
            kind: "behavioral".into(),
        });
        hub.dispatch(Action::Star {
            candidate: 2,
            cards: StarCards {
                action: "Built a two-day spike.".into(),
                ..StarCards::default()
            },
        });
        hub.dispatch(Action::Stories {
            candidate: 2,
            ranked: vec!["stories/broker.md".into(), "stories/replay.md".into()],
        });
        press(named(KeyCode::Left), &controls);
        let earlier = hub.snapshot();
        assert!(screen(&earlier).contains("QUESTION 1 of 2"));

        for c in ['r', 'w', 'p'] {
            assert!(!press(key(press_key(c)).unwrap(), &controls));
            assert!(commands.try_recv().is_err(), "{c} asks the model nothing");
            assert_eq!(hub.snapshot(), earlier, "{c} changes nothing");
        }
        // Enter is about what was just said, not about the question on screen: it asks, and
        // what it finds arrives behind the earlier question like any other.
        press(named(KeyCode::Enter), &controls);
        assert_eq!(commands.try_recv(), Ok(Command::Ask));
        assert_eq!(hub.snapshot(), earlier);

        // Back on the live question the keys act on it again.
        press(named(KeyCode::Right), &controls);
        press(Key::Swap, &controls);
        assert_eq!(commands.try_recv(), Ok(Command::Swap { candidate: 2 }));
        press(Key::Regenerate, &controls);
        assert_eq!(commands.try_recv(), Ok(Command::Regenerate));
        press(Key::Pin, &controls);
        assert!(hub.snapshot().pinned);
    }

    #[test]
    fn browsing_behind_a_pin_leaves_the_pin_and_its_queue_as_they_were() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        for a in question(1, "Design a rate limiter") {
            hub.dispatch(a);
        }
        for a in question(2, "Design a URL shortener") {
            hub.dispatch(a);
        }
        press(Key::Pin, &controls);
        press(named(KeyCode::Left), &controls);
        for a in question(3, "Design a job queue") {
            hub.dispatch(a);
        }
        let earlier = screen(&hub.snapshot());
        assert!(earlier.contains("QUESTION 1 of 2"), "{earlier}");
        assert!(!earlier.contains("PINNED"), "{earlier}");
        assert!(status_line(&earlier).contains("1 new"), "{earlier}");

        press(named(KeyCode::Right), &controls);
        let pinned = screen(&hub.snapshot());
        assert!(pinned.contains("PINNED QUESTION"), "{pinned}");
        assert!(pinned.contains("Design a URL shortener"), "{pinned}");
        let title = pinned.lines().next().unwrap_or_default();
        assert!(title.contains("1 new"), "the badge, as today:\n{pinned}");
        assert!(!status_line(&pinned).contains("new"), "{pinned}");

        press(Key::Pin, &controls);
        let unpinned = screen(&hub.snapshot());
        assert!(unpinned.contains("LIVE QUESTION"), "{unpinned}");
        assert!(unpinned.contains("Design a job queue"), "{unpinned}");
    }

    /// A coding question whose worked solution is `lines` rows of code, Deeper Answer closed.
    fn solution(hub: &dmn_session::SessionHub, lines: usize) {
        use dmn_session::{CodeBlock, SlowEnd, SlowField};
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "Reverse a linked list in place.".into(),
            kind: "coding".into(),
        });
        hub.dispatch(Action::ApproachLine {
            candidate: 1,
            index: 0,
            text: "Walk the list with three pointers.".into(),
        });
        hub.dispatch(Action::SlowStarted { candidate: 1 });
        hub.dispatch(Action::Slow {
            candidate: 1,
            field: SlowField::Solution {
                code: CodeBlock {
                    language: "python".into(),
                    code: (1..=lines)
                        .map(|n| format!("line {n:02}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            },
        });
        hub.dispatch(Action::SlowEnded {
            candidate: 1,
            end: SlowEnd::Done,
            payload: "{}".into(),
            at: 0,
        });
    }

    #[test]
    fn the_scroll_keys_take_a_tall_section_to_its_last_row_and_stop_at_both_ends() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        // A header row and 30 rows of code, in the 16 rows a 100 × 30 pane leaves the body.
        solution(hub, 30);
        press(key(press_key('1')).unwrap(), &controls);
        let top = screen(&hub.snapshot());
        assert!(top.contains("Solution · python"), "{top}");
        assert!(top.contains("line 14") && !top.contains("line 15"), "{top}");
        assert!(top.contains("↓ 16 more"), "{top}");
        assert!(
            status_line(&top).contains("↑↓ scroll · s: hints"),
            "the footer names the keys while the body is cut:\n{top}"
        );

        // Already at the top: nothing moves, and nothing is dispatched for it.
        let mut actions = hub.subscribe();
        for k in [KeyCode::Up, KeyCode::PageUp] {
            assert!(!press(named(k), &controls), "{k:?} does not quit");
        }
        assert!(actions.try_recv().is_err());
        assert_eq!(screen(&hub.snapshot()), top);

        press(named(KeyCode::Down), &controls);
        assert_eq!(hub.snapshot().scroll, 1);
        let one = screen(&hub.snapshot());
        assert!(!one.contains("Solution · python"), "{one}");
        assert!(one.contains("line 01") && one.contains("line 15"), "{one}");
        assert!(one.contains("↑ 1 · ↓ 15 more"), "{one}");

        // PgDn goes by the 15 rows on screen, which here is all that is left.
        press(named(KeyCode::PageDown), &controls);
        assert_eq!(hub.snapshot().scroll, 16);
        let end = screen(&hub.snapshot());
        assert!(
            end.contains("line 16") && end.contains("line 30") && !end.contains("line 15"),
            "{end}"
        );
        assert!(end.contains("↑ 16") && !end.contains("more"), "{end}");

        let mut actions = hub.subscribe();
        for k in [KeyCode::Down, KeyCode::PageDown] {
            press(named(k), &controls);
        }
        assert!(actions.try_recv().is_err(), "it stops at the last row");
        assert_eq!(screen(&hub.snapshot()), end);

        press(named(KeyCode::Up), &controls);
        assert_eq!(hub.snapshot().scroll, 15);
        press(named(KeyCode::PageUp), &controls);
        press(named(KeyCode::PageUp), &controls);
        assert_eq!(screen(&hub.snapshot()), top);

        // Closing the section forgets the offset: opened again, it starts from its top.
        press(named(KeyCode::PageDown), &controls);
        press(key(press_key('1')).unwrap(), &controls);
        assert_eq!(hub.snapshot().scroll, 0);
        press(key(press_key('1')).unwrap(), &controls);
        assert_eq!(screen(&hub.snapshot()), top);
    }

    #[test]
    fn the_scroll_keys_do_nothing_to_a_section_that_fits_or_with_none_open() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        solution(hub, 5);
        let mut actions = hub.subscribe();
        let closed = screen(&hub.snapshot());
        press(named(KeyCode::Down), &controls);
        assert!(actions.try_recv().is_err(), "nothing is open");
        assert_eq!(screen(&hub.snapshot()), closed);

        press(key(press_key('1')).unwrap(), &controls);
        let open = screen(&hub.snapshot());
        assert!(open.contains("line 05"), "{open}");
        assert!(
            !open.contains("more"),
            "a body that fits has no hint:\n{open}"
        );
        assert!(status_line(&open).contains("1 close · s: hints"), "{open}");
        let mut actions = hub.subscribe();
        for k in [
            KeyCode::Down,
            KeyCode::PageDown,
            KeyCode::Up,
            KeyCode::PageUp,
        ] {
            press(named(k), &controls);
        }
        assert!(actions.try_recv().is_err());
        assert_eq!(screen(&hub.snapshot()), open);
    }

    #[test]
    fn errors_show_only_as_a_red_dot_with_its_reason() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        hub.dispatch(Action::Status {
            dot: dmn_session::Dot::Model,
            health: Health::Error("connection refused".into()),
        });
        let s = screen(&hub.snapshot());
        let status_line = s.lines().last().unwrap_or_default();
        assert!(status_line.contains("model: connection refused"), "{s}");
        // Nowhere else on screen.
        assert_eq!(s.matches("connection refused").count(), 1, "{s}");
    }

    #[test]
    fn overruns_show_on_the_status_line_only_once_there_are_any() {
        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        let status_line = |s: &str| s.lines().last().unwrap_or_default().to_owned();
        assert!(!status_line(&screen(&hub.snapshot())).contains("dropped"));
        hub.dispatch(Action::Overruns { total: 3 });
        let s = screen(&hub.snapshot());
        assert!(status_line(&s).contains("3 dropped"), "{s}");
    }

    fn args(mic: Option<&str>, app: Option<&str>) -> Args {
        Args {
            replay: None,
            mic: mic.map(str::to_owned),
            app: app.map(str::to_owned),
            model: None,
            threads: None,
            vad_close_ms: None,
            vad: None,
            device: None,
            language: None,
            no_store: true,
            headless: false,
            duration: None,
            solutions: None,
            no_assist: false,
            question_source: None,
        }
    }

    #[test]
    fn headless_without_a_replay_needs_a_duration() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            &Config::default(),
            &Paths::under(dir.path()),
            &Args {
                headless: true,
                ..args(None, None)
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--duration"), "{err}");
    }

    #[test]
    fn swap_cycles_the_star_story_and_back_and_with_one_story_says_there_is_no_other() {
        use crate::pipeline::Command;
        use dmn_session::StarCards;
        assert_eq!(key(press_key('w')), Some(Key::Swap));

        let behavioral = |hub: &dmn_session::SessionHub, ranked: &[&str]| {
            hub.dispatch(Action::Question {
                candidate: 1,
                text: "Tell me about a time you disagreed with a technical decision.".into(),
                kind: "behavioral".into(),
            });
            hub.dispatch(Action::ApproachLine {
                candidate: 1,
                index: 0,
                text: "Set the scene.".into(),
            });
            hub.dispatch(Action::Star {
                candidate: 1,
                cards: StarCards {
                    situation: "We were choosing a broker.".into(),
                    task: "Make the case without an argument.".into(),
                    action: "Built a two-day spike and took it to the lead first.".into(),
                    result: "Shipped on time; replay saved an incident.".into(),
                    story: Some(ranked[0].into()),
                },
            });
            hub.dispatch(Action::Answered {
                candidate: 1,
                clarification: false,
                payload: "{}".into(),
                at: 0,
            });
            hub.dispatch(Action::Stories {
                candidate: 1,
                ranked: ranked.iter().map(|s| (*s).into()).collect(),
            });
        };

        let (controls, commands) = Controls::for_test();
        let hub = controls.hub();
        let ranked = [
            "stories/disagreed-on-broker.md",
            "stories/kafka-replay-incident.md",
            "stories/security-review-influence.md",
        ];
        behavioral(hub, &ranked);
        let first = screen(&hub.snapshot());
        assert!(
            first.contains("STAR · stories/disagreed-on-broker.md 1/3"),
            "the STAR block replaces the Approach and names its story:\n{first}"
        );
        assert!(!first.contains("Set the scene."), "{first}");
        assert!(first.contains("w swap"), "the footer offers Swap:\n{first}");

        for (n, want) in [(2, ranked[1]), (3, ranked[2]), (1, ranked[0])] {
            assert!(!press(Key::Swap, &controls));
            assert_eq!(commands.try_recv(), Ok(Command::Swap { candidate: 1 }));
            let s = screen(&hub.snapshot());
            assert!(s.contains(&format!("STAR · {want} {n}/3")), "{s}");
        }

        let (controls, commands) = Controls::for_test();
        let hub = controls.hub();
        behavioral(hub, &["stories/disagreed-on-broker.md"]);
        let one = screen(&hub.snapshot());
        assert!(
            one.contains("STAR · stories/disagreed-on-broker.md "),
            "{one}"
        );
        assert!(!one.contains("w swap"), "nothing to swap to:\n{one}");
        assert!(!press(Key::Swap, &controls));
        assert!(
            commands.try_recv().is_err(),
            "nothing is asked of the model"
        );
        let after = screen(&hub.snapshot());
        assert!(after.contains("no other story"), "{after}");
    }

    #[test]
    fn keys_1_to_6_open_one_section_at_a_time_on_screen() {
        use dmn_session::{DeeperItem, SlowEnd, SlowField};
        assert_eq!(key(press_key('1')), Some(Key::Section(Section::Deeper)));
        assert_eq!(
            key(press_key('6')),
            Some(Key::Section(Section::ResumeContext))
        );
        assert_eq!(key(press_key('7')), None);
        assert_eq!(key(press_key('0')), None);

        let (controls, _commands) = Controls::for_test();
        let hub = controls.hub();
        hub.dispatch(Action::Question {
            candidate: 1,
            text: "How would you design a cache?".into(),
            kind: "system_design".into(),
        });
        hub.dispatch(Action::ApproachLine {
            candidate: 1,
            index: 0,
            text: "Pick an eviction policy.".into(),
        });
        hub.dispatch(Action::SlowStarted { candidate: 1 });
        for field in [
            SlowField::Deeper {
                items: vec![DeeperItem {
                    label: "Eviction".into(),
                    text: "LRU with a sampled clock hand.".into(),
                }],
            },
            SlowField::Followups {
                items: vec!["What about TTLs?".into()],
            },
        ] {
            hub.dispatch(Action::Slow {
                candidate: 1,
                field,
            });
        }
        hub.dispatch(Action::SlowEnded {
            candidate: 1,
            end: SlowEnd::Done,
            payload: "{}".into(),
            at: 0,
        });
        let before = screen(&hub.snapshot());
        assert!(before.contains("1 ▸ Deeper Answer"), "{before}");
        assert!(
            !before.contains("LRU with a sampled"),
            "nothing is open until a key is pressed:\n{before}"
        );

        assert!(!press(key(press_key('1')).unwrap(), &controls));
        let one = screen(&hub.snapshot());
        assert!(one.contains("1 ▾ Deeper Answer"), "{one}");
        assert!(
            one.contains("Eviction: LRU with a sampled clock hand."),
            "{one}"
        );
        assert!(
            one.contains("1 close"),
            "the footer offers to close it:\n{one}"
        );

        press(key(press_key('2')).unwrap(), &controls);
        let two = screen(&hub.snapshot());
        assert!(two.contains("• What about TTLs?"), "{two}");
        assert!(
            !two.contains("LRU with a sampled"),
            "opening the second closed the first:\n{two}"
        );

        press(key(press_key('3')).unwrap(), &controls);
        assert_eq!(
            hub.snapshot().live.and_then(|q| q.open),
            Some(Section::Followups),
            "3 is Tradeoffs, which had nothing and is hidden: the key does nothing"
        );

        press(key(press_key('2')).unwrap(), &controls);
        let closed = screen(&hub.snapshot());
        assert!(!closed.contains("• What about TTLs?"), "{closed}");
        assert!(closed.contains("1–6 more"), "{closed}");
    }

    #[test]
    fn live_capture_follows_the_config_and_the_flags_override_it() {
        let cfg = Config::default();
        assert_eq!(
            live_input(&cfg, &args(None, None)).unwrap(),
            Input::Live {
                mic: CaptureTarget::Mic(None),
                interviewer: CaptureTarget::interviewer(&cfg.audio.app_stream),
            }
        );
        assert_eq!(
            live_input(&cfg, &args(Some("alsa_input.usb-mic"), Some(""))).unwrap(),
            Input::Live {
                mic: CaptureTarget::Mic(Some("alsa_input.usb-mic".into())),
                interviewer: CaptureTarget::SinkMonitor,
            },
            "--app '' captures all system audio"
        );
        let mut no_system = Config::default();
        no_system.audio.system_audio = false;
        let err = live_input(&no_system, &args(None, None)).unwrap_err();
        assert!(err.contains("--app"), "{err}");
        assert!(live_input(&no_system, &args(None, Some("chrome"))).is_ok());
    }
}
