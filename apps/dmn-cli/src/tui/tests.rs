//! Snapshot tests: each `testdata/session-*.json` fixture rendered at 100 × 30 and 80 × 24 into a
//! `TestBackend` and compared cell for cell (text) with `src/tui/snapshots/*.txt`, plus style
//! checks where the style carries meaning (the dimmed Approach, the pinned badge, error dots).
//!
//! The fixtures are not hand-written JSON: each is the `Session` that `dmn-session`'s own reducer
//! builds from an action list below, and `fixtures_are_what_the_reducer_builds` fails if a file
//! and its actions ever disagree, so a change to the `Session` type cannot leave them stale.
//!
//! After an intended change, regenerate with
//! `DMN_UPDATE_SNAPSHOTS=1 cargo test -p dmn-cli tui` and review the diff.

use std::path::{Path, PathBuf};

use dmn_session::{Action, Dot, Health, Session, Speaker, StarCards};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::draw;

const STATES: [&str; 5] = ["listening", "live", "pinned", "loading", "behavioral"];
const SIZES: [(u16, u16); 2] = [(100, 30), (80, 24)];

fn updating() -> bool {
    std::env::var_os("DMN_UPDATE_SNAPSHOTS").is_some()
}

fn fixture_path(state: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(format!("session-{state}.json"))
}

fn snapshot_path(state: &str, (w, h): (u16, u16)) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tui/snapshots")
        .join(format!("{state}-{w}x{h}.txt"))
}

// --- Action lists behind the fixtures --------------------------------------------------------

fn seg(speaker: Speaker, start: f64, end: f64, text: &str, is_final: bool) -> Action {
    Action::Segment {
        speaker,
        start,
        end,
        text: text.into(),
        is_final,
        avg_logprob: -0.2,
    }
}

fn status_ok() -> Vec<Action> {
    [Dot::Mic, Dot::System, Dot::Model]
        .into_iter()
        .map(|dot| Action::Status {
            dot,
            health: Health::Ok,
        })
        .collect()
}

fn opening() -> Vec<Action> {
    let mut a = vec![Action::Started {
        label: "sysdesign mock".into(),
        at: 1_790_200_000,
    }];
    a.extend(status_ok());
    a.extend([
        seg(
            Speaker::Interviewer,
            1.0,
            13.0,
            "Hi, thanks for making the time today. This session is about an hour of system design.",
            true,
        ),
        seg(Speaker::You, 16.2, 17.2, "Sounds great.", true),
    ]);
    a
}

/// Candidate `id` answered in full: question, focus, approach lines, done.
fn answered(id: u64, question: &str, kind: &str, focus: &str, approach: &[&str]) -> Vec<Action> {
    let mut a = vec![
        Action::Candidate {
            id,
            speaker: Speaker::Interviewer,
            start: 20.0,
            end: 29.7,
            text: question.into(),
            score: 7,
        },
        Action::Question {
            candidate: id,
            text: question.into(),
            kind: kind.into(),
        },
        Action::Focus {
            candidate: id,
            text: focus.into(),
        },
    ];
    a.extend(
        approach
            .iter()
            .enumerate()
            .map(|(index, t)| Action::ApproachLine {
                candidate: id,
                index,
                text: (*t).into(),
            }),
    );
    a.push(Action::Answered {
        candidate: id,
        clarification: false,
        payload: "{}".into(),
        at: 1_790_200_031,
    });
    a
}

fn rate_limiter() -> Vec<Action> {
    answered(
        1,
        "How would you design a rate limiter for a globally distributed API, and how would you \
         handle the case where the limiter itself goes down?",
        "system_design",
        "architecture, consistency tradeoffs, distributed state, failure handling",
        &[
            "Clarify scope: per user, per API key or per IP, and the limits per tier.",
            "Use a token bucket per key; sliding-window counters if bursts matter.",
            "Keep counters in regional Redis with a local in-process cache in front.",
            "Accept small over-admission across regions; reconcile quotas asynchronously.",
            "If Redis is down, fail open to a conservative local limit and alert.",
        ],
    )
}

fn actions(state: &str) -> Vec<Action> {
    let mut a = opening();
    match state {
        "listening" => {
            a.push(seg(
                Speaker::Interviewer,
                18.0,
                21.5,
                "Great. Let me tell you a bit about the team before we start.",
                true,
            ));
            a.push(seg(
                Speaker::Interviewer,
                22.0,
                24.0,
                "We run the platform",
                false,
            ));
        }
        "live" => {
            a.extend(rate_limiter());
            a.push(seg(
                Speaker::You,
                31.0,
                36.0,
                "Okay, so first I'd want to know what we're limiting on.",
                true,
            ));
            a.push(seg(Speaker::You, 36.5, 38.0, "Per user or per", false));
        }
        "pinned" => {
            a.extend(rate_limiter());
            a.push(Action::Pin);
            a.extend(answered(
                2,
                "Where would you put the limiter in the request path?",
                "system_design",
                "gateway, edge, CDN",
                &["Enforce at the gateway, behind the CDN."],
            ));
            a.push(seg(
                Speaker::You,
                118.0,
                134.0,
                "Per-region quotas, reconciled asynchronously, so a partition only over-admits.",
                true,
            ));
            a.push(seg(
                Speaker::Interviewer,
                136.5,
                144.2,
                "Where would you put the limiter in the request path?",
                true,
            ));
        }
        "loading" => {
            a.extend(rate_limiter());
            a.push(seg(
                Speaker::You,
                80.0,
                93.5,
                "Redis in each region, with a small local cache to absorb bursts before it.",
                true,
            ));
            a.push(seg(
                Speaker::Interviewer,
                94.7,
                100.4,
                "And what if it goes down for a whole region, not just one node?",
                true,
            ));
            // The next candidate is out to the model: the rate-limiter Approach dims.
            a.push(Action::Candidate {
                id: 2,
                speaker: Speaker::Interviewer,
                start: 94.7,
                end: 100.4,
                text: "And what if it goes down for a whole region, not just one node?".into(),
                score: 5,
            });
        }
        "behavioral" => {
            a.push(Action::Candidate {
                id: 1,
                speaker: Speaker::Interviewer,
                start: 40.0,
                end: 44.0,
                text: "Tell me about a time you disagreed with a teammate.".into(),
                score: 6,
            });
            a.push(Action::Question {
                candidate: 1,
                text: "Tell me about a time you disagreed with a teammate.".into(),
                kind: "behavioral".into(),
            });
            a.push(Action::Star {
                candidate: 1,
                cards: StarCards {
                    situation: "Payments team, 2024: a rushed schema migration before a launch."
                        .into(),
                    task: "I owned the rollout plan and had to get sign-off from the lead.".into(),
                    action: "Wrote a one-page risk comparison and proposed a dual-write step."
                        .into(),
                    result: "Shipped a week later with zero incidents; dual-write became standard."
                        .into(),
                    story: Some("payments-migration".into()),
                },
            });
            a.push(Action::Answered {
                candidate: 1,
                clarification: false,
                payload: "{}".into(),
                at: 1_790_200_050,
            });
            a.push(Action::Clarification {
                candidate: 2,
                text: "And how did the teammate take it afterwards?".into(),
            });
            a.push(Action::Status {
                dot: Dot::Model,
                health: Health::Error("ollama unreachable".into()),
            });
            a.push(Action::Status {
                dot: Dot::Mic,
                health: Health::Muted,
            });
        }
        other => panic!("no actions for state {other}"),
    }
    a
}

fn build(state: &str) -> Session {
    Session::replay(&actions(state))
}

fn load(state: &str) -> Session {
    // While regenerating, tests run in parallel with the one rewriting the files, so they use
    // the freshly built session instead of reading a half-written fixture.
    if updating() {
        return build(state);
    }
    let text = std::fs::read_to_string(fixture_path(state)).expect("fixture file");
    serde_json::from_str(&text).expect("fixture deserialises into a Session")
}

// --- Rendering --------------------------------------------------------------------------------

fn render(session: &Session, (w, h): (u16, u16)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    terminal.draw(|frame| draw(frame, session)).expect("draw");
    terminal.backend().buffer().clone()
}

/// The buffer's symbols, one line per row, trailing spaces trimmed.
fn text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// Cells of the row holding `needle`, from its first character to its end.
fn row_containing(buf: &Buffer, needle: &str) -> (u16, u16) {
    let rows = text(buf);
    for (y, row) in rows.lines().enumerate() {
        if let Some(byte) = row.find(needle) {
            let x = row[..byte].chars().count();
            return (
                u16::try_from(x).expect("x fits"),
                u16::try_from(y).expect("y fits"),
            );
        }
    }
    panic!("{needle:?} not on screen:\n{rows}");
}

// --- Tests ------------------------------------------------------------------------------------

#[test]
fn fixtures_are_what_the_reducer_builds() {
    for state in STATES {
        let built = build(state);
        if updating() {
            let json = serde_json::to_string_pretty(&built).expect("serialise");
            std::fs::write(fixture_path(state), json + "\n").expect("write fixture");
        }
        assert_eq!(load(state), built, "testdata/session-{state}.json is stale");
    }
}

#[test]
fn tui_snapshots_match() {
    let mut failures = Vec::new();
    for state in STATES {
        let session = load(state);
        for size in SIZES {
            let got = text(&render(&session, size));
            let path = snapshot_path(state, size);
            if updating() {
                std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
                std::fs::write(&path, &got).expect("write snapshot");
                continue;
            }
            let want = std::fs::read_to_string(&path).unwrap_or_default();
            if got != want {
                failures.push(format!("--- {} ---\n{got}", path.display()));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "snapshots differ (DMN_UPDATE_SNAPSHOTS=1 to accept):\n{}",
        failures.join("\n")
    );
}

#[test]
fn tui_listening_shows_the_last_interviewer_line_muted() {
    let buf = render(&load("listening"), (100, 30));
    let screen = text(&buf);
    assert!(screen.contains("LISTENING"), "{screen}");
    let (x, y) = row_containing(&buf, "Great. Let me tell you");
    assert_eq!(buf[(x, y)].fg, Color::DarkGray);
    assert!(
        screen.contains("We run the platform▍"),
        "partial with cursor:\n{screen}"
    );
}

#[test]
fn tui_live_question_is_labelled_teal_and_its_approach_is_not_dimmed() {
    let buf = render(&load("live"), (100, 30));
    let (x, y) = row_containing(&buf, "LIVE QUESTION");
    assert_eq!(buf[(x, y)].fg, Color::Cyan);
    let (x, y) = row_containing(&buf, "Use a token bucket");
    assert!(!buf[(x, y)].modifier.contains(Modifier::DIM));
    assert!(text(&buf).contains("p pin · r redo"));
}

#[test]
fn tui_pinned_shows_the_amber_label_and_the_new_badge() {
    let buf = render(&load("pinned"), (100, 30));
    let (x, y) = row_containing(&buf, "PINNED QUESTION");
    assert_eq!(buf[(x, y)].fg, Color::Yellow);
    let (x, y) = row_containing(&buf, "1 new");
    assert_eq!(buf[(x, y)].bg, Color::Yellow);
    let screen = text(&buf);
    assert!(
        screen.contains("rate limiter"),
        "the pinned question stays:\n{screen}"
    );
    assert!(
        !screen.contains("Enforce at the gateway"),
        "the queued reply does not replace it:\n{screen}"
    );
}

#[test]
fn tui_a_long_question_is_cut_to_three_lines_not_grown() {
    let mut session = load("live");
    if let Some(q) = session.live.as_mut() {
        q.text = "Suppose the rate limiter sits in front of every region. ".repeat(12);
        q.focus.clear();
    }
    let screen = text(&render(&session, (80, 24)));
    let block: Vec<&str> = screen.lines().skip(1).take(4).collect();
    let question_rows = block
        .iter()
        .filter(|r| !r.trim_matches(['│', ' ']).is_empty())
        .count();
    assert_eq!(question_rows, 3, "{screen}");
    assert!(
        block[2].trim_end_matches(['│', ' ']).ends_with('…'),
        "{screen}"
    );
    assert!(
        screen.contains("APPROACH"),
        "the layout did not move:\n{screen}"
    );
}

#[test]
fn tui_loading_dims_the_previous_approach_instead_of_clearing_it() {
    let buf = render(&load("loading"), (100, 30));
    for line in ["Use a token bucket", "If Redis is down"] {
        let (x, y) = row_containing(&buf, line);
        assert!(
            buf[(x, y)].modifier.contains(Modifier::DIM),
            "{line:?} should be dimmed"
        );
    }
    assert!(text(&buf).contains("thinking…"));
}

#[test]
fn tui_behavioral_shows_star_cards_a_clarification_and_error_dots() {
    let buf = render(&load("behavioral"), (100, 30));
    let screen = text(&buf);
    assert!(screen.contains(" STAR "), "{screen}");
    let (x, y) = row_containing(&buf, "Wrote a one-page");
    assert!(
        buf[(x, y)].modifier.contains(Modifier::BOLD),
        "Action card emphasised"
    );
    assert!(
        screen.contains("↳ And how did the teammate take it"),
        "{screen}"
    );
    let (x, y) = row_containing(&buf, "● model");
    assert_eq!(buf[(x, y)].fg, Color::Red);
    assert!(screen.contains("model: ollama unreachable"), "{screen}");
    assert!(screen.contains("mic muted"), "{screen}");
}

#[test]
fn tui_nothing_overflows_its_width_at_either_size() {
    for state in STATES {
        for size in SIZES {
            let screen = text(&render(&load(state), size));
            assert_eq!(screen.lines().count(), usize::from(size.1));
            for row in screen.lines() {
                assert!(
                    super::text::width(row) <= usize::from(size.0),
                    "{state} {size:?}: {row}"
                );
            }
        }
    }
}
