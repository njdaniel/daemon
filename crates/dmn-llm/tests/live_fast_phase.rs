//! Live fast phase against Ollama. Needs Ollama on localhost with `qwen3.6:35b-a3b` pulled:
//! `cargo test -p dmn-llm --test live_fast_phase -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use dmn_llm::fast::FAST_PHASE_TIMEOUT;
use dmn_llm::{
    CancellationToken, FastEvent, FastOutcome, FastRequest, Kind, OllamaProvider, Provider,
    run_fast_phase,
};

#[tokio::test]
#[ignore]
async fn live_question_streams_before_the_approach() {
    let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
    p.warm_up().await.unwrap();
    let mut req = FastRequest::new(
        "So let's switch gears. How would you design a rate limiter for a globally distributed API?",
        5,
    );
    req.transcript = "Interviewer: Thanks for the intro.\nYou: Happy to be here.".into();
    let started = Instant::now();
    let mut events = Vec::new();
    let outcome = run_fast_phase(&p, &req, CancellationToken::new(), |e| {
        events.push((started.elapsed(), e));
    })
    .await
    .unwrap();
    let total = started.elapsed();
    for (t, e) in &events {
        eprintln!("{t:>10.3?} {e:?}");
    }
    eprintln!("total {total:?}");
    assert!(matches!(events[0].1, FastEvent::Question { .. }));
    let first_line = events
        .iter()
        .find(|(_, e)| matches!(e, FastEvent::ApproachLine { .. }))
        .map(|(t, _)| *t)
        .unwrap();
    assert!(events[0].0 < first_line);
    assert!(total < FAST_PHASE_TIMEOUT);
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(suggestion.kind, Some(Kind::SystemDesign));
    assert!((3..=5).contains(&suggestion.approach.len()));
}
