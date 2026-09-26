//! `dmn run`: the copilot in a terminal beside the call (SPEC § UI, Terminal MVP).
//!
//! The pipeline ([`crate::pipeline`], the same one `dmn replay` runs) works on its own thread
//! and dispatches everything into the session; this thread redraws the TUI from the session's
//! `watch` channel whenever it changes and turns keys into [`Controls`] calls: `p` pin / unpin,
//! `r` regenerate, `m` mute the mic, `s` coding solutions hints ↔ full (#50), `q` (or Ctrl-C)
//! quit. Nothing else writes to the terminal
//! while the TUI is up: the fast phase is [`Output::Quiet`], warnings go to the log file, and
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
    pub no_store: bool,
    pub headless: bool,
    /// Live: stop after this many seconds, as if `q` had been pressed.
    pub duration: Option<u64>,
    /// `--solutions <hints|full>`: coding solutions for this run, over `[ui].coding_solutions`.
    pub solutions: Option<CodingSolutions>,
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
    Quit,
}

/// Map a terminal key event to a [`Key`]. Only presses count (terminals that report releases
/// would otherwise act twice).
pub fn key(ev: KeyEvent) -> Option<Key> {
    if ev.kind != KeyEventKind::Press {
        return None;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        return (ev.code == KeyCode::Char('c')).then_some(Key::Quit);
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
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Key::Quit),
        _ => None,
    }
}

/// Act on one key. Returns `true` when the TUI should close.
pub fn press(k: Key, controls: &Controls) -> bool {
    match k {
        Key::Pin => controls.toggle_pin(),
        Key::Regenerate => controls.regenerate(),
        Key::Mute => controls.toggle_mute(),
        Key::Solutions => controls.toggle_coding_solutions(),
        Key::Section(s) => controls.toggle_section(s),
        Key::Swap => controls.swap_story(),
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
            partials: Partials::Auto,
            spec_literal_detector: false,
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
                    if press(k, controls) {
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
}

impl Observer for Emitted {
    fn candidate(&mut self, id: u64, found: &Found) {
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
    use dmn_session::{Action, Health};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn screen(session: &Session) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
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
            no_store: true,
            headless: false,
            duration: None,
            solutions: None,
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
