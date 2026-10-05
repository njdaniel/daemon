//! #101 against the local model: an answer that depends on what is current comes back with a
//! `lookup` and an Approach that does not state a remembered version as the current one; a
//! question about how something works comes back with an empty `lookup`. Needs Ollama on
//! localhost with `qwen3.6:35b-a3b` pulled; run by hand, results in the PR:
//! `cargo test -p dmn-llm --test live_time_sensitive -- --ignored --nocapture time_sensitive`.
//! `DMN_LIVE_RUNS` sets how many times each question is asked (default 5).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use dmn_llm::{
    CancellationToken, FastOutcome, FastRequest, OllamaProvider, OllamaSettings, Provider,
    Suggestion, run_fast_phase,
};

/// What every live request carries as today's date. Pinned, as the caller's clock would give it
/// on the day these were measured.
const TODAY: &str = "2026-10-05";

fn runs() -> usize {
    std::env::var("DMN_LIVE_RUNS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(5)
}

/// The provider as `dmn` builds it from the dev box's config: the context size and keep-alive
/// match, so Ollama does not reload the model for these requests.
async fn provider() -> OllamaProvider {
    let p = OllamaProvider::from_settings(&OllamaSettings {
        num_ctx: Some(8192),
        keep_alive: "30m".into(),
        ..OllamaSettings::default()
    })
    .unwrap();
    p.warm_up().await.unwrap();
    p
}

/// What was said before the question, one per run. The fast phase runs at temperature 0, so the
/// same request gets the same reply; a different lead-in each run is what makes five runs five
/// samples rather than one.
const LEAD_INS: [&str; 5] = [
    "Interviewer: Thanks for walking me through that project.\nYou: Sure, happy to.",
    "Interviewer: Great, let's move on to some quick technical questions.\nYou: Sounds good.",
    "You: And that's how we cut the deploy time in half.\nInterviewer: Nice. Switching gears a bit.",
    "Interviewer: We use a mix of languages on the team.\nYou: Makes sense, I've worked in a few.",
    "",
];

async fn ask(p: &OllamaProvider, question: &str, run: usize) -> (Suggestion, f64) {
    let mut req = FastRequest::new(question, 6);
    req.today = TODAY.into();
    let lead = LEAD_INS[run % LEAD_INS.len()];
    req.transcript = format!("{lead}\nInterviewer: {question}").trim().to_owned();
    let started = Instant::now();
    let mut first_line = None;
    let outcome = run_fast_phase(p, &req, CancellationToken::new(), |e| {
        if matches!(e, dmn_llm::FastEvent::ApproachLine { index: 0, .. }) {
            first_line.get_or_insert(started.elapsed().as_secs_f64());
        }
    })
    .await
    .unwrap();
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("{question}: not shown: {outcome:?}");
    };
    (*suggestion, first_line.unwrap_or(f64::NAN))
}

/// A version-like token: digits with a dot between them ("1.24", "3.13") or "v" and digits.
fn has_version(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')'))
        .map(|w| w.trim_end_matches(['.', '!', '?']))
        .any(|w| {
            let w = w.trim_start_matches(['v', 'V']);
            let mut parts = w.split('.');
            matches!((parts.next(), parts.next()), (Some(a), Some(b))
                if !a.is_empty() && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Words that say a remembered fact may no longer hold.
const HEDGES: &[&str] = &[
    "may be",
    "may have",
    "known",
    "might",
    "out of date",
    "outdated",
    "as of",
    "last i",
    "you know",
    "knew",
    "check",
    "verify",
    "confirm",
    "at the time",
    "newer",
    // As a person says it.
    "as far as",
    "moved on",
    "worked with",
    "i believe",
];

/// Words that make a line's version the one the model remembers, not the current one.
const REMEMBERED: &[&str] = &[
    "know",
    "knew",
    "as of",
    "last",
    "may",
    "might",
    "training",
    "as far as",
    "worked with",
    "believe",
];

/// Words a candidate never says out loud: the Approach is spoken, in their voice, so it names no
/// knowledge cutoff, no training data and never the schema's `lookup` field.
const MACHINE_VOICE: &[&str] = &["cutoff", "cut-off", "training", "lookup", "as an ai"];

/// The first Approach line in the model's voice rather than the candidate's, if any.
fn machine_voice(approach: &[String]) -> Option<&String> {
    approach.iter().find(|l| {
        let l = l.to_lowercase();
        MACHINE_VOICE.iter().any(|w| l.contains(w))
    })
}

/// The Approach states a version as the current one: a line calls a version the latest, current
/// or newest without saying it is only the one the model knows of, or the Approach names a version
/// and nothing on it says it may be old.
fn states_a_version_as_current(approach: &[String]) -> bool {
    let lower: Vec<String> = approach.iter().map(|l| l.to_lowercase()).collect();
    let hedged = lower.iter().any(|l| HEDGES.iter().any(|h| l.contains(h)));
    let called_current = lower.iter().any(|l| {
        has_version(l)
            && ["latest", "current", "newest"]
                .iter()
                .any(|w| l.contains(w))
            && !REMEMBERED.iter().any(|w| l.contains(w))
    });
    called_current || (lower.iter().any(|l| has_version(l)) && !hedged)
}

/// The Approach a time-sensitive answer should have: it says somewhere that what it remembers
/// may be old, never calls a remembered version the current one, and says it all in the
/// candidate's voice ([`machine_voice`]).
fn honest(approach: &[String]) -> bool {
    let lower: Vec<String> = approach.iter().map(|l| l.to_lowercase()).collect();
    lower.iter().any(|l| HEDGES.iter().any(|h| l.contains(h)))
        && !states_a_version_as_current(approach)
        && machine_voice(approach).is_none()
}

fn show(q: &str, i: usize, s: &Suggestion, first_line: f64) {
    eprintln!(
        "[{i}] {q}\n    lookup: {:?}  first line {first_line:.2} s\n    {}",
        s.lookup,
        s.approach.join("\n    ")
    );
}

#[tokio::test]
#[ignore]
async fn time_sensitive_latest_go_version_is_flagged_and_not_stated_as_fact() {
    let p = provider().await;
    let q = "What is the latest stable major version of Go?";
    let mut misses = Vec::new();
    for i in 0..runs() {
        let (s, t) = ask(&p, q, i).await;
        show(q, i, &s, t);
        assert_eq!(
            machine_voice(&s.approach),
            None,
            "run {i}: an Approach line in the model's voice"
        );
        if s.lookup.is_empty() || !honest(&s.approach) {
            misses.push(i);
        }
    }
    assert!(misses.is_empty(), "missed on runs {misses:?}");
}

#[tokio::test]
#[ignore]
async fn time_sensitive_b_tree_split_is_not_flagged() {
    let p = provider().await;
    let q = "How does a B-tree split a full node?";
    let mut misses = Vec::new();
    for i in 0..runs() {
        let (s, t) = ask(&p, q, i).await;
        show(q, i, &s, t);
        if !s.lookup.is_empty() {
            misses.push(i);
        }
    }
    assert!(misses.is_empty(), "flagged on runs {misses:?}");
}

/// More of each sort, for the hit rate in the PR. Prints every count, misses included; the only
/// assertion, after the report, is that no flagged reply's Approach speaks in the model's voice.
#[tokio::test]
#[ignore]
async fn time_sensitive_survey() {
    let p = provider().await;
    let current = [
        "Which Kubernetes version is current?",
        "What's new in the latest Python release?",
        "What is the current LTS of Node?",
        "Is Redis still open source under the same license?",
    ];
    let stable = [
        "Explain how a hash map handles collisions.",
        "Tell me about a time you disagreed with a teammate.",
        "How would you design a rate limiter?",
        "What is the difference between a process and a thread?",
    ];
    let mut report = Vec::new();
    let mut voiced = Vec::new();
    for (questions, wants_lookup) in [(&current[..], true), (&stable[..], false)] {
        for q in questions {
            let (mut flagged, mut hedged) = (0, 0);
            for i in 0..runs() {
                let (s, t) = ask(&p, q, i).await;
                show(q, i, &s, t);
                flagged += usize::from(!s.lookup.is_empty());
                hedged += usize::from(honest(&s.approach));
                if !s.lookup.is_empty()
                    && let Some(line) = machine_voice(&s.approach)
                {
                    voiced.push(format!("[{i}] {q}: {line}"));
                }
            }
            let n = runs();
            report.push(if wants_lookup {
                format!("lookup {flagged}/{n}, Approach hedged {hedged}/{n}: {q}")
            } else {
                format!("lookup empty {}/{n}: {q}", n - flagged)
            });
        }
    }
    eprintln!("\n{}", report.join("\n"));
    assert!(
        voiced.is_empty(),
        "Approach lines in the model's voice:\n{}",
        voiced.join("\n")
    );
}

#[test]
fn the_version_check_reads_versions_and_hedges() {
    let said = |lines: &[&str]| {
        states_a_version_as_current(&lines.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>())
    };
    // The live session's reply (#101).
    assert!(said(&[
        "State Go 1.24 as the latest major version.",
        "Mention it was released in August 2025."
    ]));
    assert!(!said(&[
        "Say Go 1.24 is the newest you know of.",
        "Say it may be out of date."
    ]));
    assert!(!said(&["Explain the split.", "Promote the median key."]));
    // Hedged elsewhere, but one line still calls the remembered version current.
    assert!(said(&[
        "State Go 1.24 as the likely current major version.",
        "Verify the exact number via the official site."
    ]));
    let lines = |l: &[&str]| l.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>();
    assert!(honest(&lines(&[
        "State Go 1.24 as the most recent known stable version.",
        "Suggest checking go.dev."
    ])));
    assert!(!honest(&lines(&[
        "Check lookup for the current Python version.",
        "It may differ."
    ])));
    assert!(!honest(&lines(&[
        "State Redis changed to SSPL in 2024.",
        "Explain why."
    ])));
    for line in [
        "Note it may have changed since my knowledge cutoff.",
        "Verify the exact version number via lookup.",
        "Say newer releases may postdate your training data.",
    ] {
        assert!(machine_voice(&lines(&[line])).is_some(), "{line}");
    }
    assert!(honest(&lines(&[
        "Say the latest you've worked with is Go 1.24.",
        "Add that it may have moved on; you'd confirm on go.dev."
    ])));
    assert!(has_version("Python 3.13 added a JIT"));
    assert!(has_version("since v1.31."));
    assert!(!has_version("O(log n) per insert"));
}
