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
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, Dot, Health, IgnoreReason,
    QuestionSource, Section, Session, SlowEnd, SlowField, Speaker, StarCards, TradeoffItem,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::draw;

const STATES: [&str; 37] = [
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
    // #79: assistance off, transcript first; and on again, back to the three areas.
    "assist-off",
    "assist-on-again",
    // #90: the mic lane asks (nothing eligible heard yet), and either lane asks (a You question
    // answered).
    "asks-you",
    "asks-both",
    // #96: the model answered "not a question" to what Enter sent (the status line says so),
    // and to a detector candidate (nothing is said).
    "asked-not-a-question",
    "detected-not-a-question",
    // #94: the first of three questions on screen again with one of its sections open; the
    // second of five, two of which arrived behind it; a solution taller than its rows, scrolled.
    "earlier",
    "earlier-new",
    "scrolled",
    // #94: a Deeper Answer of four items at the 240-character limit, each wrapped in full and
    // scrolled to the last word of the last: at 80 × 24 that is twelve rows down.
    "long-items",
    // #101: an answer that depends on what is current, tagged `may be out of date`.
    "lookup",
    // #102, the web lookup on: the same question rebuilt on what go.dev says, tagged `checked ·
    // go.dev`; and a lookup that failed, the first answer kept and `lookup failed` said.
    "checked",
    "lookup-failed",
    // #109: `t` translating your last turn, then speaking it (the translation under the
    // question), a press with nothing to speak, and `t` with assistance off.
    "speak-translating",
    "speak-speaking",
    "speak-nothing",
    "speak-assist-off",
];
const SIZES: [(u16, u16); 2] = [(100, 30), (80, 24)];
/// #88: the side-column layout, from 120 columns, on a pane wide enough to read it.
const WIDE_STATES: [&str; 8] = [
    "live",
    "pinned",
    "sections-open-1",
    "behavioral",
    "earlier",
    "earlier-new",
    "scrolled",
    "long-items",
];
const WIDE: (u16, u16) = (140, 30);

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

/// #101: the live session's question, answered without stating a remembered version as current,
/// with what to look up.
fn go_version() -> Vec<Action> {
    let mut a = answered(
        1,
        "What is the latest stable major version of Go?",
        "knowledge",
        "Go release cadence, keeping current",
        &[
            "Say the newest you know of is Go 1.24, from early 2025.",
            "Say a newer one may be out: Go ships a release every six months.",
            "Offer to check go.dev before relying on the number.",
        ],
    );
    // As the fast phase sends it: after the focus, before the first Approach line.
    a.insert(
        3,
        Action::Lookup {
            candidate: 1,
            query: "latest stable Go release".into(),
        },
    );
    a
}

/// #102: [`go_version`] with the web lookup on, its search come back, and the fast phase asked
/// once more with the results: the reply replaces the first in place, tagged with the domain.
fn go_version_checked() -> Vec<Action> {
    let mut a = vec![Action::WebLookup { enabled: true }];
    a.extend(go_version());
    a.push(Action::Regenerate { candidate: 1 });
    a.extend(
        answered(
            1,
            "What is the latest stable major version of Go?",
            "knowledge",
            "Go release cadence, keeping current",
            &[
                "Say the latest stable major release is Go 1.25.",
                "Mention Go 1.25.1 is its newest patch, from September 2025.",
                "Add that Go ships a major release every six months.",
            ],
        )
        .into_iter()
        .skip(1),
    );
    a.push(Action::Checked {
        candidate: 1,
        domain: "go.dev".into(),
    });
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
        "assist-off" | "assist-on-again" => {
            // A question answered while assistance was on, then Ctrl+Shift+A: the Spanish call goes on
            // translated, and the screen is the transcript alone, wrapped, newest at the bottom.
            a.extend(rate_limiter());
            a.push(Action::Assist { enabled: false });
            for (i, (speaker, text)) in ASSIST_OFF_TURNS.iter().enumerate() {
                let start = 40.0 + 6.0 * i as f64;
                a.push(Action::Segment {
                    speaker: *speaker,
                    start,
                    end: start + 5.0,
                    text: (*text).into(),
                    is_final: true,
                    avg_logprob: -0.2,
                    language: Some("es".into()),
                });
            }
            a.push(seg(
                Speaker::Interviewer,
                90.0,
                92.0,
                "And what about the",
                false,
            ));
            if state == "assist-on-again" {
                a.push(Action::Assist { enabled: true });
            }
        }
        "asks-you" => {
            // The interviewer's greeting is in the transcript, but that lane cannot ask: the
            // question area still waits for the mic.
            a.insert(
                1,
                Action::QuestionSource {
                    source: QuestionSource::You,
                },
            );
            a.pop();
        }
        "asks-both" => {
            a.insert(
                1,
                Action::QuestionSource {
                    source: QuestionSource::Both,
                },
            );
            a.push(seg(
                Speaker::You,
                19.0,
                23.0,
                "How does a token bucket differ from a sliding window?",
                true,
            ));
            // The candidate came from the mic lane; the answer is drawn as any other.
            let mut reply = answered(
                1,
                "How does a token bucket differ from a sliding window?",
                "knowledge",
                "burst handling, memory per key, accuracy at the window edge",
                &[
                    "Token bucket: refill at a fixed rate, spend one per request",
                    "It allows bursts up to the bucket size",
                    "Sliding window: count requests in the last N seconds",
                    "It is stricter at the edges, and costs more memory per key",
                ],
            );
            reply[0] = Action::Candidate {
                id: 1,
                speaker: Speaker::You,
                start: 19.0,
                end: 23.0,
                text: "How does a token bucket differ from a sliding window?".into(),
                score: 5,
            };
            a.extend(reply);
        }
        // #96: the rate-limiter question on screen, then a remark of the interviewer's goes to
        // the model, sent by Enter or found by the detector, and the model says it is not a
        // question. The screen keeps what it had; only the key press is answered.
        "asked-not-a-question" | "detected-not-a-question" => {
            let asked = state == "asked-not-a-question";
            a.extend(rate_limiter());
            a.push(seg(
                Speaker::You,
                31.0,
                36.0,
                "Okay, so first I'd want to know what we're limiting on.",
                true,
            ));
            a.push(seg(
                Speaker::Interviewer,
                37.0,
                40.5,
                "Sure. Take whichever you think matters most, right?",
                true,
            ));
            a.push(Action::Candidate {
                id: 2,
                speaker: Speaker::Interviewer,
                start: 37.0,
                end: 40.5,
                text: "Sure. Take whichever you think matters most, right?".into(),
                // The detector never scored what Enter sent.
                score: if asked { 0 } else { 3 },
            });
            if asked {
                a.push(Action::Asked { candidate: 2 });
            }
            a.push(Action::Ignored {
                candidate: 2,
                reason: IgnoreReason::NotAQuestion,
                score: if asked { 0 } else { 3 },
            });
        }
        // #94: three questions, then `←` twice and `2`: the first one again, with its own focus,
        // Approach and sections, Follow-up Questions open.
        "earlier" => {
            a.extend(sections());
            a.extend(follow_on(2));
            a.extend(follow_on(3));
            a.extend([Action::Earlier, Action::Earlier]);
            a.push(Action::ToggleSection {
                section: Section::Followups,
            });
        }
        // #94: `←` once from the third question, then two more arrive behind the second: the
        // screen keeps it, the title counts five and the status line says `2 new`.
        "earlier-new" => {
            a.extend(sections());
            a.extend(follow_on(2));
            a.extend(follow_on(3));
            a.push(Action::Earlier);
            a.extend(follow_on(4));
            a.extend(follow_on(5));
        }
        // #94: a worked solution of 36 lines under two rows of hints, Deeper Answer open and
        // scrolled nine rows down.
        "scrolled" => {
            a.push(Action::CodingSolutions {
                mode: CodingSolutions::Full,
            });
            a.extend(lru_question());
            a.push(Action::ToggleSection {
                section: Section::Deeper,
            });
            a.push(Action::Scroll { offset: 9 });
        }
        // #94: the issue's own case, a Deeper Answer of four 240-character items. None is cut
        // inside: each wraps onto the rows it needs and the body scrolls to the end of the last.
        "lookup" => a.extend(go_version()),
        "checked" => a.extend(go_version_checked()),
        "lookup-failed" => {
            a.push(Action::WebLookup { enabled: true });
            a.extend(go_version());
            a.push(Action::LookupFailed { candidate: 1 });
        }
        // #109: `t` pressed after your answer: the model translating it, then the TTS server and
        // `dmn-voice` speaking it (the translation under the question); a press before you had
        // said anything; and `t` with assistance off, where the line sits above the status line.
        "speak-translating" | "speak-speaking" => {
            a.push(Action::SpeakEnabled { enabled: true });
            a.extend(rate_limiter());
            a.push(seg(Speaker::You, 31.0, 36.5, YOUR_ANSWER, true));
            a.push(Action::SpeakStarted { id: 1 });
            a.push(Action::SpeakElapsed { id: 1, secs: 1 });
            if state == "speak-speaking" {
                a.push(Action::SpeakTranslated {
                    id: 1,
                    text: YOUR_ANSWER_ES.into(),
                });
                a.push(Action::SpeakElapsed { id: 1, secs: 12 });
            }
        }
        "speak-nothing" => {
            a.push(Action::SpeakEnabled { enabled: true });
            a.extend(rate_limiter());
            a.push(Action::SpeakFailed {
                id: 1,
                reason: "nothing to speak".into(),
            });
        }
        "speak-assist-off" => {
            let mut a = actions("assist-off");
            a.insert(1, Action::SpeakEnabled { enabled: true });
            a.extend([
                seg(Speaker::You, 92.5, 97.0, YOUR_ANSWER, true),
                Action::SpeakStarted { id: 1 },
                Action::SpeakTranslated {
                    id: 1,
                    text: YOUR_ANSWER_ES.into(),
                },
                Action::SpeakElapsed { id: 1, secs: 9 },
            ]);
            return a;
        }
        "long-items" => {
            a.extend(long_items());
            a.push(Action::ToggleSection {
                section: Section::Deeper,
            });
            a.push(Action::Scroll { offset: 12 });
        }
        other => panic!("no actions for state {other}"),
    }
    a
}

/// #109: what you said, and the translation `t` speaks.
const YOUR_ANSWER: &str =
    "I'd start with a token bucket per API key and keep the counters in regional Redis.";
const YOUR_ANSWER_ES: &str = "Empezaría con un token bucket por clave de API y guardaría los \
                              contadores en un Redis regional.";

/// #94: four Deeper Answer items, each exactly the 240 characters `dmn-llm`'s limits allow.
const LONG_ITEMS: [(&str, &str); 4] = [
    (
        "Algorithm",
        "Token bucket per key: refill r tokens a second up to burst b, and take one per request. \
         It needs two numbers per key and one atomic update, where a sliding-window log keeps \
         each timestamp and a fixed window lets a burst through at its edge.",
    ),
    (
        "State",
        "Keep the counters in a Redis cluster per region and update them in a Lua script, so the \
         read and the write are one step. A tiny in-process cache in front absorbs hot keys and \
         cuts the Redis round trips to one per key every few milliseconds.",
    ),
    (
        "Consistency",
        "Regions do not share counters on the request path. Each one enforces its share of the \
         quota and reports usage to a reconciler, which moves any unused share to busy regions \
         every few seconds, so a partition over-admits by one share at worst.",
    ),
    (
        "Failure",
        "If Redis is unreachable the limiter fails open to a conservative per-instance limit \
         held in its own memory, sends an alert and marks its decisions as degraded in the \
         access log, so abuse during the outage can be found and billed afterwards.",
    ),
];

/// #94: the rate-limiter question with [`LONG_ITEMS`] as its whole slow phase, none open.
fn long_items() -> Vec<Action> {
    let mut a = rate_limiter();
    a.push(Action::SlowStarted { candidate: 1 });
    a.push(slow(
        1,
        SlowField::Deeper {
            items: LONG_ITEMS
                .iter()
                .map(|(label, text)| DeeperItem {
                    label: (*label).into(),
                    text: (*text).into(),
                })
                .collect(),
        },
    ));
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

/// #94: the questions that follow the rate limiter's, each answered in full; `id` is the
/// candidate, 2 to 5.
fn follow_on(id: u64) -> Vec<Action> {
    let (question, focus, approach): (&str, &str, &[&str]) = match id {
        2 => (
            "Where would you put the limiter in the request path?",
            "gateway, edge, CDN",
            &[
                "Enforce at the gateway, behind the CDN.",
                "Keep a coarse per-IP limit at the edge for abuse.",
            ],
        ),
        3 => (
            "How would you test the limiter before rolling it out?",
            "shadow traffic, staged rollout, kill switch",
            &[
                "Replay a day of production traffic against a shadow limiter.",
                "Compare its decisions with the old limiter's before enforcing.",
                "Roll out region by region, with a kill switch.",
            ],
        ),
        4 => (
            "What would you alert on once it is live?",
            "rejection rate, latency, Redis health",
            &[
                "Rejection rate per tier against its baseline.",
                "Limiter latency at p99, and Redis errors.",
            ],
        ),
        _ => (
            "How does the design change if there is only one region?",
            "exact counts, no reconciliation",
            &["One Redis, exact counts, nothing to reconcile."],
        ),
    };
    answered(id, question, "system_design", focus, approach)
}

/// #94: a coding question whose worked solution is taller than any pane the tests draw.
fn lru_question() -> Vec<Action> {
    let mut a = answered(
        1,
        "Implement an LRU cache with O(1) get and put.",
        "coding",
        "hash map, doubly linked list, eviction order",
        &[
            "Keep a hash map from key to node, and the nodes in a doubly linked list.",
            "Move a node to the front on every get and put.",
            "On overflow, drop the node at the back and its key.",
        ],
    );
    a.push(Action::SlowStarted { candidate: 1 });
    a.push(slow(
        1,
        SlowField::Deeper {
            items: vec![DeeperItem {
                label: "Insight".into(),
                text: "The list holds the order and the map finds a node in O(1); two sentinel \
                       nodes mean no insert or unlink ever checks for an empty list."
                    .into(),
            }],
        },
    ));
    a.push(slow(
        1,
        SlowField::Solution {
            code: CodeBlock {
                language: "python".into(),
                code: LRU_SOLUTION.into(),
            },
        },
    ));
    a.push(slow_ended(1, SlowEnd::Done));
    a.push(seg(
        Speaker::You,
        31.0,
        36.0,
        "I'd pair a dictionary with a doubly linked list.",
        true,
    ));
    a
}

const LRU_SOLUTION: &str = "\
class Node:
    def __init__(self, key, value):
        self.key, self.value = key, value
        self.prev = self.next = None

class LRUCache:
    def __init__(self, capacity):
        self.capacity = capacity
        self.items = {}
        self.head, self.tail = Node(None, None), Node(None, None)
        self.head.next, self.tail.prev = self.tail, self.head

    def _unlink(self, node):
        node.prev.next, node.next.prev = node.next, node.prev

    def _push_front(self, node):
        node.prev, node.next = self.head, self.head.next
        self.head.next.prev = node
        self.head.next = node

    def get(self, key):
        node = self.items.get(key)
        if node is None:
            return -1
        self._unlink(node)
        self._push_front(node)
        return node.value

    def put(self, key, value):
        if key in self.items:
            self._unlink(self.items[key])
        node = Node(key, value)
        self.items[key] = node
        self._push_front(node)
        if len(self.items) > self.capacity:
            oldest = self.tail.prev
            self._unlink(oldest)
            del self.items[oldest.key]
";

/// #79's fixture: a translated exchange, some lines long enough to wrap at 80 columns.
const ASSIST_OFF_TURNS: [(Speaker, &str); 7] = [
    (
        Speaker::Interviewer,
        "Let's move on. Tell me about the team you worked with on the payments migration.",
    ),
    (
        Speaker::You,
        "There were five of us: two backend engineers, a data engineer, a product manager and me leading the cut-over plan.",
    ),
    (Speaker::Interviewer, "How long did the migration take?"),
    (
        Speaker::You,
        "About four months, with the last three weeks running both systems side by side and comparing every ledger entry.",
    ),
    (
        Speaker::Interviewer,
        "And did anything go wrong on the day?",
    ),
    (
        Speaker::You,
        "One batch job still pointed at the old database, so we paused it and replayed its writes.",
    ),
    (
        Speaker::Interviewer,
        "Good. Let's talk about how you would test that.",
    ),
];

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
    let wide = WIDE_STATES.iter().map(|s| (*s, WIDE));
    let every = STATES
        .iter()
        .flat_map(|s| SIZES.iter().map(move |size| (*s, *size)))
        .chain(wide);
    for (state, size) in every {
        let got = text(&render(&load(state), size));
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
fn tui_an_answer_that_depends_on_what_is_current_is_tagged_dim_on_the_approach_border() {
    for size in SIZES {
        let buf = render(&load("lookup"), size);
        let (x, y) = row_containing(&buf, "may be out of date");
        let screen = text(&buf);
        let row = screen.lines().nth(usize::from(y)).unwrap_or_default();
        assert!(
            row.contains("APPROACH"),
            "on the Approach border:\n{screen}"
        );
        assert_eq!(buf[(x, y)].fg, Color::DarkGray, "{screen}");
    }
    // No query, no tag: the other fixtures are the snapshots above, unchanged.
    for state in ["live", "pinned", "behavioral", "sections", "earlier"] {
        let screen = text(&render(&load(state), (100, 30)));
        assert!(!screen.contains("may be out of date"), "{state}:\n{screen}");
    }
}

#[test]
fn tui_lookup_a_checked_answer_names_its_source_and_web_sits_by_the_model_dot() {
    for size in SIZES {
        let buf = render(&load("checked"), size);
        let screen = text(&buf);
        let (x, y) = row_containing(&buf, "checked · go.dev");
        let row = screen.lines().nth(usize::from(y)).unwrap_or_default();
        assert!(
            row.contains("APPROACH"),
            "on the Approach border:\n{screen}"
        );
        assert_eq!(buf[(x, y)].fg, Color::DarkGray, "{screen}");
        assert!(!screen.contains("may be out of date"), "{screen}");
        assert!(
            screen.contains("Go 1.25"),
            "the second answer, in place:\n{screen}"
        );
        assert!(!screen.contains("1.24"), "the first is gone:\n{screen}");
        // `web` right after the model dot, dim.
        let (x, y) = row_containing(&buf, "● model  web ");
        let at = x + u16::try_from("● model  ".chars().count()).unwrap();
        assert_eq!(buf[(at, y)].symbol(), "w");
        assert_eq!(buf[(at, y)].fg, Color::DarkGray, "{screen}");
    }
    // Off, or with assistance off, no `web`: the other fixtures are the snapshots, unchanged.
    for state in ["live", "lookup", "assist-off"] {
        let screen = text(&render(&load(state), (100, 30)));
        assert!(!screen.contains(" web "), "{state}:\n{screen}");
    }
    let mut off = actions("checked");
    off.push(Action::Assist { enabled: false });
    let screen = text(&render(&Session::replay(&off), (100, 30)));
    assert!(!screen.contains(" web "), "{screen}");
}

#[test]
fn tui_lookup_failed_keeps_the_first_answer_and_says_so_in_amber_until_it_expires() {
    for size in SIZES {
        let buf = render(&load("lookup-failed"), size);
        let screen = text(&buf);
        assert!(screen.contains("may be out of date"), "{screen}");
        assert!(
            screen.contains("Go 1.24"),
            "the first answer stays:\n{screen}"
        );
        let (x, y) = row_containing(&buf, "lookup failed");
        assert_eq!(buf[(x, y)].fg, Color::Yellow, "{screen}");
        assert_eq!(
            usize::from(y) + 1,
            screen.lines().count(),
            "on the status line"
        );
    }
    let mut log = actions("lookup-failed");
    log.push(Action::NoticeExpired { candidate: 1 });
    let screen = text(&render(&Session::replay(&log), (100, 30)));
    assert!(!screen.contains("lookup failed"), "{screen}");
    assert!(screen.contains("may be out of date"), "{screen}");
}

#[test]
fn tui_the_out_of_date_tag_follows_the_newest_reply() {
    let mut log = actions("lookup");
    // Streaming: the query came before the lines, so the tag is up while they arrive.
    let streaming: Vec<Action> = log
        .iter()
        .take_while(|a| !matches!(a, Action::Answered { .. }))
        .cloned()
        .collect();
    let screen = text(&render(&Session::replay(&streaming), (100, 30)));
    assert!(screen.contains("may be out of date"), "{screen}");

    // `r`: the old Approach dims and says `thinking…` until the new reply starts.
    log.push(Action::Regenerate { candidate: 1 });
    let screen = text(&render(&Session::replay(&log), (100, 30)));
    assert!(screen.contains("thinking…"), "{screen}");
    assert!(!screen.contains("may be out of date"), "{screen}");

    // The new reply names nothing to look up: no tag.
    log.extend(
        answered(
            1,
            "What is the latest stable major version of Go?",
            "knowledge",
            "Go release cadence",
            &["a", "b", "c"],
        )
        .into_iter()
        .skip(1),
    );
    let screen = text(&render(&Session::replay(&log), (100, 30)));
    assert!(!screen.contains("may be out of date"), "{screen}");
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
fn tui_assist_off_is_the_transcript_alone_and_says_so() {
    // #79: no question block, no Approach, the status line says `assist off` with the
    // translation tag, and the footer offers Ctrl+Shift+A back.
    let buf = render(&load("assist-off"), (100, 30));
    let screen = text(&buf);
    for gone in ["LIVE QUESTION", "APPROACH", "rate limiter"] {
        assert!(
            !screen.contains(gone),
            "{gone:?} still on screen:\n{screen}"
        );
    }
    let last = screen.lines().nth(29).unwrap_or_default();
    assert!(last.contains(" es→en "), "{last}");
    assert!(last.ends_with("assist off · C-S-a on"), "{last}");
    let (x, y) = row_containing(&buf, "assist off");
    assert_eq!(buf[(x, y)].fg, Color::Yellow);
    // The transcript takes the released rows: the first translated line is on screen, the
    // newest (a partial) is the last line above the status.
    assert!(screen.contains("Tell me about the team"), "{screen}");
    let above = screen.lines().nth(28).unwrap_or_default();
    assert!(above.contains("And what about the▍"), "{screen}");

    let on = text(&render(&load("assist-on-again"), (100, 30)));
    assert!(
        on.contains("LIVE QUESTION") && !on.contains("assist off"),
        "{on}"
    );
}

#[test]
fn tui_a_wide_pane_puts_the_transcript_in_a_right_hand_column() {
    // #88: from 120 columns the transcript leaves the strip under the Approach for a column on
    // the right; one column narrower, the stacked layout is what it always was.
    let session = load("live");
    let wide = render(&session, (120, 30));
    let screen = text(&wide);
    let (x, y) = row_containing(&wide, "TRANSCRIPT");
    assert!(
        y == 0 && x > 60,
        "the column starts top-right ({x}, {y}):\n{screen}"
    );
    assert_eq!(row_containing(&wide, "LIVE QUESTION"), (2, 0), "{screen}");
    assert_eq!(
        row_containing(&wide, "● mic").1,
        29,
        "status full-width at the foot"
    );
    let narrow = render(&session, (119, 30));
    assert_eq!(
        row_containing(&narrow, "TRANSCRIPT").1,
        26,
        "stacked below 120 columns:\n{}",
        text(&narrow)
    );
}

#[test]
fn tui_the_side_column_keeps_the_end_of_an_utterance_taller_than_it() {
    let words: Vec<String> = (1..=200).map(|n| format!("w{n}")).collect();
    let mut a = actions("live");
    a.push(seg(
        Speaker::Interviewer,
        40.0,
        52.0,
        &words.join(" "),
        true,
    ));
    let screen = text(&render(&Session::replay(&a), (120, 12)));
    let rows: Vec<&str> = screen.lines().collect();
    assert!(
        rows[10].ends_with("w200"),
        "the newest word at the column's foot:\n{screen}"
    );
    assert!(
        !screen.contains("w1 "),
        "the oldest words scrolled off:\n{screen}"
    );
}

#[test]
fn tui_assist_off_keeps_the_end_of_an_utterance_taller_than_the_screen() {
    // Copilot on PR #80: one final longer than every row available kept its first rows and lost
    // its newest words.
    let words: Vec<String> = (1..=120).map(|n| format!("w{n}")).collect();
    let mut a = actions("assist-off");
    a.push(Action::Segment {
        speaker: Speaker::Interviewer,
        start: 95.0,
        end: 107.0,
        text: words.join(" "),
        is_final: true,
        avg_logprob: -0.2,
        language: Some("es".into()),
    });
    let screen = text(&render(&Session::replay(&a), (60, 8)));
    let rows: Vec<&str> = screen.lines().collect();
    assert!(
        rows[6].ends_with("w120"),
        "the newest word on the last row:\n{screen}"
    );
    assert!(
        !screen.contains("w1 "),
        "the oldest words scrolled off:\n{screen}"
    );
    assert!(!screen.contains('…'), "nothing cut mid-screen:\n{screen}");
}

#[test]
fn tui_assist_off_is_never_cut_on_a_narrow_pane() {
    // #79, found live: at 80 columns with every tag on the left (`system: all audio`, `es→en`,
    // the model dot's name), `assist off` ran into the keys as `assi`. It leads the keys now.
    let mut a = actions("assist-off");
    a.push(Action::Status {
        dot: Dot::System,
        health: Health::AllAudio,
    });
    let screen = text(&render(&Session::replay(&a), (80, 23)));
    let last = screen.lines().nth(22).unwrap_or_default();
    assert!(last.ends_with("assist off · C-S-a on"), "{last}");
    assert!(last.contains("system: all audio"), "{last}");
}

#[test]
fn tui_assist_off_keeps_the_newest_words_when_the_window_overflows() {
    // A short terminal: only the newest rows fit, and the oldest line shown is cut from the top.
    let screen = text(&render(&load("assist-off"), (80, 8)));
    let rows: Vec<&str> = screen.lines().collect();
    assert!(rows[6].contains("And what about the▍"), "{screen}");
    assert!(
        !screen.contains("payments migration"),
        "the oldest lines scroll off: {screen}"
    );
}

#[test]
fn tui_a_model_partly_in_system_memory_is_named() {
    // #79: whisper took the GPU first, the LLM loaded after it into what was left.
    let with = |cpu_percent: u8| {
        let mut a = actions("live");
        a.push(Action::LlmOffload { cpu_percent });
        render(&Session::replay(&a), (100, 30))
    };
    let buf = with(35);
    let (x, y) = row_containing(&buf, "llm partly on cpu");
    assert_eq!(buf[(x, y)].fg, Color::Yellow);
    assert!(
        !text(&with(0)).contains("partly"),
        "all on the GPU says nothing"
    );
    assert!(
        !text(&with(4)).contains("partly"),
        "the usual few percent on the dev 4090 says nothing either"
    );
}

#[test]
fn tui_the_status_line_names_a_question_source_other_than_the_interviewer() {
    // #90: `asks: you` / `asks: both`; nothing for the default, nothing while assistance is off.
    for size in SIZES {
        assert!(text(&render(&load("asks-you"), size)).contains(" asks: you "));
        assert!(text(&render(&load("asks-both"), size)).contains(" asks: both "));
        for state in ["listening", "live", "assist-off"] {
            let screen = text(&render(&load(state), size));
            assert!(!screen.contains("asks:"), "{state}:\n{screen}");
        }
    }
    let mut off = actions("asks-both");
    off.push(Action::Assist { enabled: false });
    let screen = text(&render(&Session::replay(&off), (100, 30)));
    assert!(!screen.contains("asks:"), "{screen}");
    assert!(screen.contains("assist off"), "{screen}");
}

#[test]
fn tui_a_declined_key_press_says_not_a_question_and_a_declined_detector_candidate_nothing() {
    // #96: the same reply (`is_question: false`) to the same line. Sent by Enter, the status
    // line answers the key in amber; found by the detector, nothing is said, as before.
    for size in SIZES {
        let last_row = usize::from(size.1) - 1;
        let buf = render(&load("asked-not-a-question"), size);
        let asked = text(&buf);
        let last = asked.lines().nth(last_row).unwrap_or_default();
        assert!(
            last.ends_with("not a question · p pin · r redo · ⏎ ask"),
            "{last}"
        );
        let (x, y) = row_containing(&buf, "not a question");
        assert_eq!(buf[(x, y)].fg, Color::Yellow);
        // The screen keeps what it had: the question, and its Approach at full strength.
        assert!(asked.contains("LIVE QUESTION"), "{asked}");
        let (x, y) = row_containing(&buf, "Use a token bucket");
        assert!(!buf[(x, y)].modifier.contains(Modifier::DIM));

        let detected = text(&render(&load("detected-not-a-question"), size));
        assert!(!detected.contains("not a question"), "{detected}");
        let last = detected.lines().nth(last_row).unwrap_or_default();
        assert!(last.ends_with(" p pin · r redo · ⏎ ask"), "{last}");

        // Once the pipeline says the 5 s are up, the two screens are the same.
        let mut expired = actions("asked-not-a-question");
        expired.push(Action::NoticeExpired { candidate: 2 });
        assert_eq!(text(&render(&Session::replay(&expired), size)), detected);
    }
}

#[test]
fn tui_not_a_question_is_never_cut_on_a_narrow_pane() {
    // As `assist off` (#79): it leads the keys, which are never cut, whatever tags the left
    // side of an 80-column pane carries.
    let mut a = actions("asked-not-a-question");
    a.insert(
        1,
        Action::QuestionSource {
            source: QuestionSource::Both,
        },
    );
    a.push(Action::Status {
        dot: Dot::System,
        health: Health::AllAudio,
    });
    a.push(Action::Transcriber {
        device: "CPU".into(),
        fallback: false,
    });
    let screen = text(&render(&Session::replay(&a), (80, 24)));
    let last = screen.lines().nth(23).unwrap_or_default();
    assert!(
        last.ends_with("not a question · p pin · r redo · ⏎ ask"),
        "{last}"
    );
    assert!(last.contains("system: all audio"), "{last}");
}

/// The status line of `session` at `size`.
fn status_line(session: &Session, size: (u16, u16)) -> String {
    text(&render(session, size))
        .lines()
        .nth(usize::from(size.1) - 1)
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn tui_the_keys_offer_ask_whenever_assistance_is_on() {
    // #96: in every state of the screen, since the detector's miss can come in any; not while
    // assistance is off, when Enter sends nothing.
    // #109: with `[speak]` on, `t speak` follows it.
    for state in STATES {
        let session = load(state);
        let last = status_line(&session, (100, 30));
        let tail = if session.speak {
            " · ⏎ ask · t speak"
        } else {
            " · ⏎ ask"
        };
        assert_eq!(last.ends_with(tail), !session.assist_off, "{state}: {last}");
    }
}

/// #109: `t speak` is offered while `[speak]` is on, assistance on or off, and never otherwise;
/// the tag says where the press is, with its seconds, in the You lane's colour; the translation
/// is one dim line; and why a press spoke nothing leads the keys in amber.
#[test]
fn tui_speak_shows_its_key_its_tag_its_line_and_its_notice() {
    for state in STATES {
        let session = load(state);
        let last = status_line(&session, (100, 30));
        assert_eq!(last.contains("t speak"), session.speak, "{state}: {last}");
    }
    let off = load("speak-assist-off");
    assert!(
        status_line(&off, (100, 30)).ends_with("assist off · C-S-a on · t speak"),
        "{}",
        status_line(&off, (100, 30))
    );

    let buf = render(&load("speak-translating"), (100, 30));
    let (x, y) = row_containing(&buf, "translating… 1s");
    assert_eq!(buf[(x, y)].fg, Color::Magenta);
    assert!(!text(&buf).contains("» "), "no line before the translation");

    for size in SIZES {
        let buf = render(&load("speak-speaking"), size);
        let screen = text(&buf);
        assert!(screen.contains("speaking… 12s"), "{screen}");
        let (x, y) = row_containing(&buf, "» Empezaría");
        assert_eq!(buf[(x + 2, y)].fg, Color::DarkGray, "dim");
        assert!(
            screen.contains("rate limiter for a globally distributed API"),
            "the question keeps its block:\n{screen}"
        );
    }
    let buf = render(&load("speak-assist-off"), (100, 30));
    let rows: Vec<String> = text(&buf).lines().map(str::to_owned).collect();
    assert!(
        rows[28].starts_with("» Empezaría"),
        "above the status line: {}",
        rows[28]
    );

    let buf = render(&load("speak-nothing"), (80, 24));
    let (x, y) = row_containing(&buf, "nothing to speak");
    assert_eq!(buf[(x, y)].fg, Color::Yellow);
}

#[test]
fn tui_ask_gives_its_cells_to_a_status_note_that_would_otherwise_be_cut() {
    // #96 beside #59: at 80 columns the "transcript leaves this machine" notice and an error's
    // reason had the cells `⏎ ask` would take, and keep them; with room for both, both show.
    let mut remote = actions("live");
    remote.push(Action::Provider {
        label: dmn_session::ProviderLabel {
            name: "anthropic".into(),
            model: "m".into(),
            remote: true,
        },
    });
    let remote = Session::replay(&remote);
    let narrow = status_line(&remote, (80, 24));
    assert!(
        narrow.contains("transcript leaves this machine") && narrow.ends_with(" p pin · r redo"),
        "{narrow}"
    );
    let wide = status_line(&remote, (100, 30));
    assert!(
        wide.contains("transcript leaves this machine")
            && wide.ends_with(" p pin · r redo · ⏎ ask"),
        "{wide}"
    );
    let failed = load("behavioral");
    let narrow = status_line(&failed, (80, 24));
    assert!(
        narrow.contains("model: ollama unreachable") && !narrow.contains("⏎ ask"),
        "{narrow}"
    );
    assert!(status_line(&failed, (100, 30)).ends_with(" · ⏎ ask"));

    // A reason too long for the pane beside any keys is cut as it always was, and the key
    // stays.
    let mut long = actions("live");
    long.push(Action::Status {
        dot: Dot::Model,
        health: Health::Error("error sending request for url (http://localhost:11434)".into()),
    });
    let cut = status_line(&Session::replay(&long), (80, 24));
    assert!(cut.contains('…') && cut.ends_with(" · ⏎ ask"), "{cut}");

    // The answer to the key outlasts the keys: beside the notice it stands alone.
    let mut declined = actions("asked-not-a-question");
    declined.push(Action::Provider {
        label: dmn_session::ProviderLabel {
            name: "anthropic".into(),
            model: "m".into(),
            remote: true,
        },
    });
    let both = status_line(&Session::replay(&declined), (80, 24));
    assert!(
        both.contains("transcript leaves this machine") && both.ends_with(" not a question"),
        "{both}"
    );
}

#[test]
fn tui_the_listening_placeholder_names_who_can_ask() {
    // #90: before anything is heard on a lane that can ask.
    let waiting = |source: Option<QuestionSource>| {
        let mut a = vec![Action::Started {
            label: "mock".into(),
            at: 1_790_200_000,
        }];
        a.extend(source.map(|source| Action::QuestionSource { source }));
        text(&render(&Session::replay(&a), (100, 30)))
    };
    assert!(waiting(None).contains("Waiting for the interviewer…"));
    assert!(waiting(Some(QuestionSource::Interviewer)).contains("Waiting for the interviewer…"));
    assert!(waiting(Some(QuestionSource::You)).contains("Waiting for your question…"));
    assert!(waiting(Some(QuestionSource::Both)).contains("Waiting for a question…"));
    // The interviewer's greeting does not fill the question area when only the mic can ask, and
    // it is in the transcript under its own label all the same.
    let screen = text(&render(&load("asks-you"), (100, 30)));
    assert!(screen.contains("Waiting for your question…"), "{screen}");
    assert!(
        screen.contains("Interviewer Hi, thanks for making"),
        "{screen}"
    );
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

// --- #94: earlier questions and a section that scrolls -----------------------------------------

#[test]
fn tui_an_earlier_question_is_titled_with_its_place_and_is_never_dimmed() {
    let mut a = actions("earlier");
    // A candidate out to the model dims the live Approach; the one on screen is not the live one.
    a.push(Action::Candidate {
        id: 9,
        speaker: Speaker::Interviewer,
        start: 200.0,
        end: 204.0,
        text: "And at ten times the load?".into(),
        score: 5,
    });
    for size in [(100, 30), WIDE] {
        let buf = render(&Session::replay(&a), size);
        let (x, y) = row_containing(&buf, "QUESTION 1 of 3");
        assert_eq!((y, buf[(x, y)].fg), (0, Color::Gray));
        let screen = text(&buf);
        assert!(!screen.contains("LIVE QUESTION"), "{screen}");
        assert!(!screen.contains("thinking…"), "{screen}");
        let (x, y) = row_containing(&buf, "Clarify scope");
        assert!(!buf[(x, y)].modifier.contains(Modifier::DIM), "{screen}");
        // Its own focus and sections, the one the key opened among them.
        assert!(
            screen.contains("Focus: architecture, consistency"),
            "{screen}"
        );
        assert!(screen.contains("2 ▾ Follow-up Questions"), "{screen}");
        assert!(
            screen.contains("• How would you rate-limit a key"),
            "{screen}"
        );
        assert!(!screen.contains("before rolling it out"), "{screen}");
        let keys = status_line(&Session::replay(&a), size);
        assert!(keys.ends_with(" → later · 2 close · ⏎ ask"), "{keys}");
    }
}

#[test]
fn tui_questions_behind_an_earlier_one_are_counted_in_amber_on_the_status_line() {
    for size in [(100, 30), WIDE] {
        let buf = render(&load("earlier-new"), size);
        let (x, y) = row_containing(&buf, "2 new");
        assert_eq!(y, size.1 - 1, "on the status line");
        assert_eq!(buf[(x, y)].fg, Color::Yellow);
        let screen = text(&buf);
        assert!(screen.contains("QUESTION 2 of 5"), "{screen}");
        assert!(
            screen.contains("Where would you put the limiter"),
            "the screen keeps the question it had:\n{screen}"
        );
        assert!(!screen.contains("only one region"), "{screen}");
        assert!(
            screen.ends_with(" 2 new · ← earlier · → later · ⏎ ask\n"),
            "{screen}"
        );
        // Nothing is counted until something arrives.
        assert!(!status_line(&load("earlier"), size).contains("new"));
    }
}

#[test]
fn tui_the_earlier_key_is_named_once_there_is_one_and_yields_to_a_status_note_after_ask() {
    assert!(status_line(&load("live"), (100, 30)).ends_with(" p pin · r redo · ⏎ ask"));
    let mut two = actions("live");
    two.extend(follow_on(2));
    let keys = status_line(&Session::replay(&two), (100, 30));
    assert!(
        keys.ends_with(" p pin · r redo · ← earlier · ⏎ ask"),
        "{keys}"
    );

    // Beside "transcript leaves this machine" the keys go from the end, one at a time, as far
    // as the note needs: `⏎ ask` first (#96), then `← earlier`.
    two.push(Action::Provider {
        label: dmn_session::ProviderLabel {
            name: "anthropic".into(),
            model: "m".into(),
            remote: true,
        },
    });
    let remote = Session::replay(&two);
    for (width, keys) in [
        (100, " p pin · r redo · ← earlier · ⏎ ask"),
        (90, " p pin · r redo · ← earlier"),
        (80, " p pin · r redo"),
    ] {
        let line = status_line(&remote, (width, 30));
        assert!(
            line.contains("transcript leaves this machine") && line.ends_with(keys),
            "{width}: {line}"
        );
    }

    // On an earlier question the count is the last to go, as `not a question` is.
    let mut behind = actions("earlier-new");
    behind.push(Action::Provider {
        label: dmn_session::ProviderLabel {
            name: "anthropic".into(),
            model: "m".into(),
            remote: true,
        },
    });
    let line = status_line(&Session::replay(&behind), (80, 24));
    assert!(
        line.contains("transcript leaves this machine") && line.ends_with(" 2 new"),
        "{line}"
    );
}

#[test]
fn tui_a_cut_section_says_how_much_is_above_and_below_it() {
    for size in [(100, 30), WIDE] {
        let mut s = load("scrolled");
        let buf = render(&s, size);
        let (x, y) = row_containing(&buf, "↑ 9 · ↓ ");
        assert_eq!(buf[(x, y)].fg, Color::DarkGray);
        let scrolled = text(&buf);
        assert!(
            !scrolled.contains("Insight:") && !scrolled.contains("class LRUCache"),
            "nine rows are above:\n{scrolled}"
        );
        assert!(scrolled.contains("def _unlink(self, node):"), "{scrolled}");
        assert!(status_line(&s, size).contains("↑↓ scroll"), "{scrolled}");

        // At its top the hint counts only what is below.
        s.scroll = 0;
        let top = text(&render(&s, size));
        assert!(top.contains("Insight:"), "{top}");
        assert!(top.contains("      ↓ ") && !top.contains("↑ "), "{top}");

        // An offset past the end (the pane grew under it) shows the last row, not a blank.
        s.scroll = u16::MAX;
        let end = text(&render(&s, size));
        assert!(end.contains("del self.items[oldest.key]"), "{end}");
        assert!(end.contains("      ↑ ") && !end.contains("more"), "{end}");
    }
}

#[test]
fn tui_a_section_that_fits_is_drawn_whole_with_no_hint() {
    for n in 1..=6 {
        let s = load(&format!("sections-open-{n}"));
        let screen = text(&render(&s, (100, 30)));
        assert!(
            !screen.contains(" more\n") && !screen.contains('↑'),
            "section {n}:\n{screen}"
        );
        for by in [
            super::Scroll::Up,
            super::Scroll::Down,
            super::Scroll::PageUp,
            super::Scroll::PageDown,
        ] {
            let pane = ratatui::layout::Rect::new(0, 0, 100, 30);
            assert_eq!(super::scroll_to(&s, pane, by), None, "section {n} {by:?}");
        }
    }
}

#[test]
fn tui_scroll_keys_go_a_row_or_the_rows_on_screen_and_stop_at_both_ends() {
    use super::{Scroll, scroll_to};
    use ratatui::layout::Rect;
    // The body's rows less the hint's: 13 under the Approach at 100 × 30, 16 on the wide pane.
    for ((w, h), page) in [((100, 30), 13), (WIDE, 16)] {
        let pane = Rect::new(0, 0, w, h);
        let mut s = load("scrolled");
        s.scroll = 0;
        assert_eq!(scroll_to(&s, pane, Scroll::Up), None);
        assert_eq!(scroll_to(&s, pane, Scroll::PageUp), None);
        assert_eq!(scroll_to(&s, pane, Scroll::Down), Some(1));
        assert_eq!(scroll_to(&s, pane, Scroll::PageDown), Some(page));

        // PgDn to the end: every press moves, and the last one lands on the last row.
        let mut presses = 0;
        while let Some(to) = scroll_to(&s, pane, Scroll::PageDown) {
            assert!(to > s.scroll && to - s.scroll <= page);
            s.scroll = to;
            presses += 1;
            assert!(presses < 10, "it ends");
        }
        let end = text(&render(&s, (w, h)));
        assert!(end.contains("del self.items[oldest.key]"), "{end}");
        // The hint's row, up to the transcript column's edge on the wide pane.
        let hint = format!("↑ {}", s.scroll);
        assert!(
            end.lines()
                .any(|row| row.split('│').next().is_some_and(|l| l.trim() == hint)),
            "{end}"
        );
        assert_eq!(scroll_to(&s, pane, Scroll::Down), None);
        assert_eq!(scroll_to(&s, pane, Scroll::Up), Some(s.scroll - 1));
        assert_eq!(scroll_to(&s, pane, Scroll::PageUp), Some(s.scroll - page));

        // An offset left past the end by a resize comes back within the body on the next key.
        let last = s.scroll;
        s.scroll = 500;
        assert_eq!(scroll_to(&s, pane, Scroll::Down), Some(last));
        assert_eq!(scroll_to(&s, pane, Scroll::Up), Some(last - 1));

        // With assistance off there is no section on screen to scroll.
        s.assist_off = true;
        assert_eq!(scroll_to(&s, pane, Scroll::Down), None);
    }
}

#[test]
fn tui_a_240_character_item_is_read_to_its_last_word_by_scrolling() {
    use super::{Scroll, scroll_to};
    use ratatui::layout::Rect;
    for (_, text) in LONG_ITEMS {
        assert_eq!(text.chars().count(), 240, "at the limit: {text}");
    }
    let whole: Vec<String> = LONG_ITEMS
        .iter()
        .map(|(label, text)| format!("{label}: {text}"))
        .collect();
    for (w, h) in [(80, 24), (100, 30), WIDE] {
        let pane = Rect::new(0, 0, w, h);
        let mut s = load("long-items");
        s.scroll = 0;
        // Every body row the user sees on the way down, by its place in the body.
        let mut rows: Vec<String> = Vec::new();
        loop {
            let screen = text(&render(&s, (w, h)));
            // The body's rows: under the grid's one row, down to the hint (if it is cut) or the
            // transcript; on the wide pane, left of the transcript column.
            let body: Vec<String> = screen
                .lines()
                .skip_while(|row| !row.contains("1 ▾ Deeper Answer"))
                .skip(1)
                .map(|row| row.split('│').next().unwrap_or_default().trim().to_owned())
                .take_while(|row| !row.contains("TRANSCRIPT") && !row.contains('●'))
                .filter(|row| !row.is_empty() && !row.starts_with(['↑', '↓']))
                .collect();
            for (i, row) in body.into_iter().enumerate() {
                assert!(!row.contains('…'), "{w}×{h}: cut inside an item:\n{screen}");
                let at = usize::from(s.scroll) + i;
                if at == rows.len() {
                    rows.push(row);
                } else {
                    assert_eq!(rows[at], row, "{w}×{h}: the same row at every offset");
                }
            }
            match scroll_to(&s, pane, Scroll::Down) {
                Some(to) => s.scroll = to,
                None => break,
            }
        }
        // Read top to bottom, the rows are the four items word for word.
        assert_eq!(rows.join(" "), whole.join(" "), "{w}×{h}");
        let end = text(&render(&s, (w, h)));
        assert!(end.contains("billed afterwards."), "{w}×{h}:\n{end}");
    }
    // At 80 × 24 an item takes four rows and the body has four: twelve rows of scrolling, which
    // is where the fixture stands, on the whole of the last item.
    let fixture = load("long-items");
    assert_eq!(fixture.scroll, 12);
    let pane = Rect::new(0, 0, 80, 24);
    assert_eq!(scroll_to(&fixture, pane, Scroll::Down), None, "the end");
    let end = text(&render(&fixture, (80, 24)));
    assert!(end.contains("Failure: If Redis is unreachable"), "{end}");
    assert!(end.contains("      ↑ 12\n"), "{end}");
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

/// Where `needle` first sits on the drawn screen, as (column, row) in cells.
fn find(screen: &str, needle: &str) -> Option<(u16, u16)> {
    screen.lines().enumerate().find_map(|(y, row)| {
        let at = row.find(needle)?;
        Some((
            u16::try_from(row[..at].chars().count()).ok()?,
            u16::try_from(y).ok()?,
        ))
    })
}

#[test]
fn tui_a_click_finds_each_of_the_six_rows_where_the_frame_drew_it() {
    use super::section_at;
    use ratatui::layout::Rect;
    // One column (a 50-wide pane), two columns stacked (100 and 80) and the wide layout (#88).
    for (w, h) in [(50, 24), (100, 30), (80, 24), WIDE] {
        let s = load("sections");
        let screen = text(&render(&s, (w, h)));
        let pane = Rect::new(0, 0, w, h);
        for (n, section) in Section::ALL.into_iter().enumerate() {
            let (x, y) = find(&screen, &format!("{} ▸", n + 1))
                .unwrap_or_else(|| panic!("row {} at {w}x{h}:\n{screen}", n + 1));
            assert_eq!(
                section_at(&s, pane, x, y),
                Some(section),
                "{w}x{h} key {}",
                n + 1
            );
            // Anywhere along the row counts, not only its number; one row down is another.
            assert_eq!(section_at(&s, pane, x + 6, y), Some(section), "{w}x{h}");
            assert_ne!(section_at(&s, pane, x, y + 1), Some(section), "{w}x{h}");
        }
        // Outside the grid: the question, the Approach, the transcript, the status line.
        let (_, first) = find(&screen, "1 ▸").unwrap();
        for (x, y) in [(0, 0), (5, 2), (5, first - 1), (5, h - 1), (w - 1, 0)] {
            assert_eq!(section_at(&s, pane, x, y), None, "{w}x{h} at {x},{y}");
        }
        if w < super::SIDE_MIN_COLS {
            // The transcript strip is under the grid; on the wide pane its column is on the right.
            assert_eq!(section_at(&s, pane, 5, h - 2), None, "{w}x{h}");
        } else {
            assert_eq!(section_at(&s, pane, w - 3, first), None, "{w}x{h} column");
        }
        // Past the last cell of the pane, with assistance off, and with no question.
        assert_eq!(section_at(&s, pane, w, first), None);
        let mut off = s.clone();
        off.assist_off = true;
        assert_eq!(section_at(&off, pane, 5, first), None);
        assert_eq!(section_at(&Session::default(), pane, 5, first), None);
    }
}

#[test]
fn tui_the_wheel_goes_three_rows_a_notch_on_the_left_column_and_stops_at_both_ends() {
    use super::{Scroll, on_left_column, scroll_to};
    use ratatui::layout::Rect;
    for (state, (w, h)) in [
        ("scrolled", (100, 30)),
        ("scrolled", WIDE),
        ("long-items", (80, 24)),
        ("long-items", WIDE),
    ] {
        let pane = Rect::new(0, 0, w, h);
        let mut s = load(state);
        s.scroll = 0;
        assert_eq!(scroll_to(&s, pane, Scroll::WheelUp), None);
        // Three rows, or what is left when the body overflows by fewer.
        let first = scroll_to(&s, pane, Scroll::WheelDown).expect("an open, cut section");
        assert!((1..=3).contains(&first), "{state} {w}x{h}: {first}");
        let mut notches = 0;
        while let Some(to) = scroll_to(&s, pane, Scroll::WheelDown) {
            assert!(to > s.scroll && to - s.scroll <= 3);
            s.scroll = to;
            notches += 1;
            assert!(notches < 20, "it ends");
        }
        // At the last row: no more, and the other way goes back three, or to the top.
        assert_eq!(scroll_to(&s, pane, Scroll::WheelDown), None);
        assert_eq!(
            scroll_to(&s, pane, Scroll::WheelUp),
            Some(s.scroll.saturating_sub(3))
        );
        s.scroll = 2;
        assert_eq!(scroll_to(&s, pane, Scroll::WheelUp), Some(0));

        // With no section open there is nothing to move.
        assert_eq!(scroll_to(&load("sections"), pane, Scroll::WheelDown), None);

        // The left column: the question and Approach, not the transcript.
        assert!(on_left_column(&s, pane, 5, 10));
        assert!(!on_left_column(&s, pane, 5, h - 1), "the status line");
        if w >= super::SIDE_MIN_COLS {
            assert!(on_left_column(&s, pane, w * 60 / 100 - 1, 10));
            assert!(
                !on_left_column(&s, pane, w - 2, 10),
                "the transcript column"
            );
        } else {
            assert!(!on_left_column(&s, pane, 5, h - 3), "the transcript strip");
        }
        s.assist_off = true;
        assert!(!on_left_column(&s, pane, 5, 10));
    }
}
