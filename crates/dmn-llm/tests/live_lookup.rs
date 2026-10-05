//! #102 against the local model, by hand (results in the PR). Needs Ollama on localhost with
//! `qwen3.6:35b-a3b` pulled.
//!
//! - `lookup_live_go_version_end_to_end` is the issue's by-hand check: the Go question asked as
//!   `dmn` asks it, its `lookup` sent to Ollama's web search, and the fast phase asked again with
//!   what came back. It needs a key in `OLLAMA_API_KEY` and does nothing without one:
//!   `OLLAMA_API_KEY=… cargo test -p dmn-llm --test live_lookup -- --ignored --nocapture end_to_end`
//! - `lookup_live_go_version_searxng` is the same against a SearXNG on this machine (no key;
//!   `DMN_SEARXNG_URL`, default `http://localhost:8888`); it says so and stops when none answers:
//!   `cargo test -p dmn-llm --test live_lookup -- --ignored --nocapture searxng`
//! - `lookup_live_the_model_answers_from_the_results` needs no key: it hands the second request
//!   a result set written for the test, with a version no model has seen, and checks the Approach
//!   says that one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use dmn_llm::{
    CancellationToken, FastOutcome, FastRequest, FreshFact, FreshFacts, OllamaProvider,
    OllamaSearch, OllamaSearchSettings, OllamaSettings, Provider, SearchResult, SearxngSearch,
    SearxngSettings, Suggestion, WebSearch, run_fast_phase,
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
    let mut tries = 0;
    let (outcome, first_line) = loop {
        let started = Instant::now();
        let mut first_line = None;
        let outcome = run_fast_phase(p, req, CancellationToken::new(), |e| {
            if matches!(e, dmn_llm::FastEvent::ApproachLine { index: 0, .. }) {
                first_line.get_or_insert(started.elapsed());
            }
        })
        .await;
        match outcome {
            Ok(o) => break (o, first_line),
            Err(e) if tries < 2 => {
                tries += 1;
                eprintln!("  ({e}; asked again)");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
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
    let search = OllamaSearch::new(&OllamaSearchSettings {
        api_key: Some(key),
        ..OllamaSearchSettings::default()
    })
    .unwrap();
    let p = provider().await;
    let mut cache = Cache::open();
    let run = end_to_end(&p, &search, &mut cache, &GO_CASE, 0).await;
    report(&GO_CASE, 0, &run, None);
    assert_go_newest(&run);
}

#[tokio::test]
#[ignore]
async fn lookup_live_go_version_searxng() {
    let Some(search) = searxng().await else {
        return;
    };
    let p = provider().await;
    let mut cache = Cache::open();
    let run = end_to_end(&p, &search, &mut cache, &GO_CASE, 0).await;
    cache.save();
    assert_go_newest(&run);
}

/// The SearXNG `DMN_SEARXNG_URL` names (default `http://localhost:8888`), asked for the cap of
/// ten results so a cached reply serves any `max_results`; `None`, and a note, when none answers.
async fn searxng() -> Option<SearxngSearch> {
    let base_url = std::env::var("DMN_SEARXNG_URL")
        .unwrap_or_else(|_| SearxngSearch::DEFAULT_BASE_URL.to_owned());
    let search = SearxngSearch::new(&SearxngSettings {
        base_url: base_url.clone(),
        max_results: dmn_llm::lookup::MAX_RESULTS,
        ..SearxngSettings::default()
    })
    .unwrap();
    let probe = reqwest::get(format!("{base_url}/healthz")).await;
    if let Err(e) = probe {
        eprintln!("no SearXNG answering at {base_url}: {e}; nothing searched");
        return None;
    }
    Some(search)
}

/// One question for the survey: what it asks, and the version that is right today, from
/// `DMN_TRUTH_<KEY>` (fetched from the project's own page when the survey is run; never
/// hard-coded here). Go needs none: its truth is the newest `go1.N` the results show.
struct Case {
    key: &'static str,
    question: &'static str,
}

const GO_CASE: Case = Case {
    key: "GO",
    question: GO,
};

const SURVEY: [Case; 4] = [
    GO_CASE,
    Case {
        key: "KUBERNETES",
        question: "Which Kubernetes version is current?",
    },
    Case {
        key: "NODE",
        question: "What is the current LTS version of Node.js?",
    },
    Case {
        key: "PYTHON",
        question: "What is the latest Python release?",
    },
];

/// What was said before the question, one per run, so five runs at temperature 0 are five
/// samples (as in `live_time_sensitive.rs`).
const LEAD_INS: [&str; 5] = [
    "Interviewer: Thanks for walking me through that project.\nYou: Sure, happy to.",
    "Interviewer: Great, let's move on to some quick technical questions.\nYou: Sounds good.",
    "You: And that's how we cut the deploy time in half.\nInterviewer: Nice. Switching gears a bit.",
    "Interviewer: We use a mix of languages on the team.\nYou: Makes sense, I've worked in a few.",
    "",
];

fn request_for(question: &str, run: usize, facts: Option<FreshFacts>) -> FastRequest {
    let lead = LEAD_INS[run % LEAD_INS.len()];
    FastRequest {
        today: TODAY.into(),
        transcript: format!("{lead}\nInterviewer: {question}").trim().to_owned(),
        facts,
        ..FastRequest::new(question, 6)
    }
}

/// Search replies by query, kept on disk in `DMN_LOOKUP_CACHE` when it is set, so measuring a
/// prompt change re-asks the model and not the search engines (PR #104 kept to about 40 real
/// searches). Each entry is the reply and how long the real search took.
struct Cache {
    path: Option<String>,
    entries: serde_json::Map<String, serde_json::Value>,
    searched: usize,
}

impl Cache {
    fn open() -> Self {
        let path = std::env::var("DMN_LOOKUP_CACHE").ok();
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            path,
            entries,
            searched: 0,
        }
    }

    async fn search(&mut self, search: &dyn WebSearch, query: &str) -> (Vec<SearchResult>, f64) {
        if let Some(hit) = self.entries.get(query) {
            let results = hit["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| SearchResult {
                    title: r["title"].as_str().unwrap().into(),
                    url: r["url"].as_str().unwrap().into(),
                    content: r["content"].as_str().unwrap().into(),
                })
                .collect();
            return (results, hit["took"].as_f64().unwrap());
        }
        // Spaced out, so the engines behind the instance see a person, not a burst.
        if self.searched > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        self.searched += 1;
        let started = Instant::now();
        let results = search
            .search(query, CancellationToken::new())
            .await
            .unwrap_or_default();
        let took = started.elapsed().as_secs_f64();
        let json: Vec<serde_json::Value> = results
            .iter()
            .map(|r| serde_json::json!({"title": r.title, "url": r.url, "content": r.content}))
            .collect();
        self.entries.insert(
            query.to_owned(),
            serde_json::json!({"results": json, "took": took}),
        );
        (results, took)
    }

    fn save(&self) {
        if let Some(p) = &self.path {
            std::fs::write(
                p,
                serde_json::Value::Object(self.entries.clone()).to_string(),
            )
            .unwrap();
        }
        eprintln!("real searches this run: {}", self.searched);
    }
}

/// One run of the pipeline as `dmn` sequences it.
struct Run {
    query: String,
    domain: String,
    facts: Vec<FreshFact>,
    first: Suggestion,
    second: Suggestion,
    /// Question to the corrected Approach's first line, as the suggester would see it: the
    /// second request starts once the first reply is done and the search (started when
    /// `lookup` closed) has come back.
    to_corrected: f64,
    /// The second request's user message, in characters.
    prompt_chars: usize,
}

async fn end_to_end(
    p: &OllamaProvider,
    search: &dyn WebSearch,
    cache: &mut Cache,
    case: &Case,
    run: usize,
) -> Run {
    let req = request_for(case.question, run, None);
    // The model may be busy with another client of the same Ollama: a 4 s timeout is retried
    // (twice), and said, so a busy GPU never passes for a wrong answer.
    let mut tries = 0;
    let (outcome, lookup_at, first_done) = loop {
        let started = Instant::now();
        let mut lookup_at = None;
        let outcome = run_fast_phase(p, &req, CancellationToken::new(), |e| {
            if matches!(e, dmn_llm::FastEvent::Lookup { .. }) {
                lookup_at.get_or_insert(started.elapsed().as_secs_f64());
            }
        })
        .await;
        match outcome {
            Ok(o) => break (o, lookup_at, started.elapsed().as_secs_f64()),
            Err(e) if tries < 2 => {
                tries += 1;
                eprintln!("  (first request: {e}; asked again)");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("not shown: {outcome:?}");
    };
    let first = *suggestion;
    if first.lookup.is_empty() {
        return Run {
            query: String::new(),
            domain: String::new(),
            facts: Vec::new(),
            first: first.clone(),
            second: first,
            to_corrected: f64::NAN,
            prompt_chars: 0,
        };
    }
    let (results, took) = cache.search(search, &first.lookup).await;
    let max = std::env::var("DMN_SURVEY_MAX")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(5);
    let kept: Vec<SearchResult> = results.into_iter().take(max).collect();
    let mut flags = Vec::new();
    let facts = dmn_llm::limits::fresh_facts(&kept, max, &mut flags);
    let fresh = FreshFacts {
        query: first.lookup.clone(),
        facts: facts.clone(),
    };
    let domain = fresh.domain().unwrap_or_default().to_owned();
    let second_req = request_for(case.question, run, Some(fresh));
    let prompt_chars = dmn_llm::prompt::user_message(&second_req).chars().count()
        + dmn_llm::prompt::system_prompt(None, dmn_llm::Asker::Interviewer)
            .chars()
            .count();
    let (second, first_line) = ask(p, &second_req).await;
    let search_done = lookup_at.unwrap_or(first_done) + took;
    Run {
        query: first.lookup.clone(),
        domain,
        facts,
        first,
        second,
        to_corrected: first_done.max(search_done) + first_line.as_secs_f64(),
        prompt_chars,
    }
}

/// The newest `go1.N` any kept result shows, as `1.N`.
fn newest_go(facts: &[FreshFact]) -> Option<String> {
    let mut best: Option<u32> = None;
    for f in facts {
        let text = format!("{} {}", f.title, f.snippet).to_lowercase();
        for (at, _) in text.match_indices("go1.") {
            let digits: String = text[at + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<u32>() {
                best = Some(best.map_or(n, |b| b.max(n)));
            }
        }
    }
    best.map(|n| format!("1.{n}"))
}

/// `version` (`1.27`, `24`, `3.14`) is named in `text`: a token equal to it or continuing it
/// (`1.27.1`, `v24.21.0`), not one that only starts with the same digits (`1.270`).
fn names_version(text: &str, version: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|t| t.trim_matches('.'))
        .any(|t| t == version || t.starts_with(&format!("{version}.")))
}

/// The first version the Approach names (`1.27.1`, `v24.21.0`, `3.14`), the one it puts first
/// and calls current; a year (four digits, no dot) is not a version.
fn first_version(text: &str) -> Option<&str> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|t| t.trim_matches('.'))
        .find(|t| !t.is_empty() && (t.contains('.') || t.len() < 4))
}

/// The corrected Approach puts `version` first: "Go 1.26 is the latest; 1.27 is in development"
/// names 1.27 but is wrong.
fn says_current(said: &str, version: &str) -> bool {
    first_version(said).is_some_and(|t| t == version || t.starts_with(&format!("{version}.")))
}

fn assert_go_newest(run: &Run) {
    let newest = newest_go(&run.facts).expect("the results name a go1.N");
    let said = run.second.approach.join(" ");
    assert!(
        says_current(&said, &newest),
        "the corrected Approach names {newest}, the newest the results show: {said}"
    );
}

fn report(case: &Case, i: usize, run: &Run, right: Option<bool>) {
    eprintln!(
        "[{} {i}] query {:?} · {} · {:.2} s · prompt {} chars · right {:?}\n  first:     {}\n  corrected: {}",
        case.key,
        run.query,
        if run.domain.is_empty() {
            "-".to_owned()
        } else {
            format!("checked · {}", run.domain)
        },
        run.to_corrected,
        run.prompt_chars,
        right,
        run.first.approach.join(" | "),
        run.second.approach.join(" | ")
    );
}

/// PR #104's measurement: each survey question five times, end to end against SearXNG, the
/// corrected Approach scored against the version that is right today (`DMN_TRUTH_<KEY>`; Go's
/// is the newest `go1.N` its own results show). Prints; asserts only Go.
#[tokio::test]
#[ignore]
async fn lookup_live_survey() {
    let Some(search) = searxng().await else {
        return;
    };
    let p = provider().await;
    let mut cache = Cache::open();
    let mut table = Vec::new();
    let mut go_misses = 0;
    for case in &SURVEY {
        let truth = std::env::var(format!("DMN_TRUTH_{}", case.key)).ok();
        let (mut right, mut times, mut queries, mut domains) =
            (0, Vec::new(), Vec::new(), Vec::new());
        for i in 0..5 {
            let run = end_to_end(&p, &search, &mut cache, case, i).await;
            let said = run.second.approach.join(" ");
            let want = if case.key == "GO" {
                newest_go(&run.facts)
            } else {
                truth.clone()
            };
            let ok = want.as_deref().map(|v| says_current(&said, v));
            report(case, i, &run, ok);
            right += usize::from(ok == Some(true));
            if case.key == "GO" && ok != Some(true) {
                go_misses += 1;
            }
            times.push(run.to_corrected);
            queries.push(run.query.clone());
            domains.push(run.domain.clone());
        }
        queries.dedup();
        domains.dedup();
        times.sort_by(f64::total_cmp);
        table.push(format!(
            "{}: right {right}/5 (truth {:?}) · queries {queries:?} · tag {domains:?} · question→corrected p50 {:.2} s, max {:.2} s",
            case.key,
            truth.as_deref().unwrap_or("newest go1.N in results"),
            times[2],
            times[4]
        ));
    }
    cache.save();
    eprintln!("\n{}", table.join("\n"));
    assert_eq!(
        go_misses, 0,
        "Go: the corrected Approach names the newest go1.N shown"
    );
}

#[test]
fn the_version_helpers_read_versions_as_tokens() {
    let fact = |s: &str| FreshFact {
        title: String::new(),
        snippet: s.into(),
        domain: "go.dev".into(),
    };
    assert_eq!(
        newest_go(&[
            fact("go1.26.5 (released 2026-07-07)"),
            fact("Stable versions · go1.27.1 · go1.26.8")
        ])
        .as_deref(),
        Some("1.27")
    );
    assert_eq!(newest_go(&[fact("Go 1.27 is out")]), None);
    assert!(names_version("Say Go 1.27.1 is the latest.", "1.27"));
    assert!(names_version("Node.js v24.21.0 (Krypton)", "24"));
    assert!(names_version("Python 3.14 is current", "3.14"));
    assert!(!names_version("Go 1.270", "1.27"));
    assert!(!names_version("Node 26 is current, 2024 was", "24"));
    assert!(!says_current(
        "State Go 1.26 is the latest. | Mention Go 1.27 is in development.",
        "1.27"
    ));
    assert!(says_current(
        "State Node.js 24 is the current LTS. | It reached LTS in October 2025.",
        "24"
    ));
    assert!(says_current(
        "State v1.37 as the latest stable release.",
        "1.37"
    ));
    assert!(!says_current(
        "State the latest stable minor version.",
        "1.37"
    ));
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
