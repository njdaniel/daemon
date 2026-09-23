//! `is_question=false` / `kind=small_talk` changes nothing on screen and is logged with the
//! detector score (#12). A separate test binary so no other test shares tracing's global
//! callsite cache with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use dmn_llm::{CancellationToken, FastOutcome, FastRequest, MockProvider, run_fast_phase};

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn ignored_candidate_is_logged_with_its_score() {
    let buf = Buf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let mock = MockProvider::json(&serde_json::json!({"is_question": false, "kind": "small_talk"}));
    let mut published = 0;
    let outcome = rt
        .block_on(run_fast_phase(
            &mock,
            &FastRequest::new("Can you hear me?", 2),
            CancellationToken::new(),
            |_| published += 1,
        ))
        .unwrap();
    assert!(matches!(outcome, FastOutcome::Ignored { score: 2, .. }));
    assert_eq!(published, 0, "nothing reaches the screen");
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("candidate not shown"), "{log}");
    assert!(log.contains("score=2"), "{log}");
    assert!(log.contains("Can you hear me?"), "{log}");
}
