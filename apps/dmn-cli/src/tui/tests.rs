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

use dmn_session::{
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, Dot, Health, Section, Session,
    SlowEnd, SlowField, Speaker, StarCards, TradeoffItem,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::draw;

const STATES: [&str; 20] = [
    "listening",
    "live",
    "pinned",
    "loading",
    "behavioral",
    "coding",
    "coding-full",
    // #56: the six sections under the Approach.
    "sections",
    "sections-open-1",
    "sections-open-2",
    "sections-open-3",
    "sections-open-4",
    "sections-open-5",
    "sections-open-6",
    "sections-loading",
    "sections-slow",
    "behavioral-sections",
    "coding-full-open",
    // #58: STAR built from a matched story, and Swap with no other story.
    "star-story",
    "star-no-other",
];
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
        language: None,
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

fn slow(candidate: u64, field: SlowField) -> Action {
    Action::Slow { candidate, field }
}

fn slow_ended(candidate: u64, end: SlowEnd) -> Action {
    Action::SlowEnded {
        candidate,
        end,
        payload: "{}".into(),
        at: 1_790_200_040,
    }
}

fn deeper_items() -> SlowField {
    SlowField::Deeper {
        items: vec![
            DeeperItem {
                label: "Algorithm".into(),
                text: "Token bucket per key: refill r tokens a second up to a burst b, and each \
                       request takes one. Sliding-window counters are exact but cost a sorted set \
                       per key."
                    .into(),
            },
            DeeperItem {
                label: "State".into(),
                text: "Counters live in regional Redis with INCR and a TTL; a small in-process \
                       cache absorbs hot keys and syncs every 100 ms."
                    .into(),
            },
            DeeperItem {
                label: "Failure".into(),
                text: "If Redis is unreachable, fail open to a conservative local limit per node \
                       and alert."
                    .into(),
            },
        ],
    }
}

fn followups() -> SlowField {
    SlowField::Followups {
        items: vec![
            "How would you rate-limit a key whose traffic spans regions?".into(),
            "What happens to in-flight requests when a tier's limit changes?".into(),
            "How would you test the limiter under a network partition?".into(),
        ],
    }
}

fn tradeoffs() -> SlowField {
    SlowField::Tradeoffs {
        items: vec![
            TradeoffItem {
                option: "Central Redis".into(),
                pro: "exact counts".into(),
                con: "a network hop per request and a single point of failure".into(),
            },
            TradeoffItem {
                option: "Local counters".into(),
                pro: "no hop, survives partitions".into(),
                con: "over-admits by up to N× across N nodes".into(),
            },
        ],
    }
}

fn example() -> SlowField {
    SlowField::Example {
        code: Some(CodeBlock {
            language: "python".into(),
            code: "def allow(key, now):\n    b = buckets[key]\n    b.tokens = min(b.burst, \
                   b.tokens + (now - b.last) * b.rate)\n    b.last = now\n    if b.tokens < 1:\n        \
                   return False\n    b.tokens -= 1\n    return True"
                .into(),
        }),
    }
}

fn mention() -> SlowField {
    SlowField::Mention {
        items: vec![
            "You promised to come back to clock skew between regions.".into(),
            "Return 429 with a Retry-After header.".into(),
        ],
    }
}

fn resume_context() -> SlowField {
    SlowField::ResumeContext {
        items: vec![Citation {
            source: "resume.md".into(),
            text: "Built the API gateway rate limiter at Acme (Envoy + Redis), 40k requests a \
                   second."
                .into(),
        }],
    }
}

/// The rate-limiter question with its whole slow phase in: all six sections filled, none open.
fn sections() -> Vec<Action> {
    let mut a = rate_limiter();
    a.push(Action::SlowStarted { candidate: 1 });
    a.extend(
        [
            deeper_items(),
            followups(),
            tradeoffs(),
            example(),
            mention(),
            resume_context(),
        ]
        .into_iter()
        .map(|f| slow(1, f)),
    );
    a.push(slow_ended(1, SlowEnd::Done));
    a.push(seg(
        Speaker::You,
        31.0,
        36.0,
        "Okay, so first I'd want to know what we're limiting on.",
        true,
    ));
    a
}

/// A behavioral question answered with STAR cards, then a clarification under it.
fn behavioral_star() -> Vec<Action> {
    vec![
        Action::Candidate {
            id: 1,
            speaker: Speaker::Interviewer,
            start: 40.0,
            end: 44.0,
            text: "Tell me about a time you disagreed with a teammate.".into(),
            score: 6,
        },
        Action::Question {
            candidate: 1,
            text: "Tell me about a time you disagreed with a teammate.".into(),
            kind: "behavioral".into(),
        },
        Action::Star {
            candidate: 1,
            cards: StarCards {
                situation: "Payments team, 2024: a rushed schema migration before a launch.".into(),
                task: "I owned the rollout plan and had to get sign-off from the lead.".into(),
                action: "Wrote a one-page risk comparison and proposed a dual-write step.".into(),
                result: "Shipped a week later with zero incidents; dual-write became standard."
                    .into(),
                story: Some("payments-migration".into()),
            },
        },
        Action::Answered {
            candidate: 1,
            clarification: false,
            payload: "{}".into(),
            at: 1_790_200_050,
        },
        Action::Clarification {
            candidate: 2,
            text: "And how did the teammate take it afterwards?".into(),
        },
    ]
}

/// A behavioral question as the fast phase answers it with a story matched (#58): an Approach
/// (which the STAR block stands in for), the cards naming the story, and the ranking.
fn story_star(ranked: &[&str]) -> Vec<Action> {
    let q = "Tell me about a time you disagreed with a technical decision and how you handled it.";
    let mut a = answered(
        1,
        q,
        "behavioral",
        "conflict handling, evidence, relationships",
        &[
            "Situation: choosing the pipeline broker.",
            "Task: make the case without a public fight.",
            "Action: two-day spike, one-on-one with the lead.",
            "Result: shipped on time; replay saved an incident.",
        ],
    );
    // The cards arrive before `Answered`, as in the fast phase.
    let at = a.len() - 1;
    a.insert(
        at,
        Action::Star {
            candidate: 1,
            cards: StarCards {
                situation: "Choosing a broker for the shipment event pipeline at Contoso.".into(),
                task: "Argue for replay and fan-out without turning a meeting into a fight.".into(),
                action: "Built a two-day spike on real events and took it to the lead one-on-one \
                         first."
                    .into(),
                result: "Shipped on time; a year later replay recovered six hours of events."
                    .into(),
                story: Some(ranked[0].into()),
            },
        },
    );
    a.push(Action::Stories {
        candidate: 1,
        ranked: ranked.iter().map(|s| (*s).into()).collect(),
    });
    a
}

/// #50's coding question, answered.
fn median_question() -> Vec<Action> {
    let mut a = answered(
        1,
        "Given a stream of integers, return the median after each one arrives.",
        "coding",
        "two heaps, balancing, O(log n) insert",
        &[
            "Keep a max-heap of the lower half and a min-heap of the upper half.",
            "Insert into one heap, then move its top across so the sizes differ by ≤ 1.",
            "The median is the larger heap's top, or the mean of both tops.",
            "Each insert is O(log n); reading the median is O(1).",
        ],
    );
    a.push(seg(
        Speaker::You,
        31.0,
        36.0,
        "So I'd split the numbers into two halves around the median.",
        true,
    ));
    a
}

fn actions(state: &str) -> Vec<Action> {
    let mut a = opening();
    // #56: every section filled, then the key for section N pressed.
    if let Some(n) = state.strip_prefix("sections-open-") {
        let n: usize = n.parse().expect("section number");
        a.extend(sections());
        a.push(Action::ToggleSection {
            section: Section::ALL[n - 1],
        });
        return a;
    }
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
            a.extend(behavioral_star());
            a.push(Action::Status {
                dot: Dot::Model,
                health: Health::Error("ollama unreachable".into()),
            });
            a.push(Action::Status {
                dot: Dot::Mic,
                health: Health::Muted,
            });
        }
        // #58: the fast phase's Approach and STAR cards from a matched story; three matched, so
        // the header counts them and the footer offers Swap.
        "star-story" | "star-no-other" => {
            let ranked: &[&str] = if state == "star-story" {
                &[
                    "stories/disagreed-on-broker.md",
                    "stories/kafka-replay-incident.md",
                    "stories/security-review-influence.md",
                ]
            } else {
                &["stories/disagreed-on-broker.md"]
            };
            a.extend(story_star(ranked));
            if state == "star-no-other" {
                a.push(Action::SwapStory { candidate: 1 });
            }
        }
        // #50: a coding question under each coding-solutions mode; the footer names the mode.
        "coding" | "coding-full" => {
            let mode = if state == "coding" {
                CodingSolutions::Hints
            } else {
                CodingSolutions::Full
            };
            a.push(Action::CodingSolutions { mode });
            a.extend(median_question());
        }
        // #56: all six sections filled, none open.
        "sections" => a.extend(sections()),
        // The slow phase is under way: two sections are in, the rest show their "…" hint.
        "sections-loading" => {
            a.extend(rate_limiter());
            a.push(Action::SlowStarted { candidate: 1 });
            a.push(slow(1, deeper_items()));
            a.push(slow(1, followups()));
        }
        // Timed out: two sections in, Tradeoffs empty (hidden), the other three tagged "slow".
        "sections-slow" => {
            a.extend(rate_limiter());
            a.push(Action::SlowStarted { candidate: 1 });
            a.push(slow(1, deeper_items()));
            a.push(slow(1, followups()));
            a.push(slow(1, SlowField::Tradeoffs { items: vec![] }));
            a.push(slow_ended(1, SlowEnd::TimedOut));
        }
        // STAR cards in place of the Approach, sections under them, Follow-up Questions open.
        "behavioral-sections" => {
            a.extend(behavioral_star());
            a.push(Action::SlowStarted { candidate: 1 });
            a.push(slow(
                1,
                SlowField::Followups {
                    items: vec![
                        "What would you do differently next time?".into(),
                        "How did the teammate respond afterwards?".into(),
                    ],
                },
            ));
            a.push(slow(1, SlowField::Tradeoffs { items: vec![] }));
            a.push(slow(1, SlowField::Example { code: None }));
            a.push(slow(1, mention()));
            a.push(slow_ended(1, SlowEnd::Done));
            a.push(Action::ToggleSection {
                section: Section::Followups,
            });
        }
        // A coding question in full mode with Deeper Answer open: hints, then the solution.
        "coding-full-open" => {
            a.push(Action::CodingSolutions {
                mode: CodingSolutions::Full,
            });
            a.extend(median_question());
            a.push(Action::SlowStarted { candidate: 1 });
            a.push(slow(
                1,
                SlowField::Deeper {
                    items: vec![DeeperItem {
                        label: "Insight".into(),
                        text: "Balance two heaps so the median is always at a top: O(log n) per \
                               insert, O(1) to read."
                            .into(),
                    }],
                },
            ));
            a.push(slow(
                1,
                SlowField::Solution {
                    code: CodeBlock {
                        language: "python".into(),
                        code: "import heapq\n\nlo, hi = [], []  # max-heap (negated), min-heap\n\n\
                               def add(x):\n    heapq.heappush(lo, -x)\n    \
                               heapq.heappush(hi, -heapq.heappop(lo))\n    \
                               if len(hi) > len(lo):\n        \
                               heapq.heappush(lo, -heapq.heappop(hi))\n\ndef median():\n    \
                               return -lo[0] if len(lo) > len(hi) else (hi[0] - lo[0]) / 2"
                            .into(),
                    },
                },
            ));
            a.push(slow_ended(1, SlowEnd::Done));
            a.push(Action::ToggleSection {
                section: Section::Deeper,
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
fn tui_the_status_line_names_a_detected_language_other_than_english() {
    // #77: the last final's language, once whisper reports one; partials never set it.
    let heard = |language: Option<&str>, is_final: bool| {
        let mut actions = actions("listening");
        actions.push(Action::Segment {
            speaker: Speaker::Interviewer,
            start: 1.0,
            end: 3.0,
            text: "How would you design it?".into(),
            is_final,
            avg_logprob: -0.2,
            language: language.map(str::to_owned),
        });
        text(&render(&Session::replay(&actions), (100, 30)))
    };
    assert!(
        heard(Some("es"), true).contains(" es→en "),
        "{}",
        heard(Some("es"), true)
    );
    assert!(
        !heard(Some("en"), true).contains("→en"),
        "English is not tagged"
    );
    assert!(
        !heard(None, true).contains("→en"),
        "the English-only path shows nothing"
    );
    assert!(
        !heard(Some("de"), false).contains("→en"),
        "a partial's guess is not shown"
    );
}

#[test]
fn tui_the_status_line_says_which_device_transcribes() {
    let with = |device: &str, fallback: bool| {
        let mut actions = actions("listening");
        actions.push(Action::Transcriber {
            device: device.into(),
            fallback,
        });
        render(&Session::replay(&actions), (100, 30))
    };
    let gpu = text(&with(
        "GPU NVIDIA GeForce RTX 4090 (Vulkan0, 1.9 of 25.8 GB free)",
        false,
    ));
    assert!(gpu.contains(" stt gpu "), "{gpu}");
    let cpu = text(&with("CPU", false));
    assert!(
        cpu.contains(" stt cpu ") && !cpu.contains("no gpu"),
        "{cpu}"
    );

    let buf = with("CPU (fallback: Vulkan0 failed: out of device memory)", true);
    let (x, y) = row_containing(&buf, "stt cpu (no gpu)");
    assert_eq!(buf[(x, y)].fg, Color::Yellow, "a fallback is amber");

    // Beside a local LLM the usual split: the interviewer's lane on the GPU, yours on the CPU.
    let mixed = text(&with(
        "GPU NVIDIA GeForce RTX 4090 (Vulkan0, 1.2 of 26.0 GB free); CPU (fallback: Vulkan0 has \
         0.33 GB free, whisper needs 1.16 GB)",
        true,
    ));
    assert!(
        mixed.contains(" stt gpu+cpu ") && !mixed.contains("no gpu"),
        "{mixed}"
    );

    let none = text(&render(&load("listening"), (100, 30)));
    assert!(
        !none.contains(" stt "),
        "nothing before the models load: {none}"
    );
}

#[test]
fn tui_the_sink_monitor_fallback_shows_an_all_audio_badge() {
    let mut actions = actions("listening");
    actions.push(Action::Status {
        dot: Dot::System,
        health: Health::AllAudio,
    });
    let buf = render(&Session::replay(&actions), (100, 30));
    let screen = text(&buf);
    assert!(screen.contains("● system: all audio"), "{screen}");
    let (x, y) = row_containing(&buf, "● system: all audio");
    assert_eq!(
        buf[(x, y)].fg,
        Color::Yellow,
        "amber, not the green of an app stream"
    );
}

#[test]
fn tui_no_rows_without_a_slow_phase() {
    for state in ["live", "coding", "behavioral"] {
        let screen = text(&render(&load(state), (100, 30)));
        assert!(!screen.contains("Deeper Answer"), "{state}:\n{screen}");
        assert!(!screen.contains("1–6"), "{state}:\n{screen}");
    }
}

#[test]
fn tui_loading_rows_carry_a_subtle_hint_and_slow_rows_a_small_tag() {
    let buf = render(&load("sections-loading"), (100, 30));
    let (x, y) = row_containing(&buf, "Tradeoffs …");
    let hint_x = x + u16::try_from("Tradeoffs ".chars().count()).expect("fits");
    assert_eq!(buf[(hint_x, y)].symbol(), "…");
    assert_eq!(buf[(hint_x, y)].fg, Color::DarkGray, "subtle");
    assert!(
        !text(&buf).contains("Deeper Answer …"),
        "a row that has arrived drops its hint"
    );

    let buf = render(&load("sections-slow"), (100, 30));
    let screen = text(&buf);
    let (x, y) = row_containing(&buf, "Example slow");
    let tag_x = x + u16::try_from("Example ".chars().count()).expect("fits");
    assert_eq!(buf[(tag_x, y)].fg, Color::Yellow);
    assert!(
        !screen.contains("Tradeoffs"),
        "an empty row is hidden:\n{screen}"
    );
    assert!(
        screen.contains("Deeper Answer"),
        "filled rows stay:\n{screen}"
    );
}

#[test]
fn tui_an_open_row_is_bold_and_its_body_sits_under_the_grid() {
    let buf = render(&load("sections-open-2"), (100, 30));
    let (x, y) = row_containing(&buf, "Follow-up Questions");
    assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
    let (_, body_y) = row_containing(&buf, "• How would you rate-limit");
    let (_, grid_end) = row_containing(&buf, "6 ▸ Resume Context");
    assert!(body_y > grid_end, "the body is under the grid");
    let screen = text(&buf);
    assert_eq!(
        screen.matches('▾').count(),
        1,
        "exactly one open:\n{screen}"
    );
}

#[test]
fn tui_each_section_opens_to_its_own_content() {
    for (n, needle) in [
        (1, "Algorithm: Token bucket per key"),
        (2, "• How would you rate-limit a key"),
        (3, "Central Redis: + exact counts"),
        (4, "def allow(key, now):"),
        (5, "• You promised to come back to clock skew"),
        (6, "resume.md: Built the API gateway rate limiter"),
    ] {
        let screen = text(&render(&load(&format!("sections-open-{n}")), (100, 30)));
        assert!(screen.contains(needle), "section {n}:\n{screen}");
        assert!(
            screen.contains(&format!("{n} close")),
            "section {n}:\n{screen}"
        );
    }
}

#[test]
fn tui_slow_content_arriving_never_expands_a_section() {
    // Every field of the slow phase has arrived, and no key was pressed.
    let screen = text(&render(&load("sections"), (100, 30)));
    assert!(!screen.contains('▾'), "{screen}");
    for body in ["Algorithm:", "def allow", "resume.md:"] {
        assert!(!screen.contains(body), "{body} expanded:\n{screen}");
    }
}

#[test]
fn tui_an_open_section_on_a_small_terminal_keeps_the_approach_and_cuts_the_body() {
    let screen = text(&render(&load("sections-open-1"), (80, 24)));
    for item in ["1  Clarify scope", "5  If Redis is down"] {
        assert!(screen.contains(item), "{item}:\n{screen}");
    }
    assert!(screen.contains("Algorithm:"), "{screen}");
}

#[test]
fn tui_the_star_block_names_its_story_in_the_header() {
    let screen = text(&render(&load("behavioral-sections"), (100, 30)));
    assert!(screen.contains("┌ STAR · payments-migration"), "{screen}");
    assert!(screen.contains("A  Wrote a one-page"), "{screen}");
    assert!(screen.contains("2 ▾ Follow-up Questions"), "{screen}");
}

#[test]
fn tui_the_model_dot_names_the_provider_and_a_remote_one_says_text_leaves() {
    let label = |name: &str, remote| dmn_session::ProviderLabel {
        name: name.into(),
        model: "m".into(),
        remote,
    };
    let mut actions = actions("listening");
    actions.push(Action::Status {
        dot: Dot::Model,
        health: Health::Ok,
    });
    actions.push(Action::Provider {
        label: label("ollama", false),
    });
    let screen = text(&render(&Session::replay(&actions), (100, 30)));
    assert!(screen.contains("● ollama"), "{screen}");
    assert!(!screen.contains("leaves this machine"), "{screen}");

    actions.push(Action::Provider {
        label: label("anthropic", true),
    });
    let buf = render(&Session::replay(&actions), (100, 30));
    let screen = text(&buf);
    assert!(screen.contains("● anthropic"), "{screen}");
    let (x, y) = row_containing(&buf, "transcript leaves this machine");
    assert_eq!(buf[(x, y)].fg, Color::Yellow);
    // At 80 columns the notice still fits beside the dots.
    let narrow = text(&render(&Session::replay(&actions), (80, 24)));
    assert!(narrow.contains("leaves this machine"), "{narrow}");
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
