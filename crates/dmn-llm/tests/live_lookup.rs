//! #102 against the local model, by hand (results in the PR). Needs Ollama on localhost with
//! `qwen3.6:35b-a3b` pulled.
//!
//! - `lookup_live_go_version_end_to_end` is the issue's by-hand check: the Go question asked as
//!   `dmn` asks it, its `lookup` sent to Ollama's web search, and the fast phase asked again with
//!   what came back. It needs a key in `OLLAMA_API_KEY` and does nothing without one:
//!   `OLLAMA_API_KEY=… cargo test -p dmn-llm --test live_lookup -- --ignored --nocapture end_to_end`
//! - `lookup_live_the_model_answers_from_the_results` needs no key: it hands the second request
//!   a result set written for the test, with a version no model has seen, and checks the Approach
//!   says that one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use dmn_llm::{
    CancellationToken, FastOutcome, FastRequest, FreshFact, FreshFacts, OllamaProvider,
    OllamaSearch, OllamaSearchSettings, OllamaSettings, Provider, Suggestion, WebSearch,
    run_fast_phase,
};

const TODAY: &str = "2026-10-05";
const GO: &str = "What is the latest stable major version of Go?";

/// As `dmn` builds it on the dev box, so Ollama does not reload the model.
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

fn request(facts: Option<FreshFacts>) -> FastRequest {
    FastRequest {
        today: TODAY.into(),
        transcript: format!(
            "Interviewer: Great, let's move on to some quick technical questions.\nYou: Sounds good.\nInterviewer: {GO}"
        ),
        facts,
        ..FastRequest::new(GO, 6)
    }
}

/// The reply and when its first Approach line arrived.
async fn ask(p: &OllamaProvider, req: &FastRequest) -> (Suggestion, Duration) {
    let started = Instant::now();
    let mut first_line = None;
    let outcome = run_fast_phase(p, req, CancellationToken::new(), |e| {
        if matches!(e, dmn_llm::FastEvent::ApproachLine { index: 0, .. }) {
            first_line.get_or_insert(started.elapsed());
        }
    })
    .await
    .unwrap();
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("not shown: {outcome:?}");
    };
    (*suggestion, first_line.unwrap_or_default())
}

fn show(label: &str, s: &Suggestion, at: Duration) {
    eprintln!(
        "{label} (first line {:.2} s, lookup {:?}):\n    {}",
        at.as_secs_f64(),
        s.lookup,
        s.approach.join("\n    ")
    );
}

#[tokio::test]
#[ignore]
async fn lookup_live_go_version_end_to_end() {
    let Some(key) = std::env::var("OLLAMA_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        eprintln!("OLLAMA_API_KEY is unset: nothing searched");
        return;
    };
    let p = provider().await;
    let search = OllamaSearch::new(&OllamaSearchSettings {
        api_key: Some(key),
        ..OllamaSearchSettings::default()
    })
    .unwrap();
    let asked = Instant::now();
    let (first, at) = ask(&p, &request(None)).await;
    show("first answer", &first, at);
    assert!(!first.lookup.is_empty(), "the Go question is flagged");
    let searched = Instant::now();
    let results = search
        .search(&first.lookup, CancellationToken::new())
        .await
        .unwrap();
    let took = searched.elapsed();
    let mut flags = Vec::new();
    let facts = dmn_llm::limits::fresh_facts(&results, dmn_llm::limits::FACTS_MAX, &mut flags);
    for f in &facts {
        eprintln!("  [{}] {}: {}", f.domain, f.title, f.snippet);
    }
    let fresh = FreshFacts {
        query: first.lookup.clone(),
        facts,
    };
    let domain = fresh.domain().unwrap_or_default().to_owned();
    let (second, at) = ask(&p, &request(Some(fresh))).await;
    show(&format!("checked · {domain}"), &second, at);
    eprintln!(
        "search {:.2} s; question to corrected Approach (first line) {:.2} s, done {:.2} s",
        took.as_secs_f64(),
        (searched.duration_since(asked) + took + at).as_secs_f64(),
        asked.elapsed().as_secs_f64()
    );
}

#[tokio::test]
#[ignore]
async fn lookup_live_the_model_answers_from_the_results() {
    let p = provider().await;
    // Written for this test: a release no model was trained on, so only the results can name it.
    let fresh = FreshFacts {
        query: "latest stable Go release".into(),
        facts: vec![
            FreshFact {
                title: "All releases - The Go Programming Language".into(),
                snippet: "go1.31.0 (released 2026-08-11) is a major release of Go. go1.31.1 \
                          (released 2026-09-02) includes security fixes to the crypto/tls package."
                    .into(),
                domain: "go.dev".into(),
            },
            FreshFact {
                title: "Go 1.31 Release Notes".into(),
                snippet: "The latest Go release, version 1.31, arrives six months after Go 1.30."
                    .into(),
                domain: "go.dev".into(),
            },
        ],
    };
    let mut misses = 0;
    for run in 0..3 {
        let started = Instant::now();
        let (s, at) = ask(&p, &request(Some(fresh.clone()))).await;
        show(
            &format!("run {run} in {:.2} s", started.elapsed().as_secs_f64()),
            &s,
            at,
        );
        if !s.approach.iter().any(|l| l.contains("1.31")) {
            misses += 1;
        }
    }
    assert_eq!(misses, 0, "the Approach names the version the results give");
}
