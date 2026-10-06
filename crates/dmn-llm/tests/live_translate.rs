//! #109, live: the translation `t` speaks, against Ollama with `qwen3.6:35b-a3b` (`think: false`,
//! `num_ctx` 8192, the session's settings), five one-sentence English turns into Spanish within
//! the fast phase's 4 s. Prints each time, the p50 and each translation, for the PR:
//! `cargo test -p dmn-llm --test live_translate -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use dmn_llm::{CancellationToken, OllamaProvider, Provider, translate};

const TURNS: [&str; 5] = [
    "I'd start with a token bucket per API key and keep the counters in Redis.",
    "In my last role I led the migration of our billing service to Kubernetes.",
    "If the cache goes down, I'd fail open and serve slightly stale data for a minute.",
    "I'm not sure about the exact number, but I'd measure it before making a decision.",
    "Could you tell me a bit more about how the team handles on-call?",
];

#[tokio::test]
#[ignore]
async fn live_five_turns_translate_into_spanish_within_the_fast_budget() {
    let p = OllamaProvider::new("http://localhost:11434", "qwen3.6:35b-a3b")
        .unwrap()
        .with_keep_alive("30m")
        .with_num_ctx(Some(8192));
    p.warm_up().await.unwrap();
    let budget = Duration::from_secs(4);
    let mut times = Vec::new();
    for said in TURNS {
        let started = Instant::now();
        let t = translate(&p, said, "Spanish (es)", budget, CancellationToken::new())
            .await
            .unwrap();
        let took = started.elapsed();
        eprintln!("{took:>8.3?}  {said}\n          -> {}", t.text);
        assert!(!t.text.is_empty());
        assert!(t.flags.is_empty(), "{:?}", t.flags);
        assert!(
            !t.text.contains('\n') && !t.text.starts_with('"'),
            "one line, no quotes: {}",
            t.text
        );
        times.push(took);
    }
    times.sort();
    eprintln!("p50 {:?}, max {:?}", times[2], times[4]);
    assert!(times[4] < budget);
}
