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

/// The three problem lines from live session 5 (2026-09-25), with the transcript around them.
async fn live(candidate: &str, transcript: &str, current: Option<(&str, Kind)>) -> FastOutcome {
    let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
    p.warm_up().await.unwrap();
    let mut req = FastRequest::new(candidate, 6);
    req.transcript = transcript.into();
    req.current = current.map(|(q, kind)| dmn_llm::CurrentQuestion {
        question: q.into(),
        kind,
    });
    let outcome = run_fast_phase(&p, &req, CancellationToken::new(), |_| {})
        .await
        .unwrap();
    eprintln!("{candidate}\n  => {outcome:?}\n");
    outcome
}

const SLACK: &str = "How would you design a chat app like Slack for a company with 50,000 people?";

#[tokio::test]
#[ignore]
async fn live_a_new_question_is_not_answered_as_the_one_on_screen() {
    let transcript = format!(
        "Interviewer: {SLACK}\nYou: {SLACK}\nInterviewer: Let's switch gears. Design a ride sharing device.\n\
         Interviewer: Service action system.\nInterviewer: What happens when the database behind your cache goes down?"
    );
    let outcome = live(
        "Design a ride-sharing device matching system.",
        &transcript,
        Some((SLACK, Kind::SystemDesign)),
    )
    .await;
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("{outcome:?}");
    };
    let q = suggestion.question.to_lowercase();
    assert!(q.contains("ride"), "{q}");
    assert!(!q.contains("slack"), "{q}");
}

#[tokio::test]
#[ignore]
async fn live_any_questions_for_me_gets_questions_to_ask_back() {
    let outcome = live(
        "Do you have any questions for me?",
        "Interviewer: Why are you looking to leave your current role?\nYou: I want to work on larger systems.",
        None,
    )
    .await;
    let FastOutcome::Shown { suggestion, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        (3..=5).contains(&suggestion.approach.len()),
        "{suggestion:?}"
    );
}

#[tokio::test]
#[ignore]
async fn live_small_talk_is_still_ignored() {
    let outcome = live(
        "How's your week going so far?",
        "Interviewer: Hi, can you hear me okay?",
        None,
    )
    .await;
    assert!(
        matches!(outcome, FastOutcome::Ignored { .. }),
        "{outcome:?}"
    );
}

/// #54: the job-description fixture becomes ≤ 8 requirement bullets on the real model.
#[tokio::test]
#[ignore]
async fn live_the_job_description_becomes_at_most_8_requirements() {
    let jd = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/context/job-description.txt"
    ))
    .unwrap();
    let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
    p.warm_up().await.unwrap();
    let started = Instant::now();
    let r = dmn_llm::summarise_job_description(&p, &jd, CancellationToken::new())
        .await
        .unwrap();
    eprintln!("{:?}", started.elapsed());
    for b in &r.bullets {
        eprintln!("- {b}");
    }
    assert!((3..=dmn_llm::limits::REQUIREMENTS_MAX).contains(&r.bullets.len()));
    let all = r.bullets.join(" ").to_lowercase();
    assert!(all.contains("rust") || all.contains("go"), "{all}");
}

/// #55: the slow phase on the real model fills every section within 20 s, cites only what it
/// was given, and in hints mode sends no solution.
#[tokio::test]
#[ignore]
async fn live_the_slow_phase_fills_every_section_within_20_s() {
    use dmn_llm::{CodingMode, SlowRequest, run_slow_phase};
    let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b").unwrap();
    p.warm_up().await.unwrap();
    for (question, kind, coding) in [
        (
            "How would you design a rate limiter for a globally distributed API?",
            Kind::SystemDesign,
            CodingMode::Hints,
        ),
        (
            "Given an array of integers, find two that add up to a target.",
            Kind::Coding,
            CodingMode::Hints,
        ),
        (
            "Given an array of integers, find two that add up to a target.",
            Kind::Coding,
            CodingMode::Full,
        ),
    ] {
        let mut req = SlowRequest::new(question, kind);
        req.coding = coding;
        req.focus = "approach, complexity, failure handling".into();
        req.approach = vec![
            "Clarify the constraints.".into(),
            "Pick the core data structure.".into(),
            "Walk through the edge cases.".into(),
        ];
        req.context = FastRequest::new(question, 6);
        req.context.chunks = vec![
            "resume (resume.md): Built the payments API's rate limiter in Go; Redis token buckets, 40k req/s.".into(),
        ];
        req.chunk_files = vec!["resume.md".into()];
        let started = Instant::now();
        let mut arrived = Vec::new();
        let outcome = run_slow_phase(&p, &req, CancellationToken::new(), |f| {
            arrived.push((f.name(), started.elapsed()));
        })
        .await
        .unwrap();
        eprintln!(
            "{question} ({kind:?}, {coding:?}) in {:?}",
            started.elapsed()
        );
        for (name, at) in &arrived {
            eprintln!("  {name:<15} {at:?}");
        }
        eprintln!("  {}", outcome.fields.to_json());
        assert!(started.elapsed() < dmn_llm::slow::SLOW_PHASE_TIMEOUT);
        assert!(outcome.missing.is_empty(), "{:?}", outcome.missing);
        assert!(
            outcome
                .fields
                .resume_context
                .iter()
                .flatten()
                .all(|c| c.source == "resume.md")
        );
        let wants = kind == Kind::Coding && coding == CodingMode::Full;
        assert_eq!(
            outcome.fields.solution.is_some(),
            wants,
            "{kind:?} {coding:?}"
        );
    }
}
