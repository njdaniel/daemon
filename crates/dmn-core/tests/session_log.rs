//! The session log end to end without audio: transcript segments through the real detector,
//! candidates through the real fast phase on a `MockProvider`, everything dispatched into a
//! [`Recorder`] exactly as `dmn replay` does, then checked in SQLite. Every path is under a
//! tempdir; nothing touches `~`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use dmn_audio::Source;
use dmn_core::recorder::{self, PurgeTarget, Recorder, open_store};
use dmn_core::{Config, Paths, wire};
use dmn_detect::{Detector, Following};
use dmn_llm::{CancellationToken, CurrentQuestion, FastOutcome, FastRequest, MockProvider};
use dmn_session::{Action, Session};
use dmn_transcript::Segment;
use serde_json::json;

const SECRET: &str = "sk-test-0123456789abcdef-never-store-me";

fn seg(source: Source, start: f64, end: f64, text: &str, is_final: bool) -> Segment {
    Segment {
        source,
        start: Duration::from_secs_f64(start),
        end: Duration::from_secs_f64(end),
        text: text.into(),
        is_final,
        avg_logprob: -0.2,
    }
}

fn replies() -> Vec<serde_json::Value> {
    vec![
        json!({
            "is_question": true, "kind": "system_design",
            "question": "How would you design a rate limiter for a global API?",
            "focus": "algorithm, distributed counters, failure",
            "approach": ["Clarify scope and limits.", "Token bucket per key.", "Counters in Redis.",
                         "Local caches for latency.", "Fail open on Redis loss."]
        }),
        json!({
            "is_question": true, "kind": "clarification",
            "question": "What if Redis goes down for a whole region?",
            "focus": "",
            "approach": ["Fall back to local buckets.", "Alert and shed load."]
        }),
        json!({"is_question": false, "kind": "small_talk", "question": "", "focus": "", "approach": []}),
    ]
}

/// The transcript of a tiny call: partials, finals from both speakers, three interviewer
/// questions (one a clarification, one small talk).
fn call() -> Vec<Segment> {
    vec![
        seg(Source::Interviewer, 1.0, 3.0, "Thanks for joining", false),
        seg(
            Source::Interviewer,
            1.0,
            4.0,
            "Thanks for joining today.",
            true,
        ),
        seg(Source::You, 4.5, 5.5, "Glad to be here.", true),
        seg(
            Source::Interviewer,
            8.0,
            10.0,
            "How would you design",
            false,
        ),
        seg(
            Source::Interviewer,
            8.0,
            13.0,
            "How would you design a rate limiter for a global API?",
            true,
        ),
        seg(
            Source::You,
            15.0,
            20.0,
            "I'd start with a token bucket.",
            true,
        ),
        seg(
            Source::Interviewer,
            25.0,
            28.0,
            "And what if Redis goes down for a whole region?",
            true,
        ),
        seg(
            Source::You,
            30.0,
            34.0,
            "Then each region falls back.",
            true,
        ),
        seg(
            Source::Interviewer,
            40.0,
            42.0,
            "Where are you based these days, by the way?",
            true,
        ),
        seg(Source::You, 43.0, 44.0, "Sunny.", true),
    ]
}

/// Drive `call()` through the detector and the fast phase into `rec`, the way `dmn replay` does.
/// The config (with its API key resolved) is in scope the whole time, as it is in the binary.
async fn drive(rec: &Recorder, cfg: &Config) {
    let _key = cfg.api_key_from(|_| Some(SECRET.to_owned()));
    let hub = rec.hub();
    hub.dispatch(Action::Started {
        label: "test call".into(),
        at: 1_700_000_000,
    });
    let detector = Detector::default();
    let segments = call();
    let mut replies = replies().into_iter();
    let mut next_id = 0u64;
    let mut current: Option<CurrentQuestion> = None;
    for (i, s) in segments.iter().enumerate() {
        hub.dispatch(wire::segment(s));
        let following = segments[i + 1..]
            .iter()
            .find(|n| n.is_final)
            .map(|n| Following {
                silence: Some(n.start.saturating_sub(s.end)),
                next_speaker: Some(n.source),
            })
            .unwrap_or(Following::UNKNOWN);
        let Some(c) = detector.observe(s, following) else {
            continue;
        };
        next_id += 1;
        let id = next_id;
        hub.dispatch(wire::candidate(id, &c));
        let provider = MockProvider::json(&replies.next().expect("a reply per candidate"));
        let req = FastRequest {
            current: current.clone(),
            ..FastRequest::new(c.text.clone(), c.score)
        };
        let result = dmn_llm::run_fast_phase(&provider, &req, CancellationToken::new(), |e| {
            hub.dispatch(wire::fast_event(id, &e));
        })
        .await;
        if let Ok(FastOutcome::Shown {
            suggestion,
            clarification: false,
            ..
        }) = &result
        {
            current = suggestion.kind.map(|kind| CurrentQuestion {
                question: suggestion.question.clone(),
                kind,
            });
        }
        hub.dispatch(wire::outcome(id, &result, 1_700_000_100 + id as i64));
        if id == 1 {
            hub.dispatch(Action::Pin);
        }
    }
}

fn config_with_key_env() -> Config {
    let mut cfg = Config::default();
    cfg.llm.api_key_env = "DMN_TEST_API_KEY".into();
    cfg
}

#[tokio::test]
async fn replay_writes_rows_and_the_action_log_replays_to_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let store = open_store(&paths).unwrap();
    let rec = Recorder::start(Some(store), Some(&paths.db_file));
    let mut log_rx = rec.hub().subscribe();
    let watch = rec.hub().watch();
    drive(&rec, &config_with_key_env()).await;
    let report = rec.finish(1_700_000_600).unwrap().unwrap();

    let mut recorded = Vec::new();
    while let Ok(a) = log_rx.try_recv() {
        recorded.push(a);
    }
    // Determinism: the action log alone rebuilds the state the UI saw.
    let jsonl: String = recorded
        .iter()
        .map(|a| serde_json::to_string(a).unwrap() + "\n")
        .collect();
    let parsed: Vec<Action> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let final_state = watch.borrow().clone();
    assert_eq!(Session::replay(&parsed), final_state);
    let live = final_state.live.as_ref().unwrap();
    assert_eq!(live.approach.len(), 5);
    assert_eq!(live.clarifications.len(), 1, "clarification attached");
    assert_eq!(live.clarifications[0].approach.len(), 2);
    assert!(final_state.pinned);
    assert_eq!(final_state.counts.ignored, 1);

    // Rows: finals only (8 of 10 segments), one question per candidate, a suggestion per shown.
    assert_eq!(report.errors, 0, "{report:?}");
    assert_eq!(report.lagged, 0);
    let id = report.session_id.unwrap();
    let store = open_store(&paths).unwrap();
    assert_eq!(store.segments(id).unwrap().len(), 8);
    assert!(store.segments(id).unwrap().iter().all(|s| s.is_final));
    let questions = store.questions(id).unwrap();
    assert_eq!(questions.len(), 3);
    assert_eq!(
        questions[0].text.as_deref(),
        Some("How would you design a rate limiter for a global API?")
    );
    assert_eq!(questions[0].kind.as_deref(), Some("system_design"));
    assert_eq!(
        questions[0].focus.as_deref(),
        Some("algorithm, distributed counters, failure")
    );
    assert!(questions[0].pinned);
    assert_eq!(questions[1].kind.as_deref(), Some("clarification"));
    assert_eq!(
        questions[2].text, None,
        "small talk is logged, never filled"
    );
    assert_eq!(store.suggestions_for_session(id).unwrap().len(), 2);
    assert_eq!(
        store.session(id).unwrap().unwrap().ended_at,
        Some(1_700_000_600)
    );
    assert_eq!(
        (report.segments, report.questions, report.suggestions),
        (8, 3, 2)
    );
}

#[tokio::test]
async fn the_api_key_never_reaches_the_database_or_an_export() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let cfg = config_with_key_env();
    assert_eq!(
        cfg.api_key_from(|_| Some(SECRET.to_owned()))
            .unwrap()
            .expose(),
        SECRET,
        "the key is resolved in this process"
    );
    let rec = Recorder::start(Some(open_store(&paths).unwrap()), Some(&paths.db_file));
    drive(&rec, &cfg).await;
    let id = rec
        .finish(1_700_000_600)
        .unwrap()
        .unwrap()
        .session_id
        .unwrap();
    let (export, _) = recorder::export(&paths, id).unwrap();

    for file in [&paths.db_file, &export] {
        let bytes = std::fs::read(file).unwrap();
        for needle in [SECRET.as_bytes(), b"DMN_TEST_API_KEY"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "{} holds {:?}",
                file.display(),
                String::from_utf8_lossy(needle)
            );
        }
    }
}

#[tokio::test]
async fn purge_removes_rows_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let cfg = Config::default();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let rec = Recorder::start(Some(open_store(&paths).unwrap()), Some(&paths.db_file));
        drive(&rec, &cfg).await;
        let id = rec
            .finish(1_700_000_600)
            .unwrap()
            .unwrap()
            .session_id
            .unwrap();
        recorder::export(&paths, id).unwrap();
        ids.push(id);
    }
    // A recorded WAV of the first session, as privacy.record_audio would write it.
    std::fs::write(
        paths.sessions_dir.join(format!("{}-you.wav", ids[0])),
        b"RIFF",
    )
    .unwrap();

    let one = recorder::purge(&paths, PurgeTarget::One(ids[0])).unwrap();
    assert_eq!(one.sessions, [ids[0]]);
    assert_eq!(one.files.len(), 2, "{:?}", one.files);
    let store = open_store(&paths).unwrap();
    assert_eq!(store.session_ids().unwrap(), [ids[1]]);
    assert!(store.questions(ids[0]).unwrap().is_empty());
    assert!(
        paths
            .sessions_dir
            .join(format!("{}.jsonl", ids[1]))
            .is_file()
    );
    drop(store);

    assert!(matches!(
        recorder::purge(&paths, PurgeTarget::One(ids[0])),
        Err(recorder::RecorderError::NoSuchSession(_))
    ));

    let all = recorder::purge(&paths, PurgeTarget::All).unwrap();
    assert_eq!(all.sessions, [ids[1]]);
    assert_eq!(all.files.len(), 1);
    let store = open_store(&paths).unwrap();
    assert_eq!(store.counts().unwrap(), (0, 0, 0, 0));
    assert_eq!(std::fs::read_dir(&paths.sessions_dir).unwrap().count(), 0);
    let bytes = std::fs::read(&paths.db_file).unwrap();
    let text = b"rate limiter";
    assert!(
        !bytes.windows(text.len()).any(|w| w == text),
        "purged text is gone from the file, free pages included"
    );
}

#[test]
fn purge_all_leaves_files_that_are_not_a_sessions_alone() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(&paths.sessions_dir).unwrap();
    for name in [
        "7.jsonl",
        "7-you.wav",
        "notes.txt",
        "70s-playlist.m3u",
        "README",
    ] {
        std::fs::write(paths.sessions_dir.join(name), b"x").unwrap();
    }
    // No database at all: the rows are long gone, the files are not.
    let one = recorder::purge(&paths, PurgeTarget::One(7)).unwrap();
    assert_eq!(one.files.len(), 2);
    assert!(one.sessions.is_empty());
    std::fs::write(paths.sessions_dir.join("8.jsonl"), b"x").unwrap();
    let all = recorder::purge(&paths, PurgeTarget::All).unwrap();
    assert_eq!(all.files, [paths.sessions_dir.join("8.jsonl")]);
    let mut left: Vec<String> = std::fs::read_dir(&paths.sessions_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["70s-playlist.m3u", "README", "notes.txt"]);
}

#[tokio::test]
async fn a_failed_file_delete_leaves_the_rows_so_a_retry_finishes_the_purge() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let rec = Recorder::start(Some(open_store(&paths).unwrap()), Some(&paths.db_file));
    drive(&rec, &Config::default()).await;
    let id = rec
        .finish(1_700_000_600)
        .unwrap()
        .unwrap()
        .session_id
        .unwrap();
    let (export, _) = recorder::export(&paths, id).unwrap();

    // A read-only directory makes deleting the export fail.
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&paths.sessions_dir, mode(0o555)).unwrap();
    let first = recorder::purge(&paths, PurgeTarget::One(id));
    std::fs::set_permissions(&paths.sessions_dir, mode(0o755)).unwrap();
    if first.is_ok() {
        // Running as root: permissions do not stop the delete, so there is nothing to retry.
        return;
    }
    assert!(export.is_file(), "the file is still there");
    assert_eq!(
        open_store(&paths).unwrap().session_ids().unwrap(),
        [id],
        "and so are the rows"
    );
    let retry = recorder::purge(&paths, PurgeTarget::One(id)).unwrap();
    assert_eq!(retry.sessions, [id]);
    assert_eq!(retry.files, [export]);
}

#[test]
fn finish_returns_while_another_holder_still_has_the_hub() {
    // `dmn run`'s key handler keeps a handle on the hub until the TUI closes; ending the
    // session must not wait for it.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let rec = Recorder::start(Some(open_store(&paths).unwrap()), Some(&paths.db_file));
    let held = rec.hub();
    held.dispatch(Action::Started {
        label: "held".into(),
        at: 1_700_000_000,
    });
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(rec.finish(1_700_000_060));
    });
    let report = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("finish returned while the hub was still held")
        .unwrap()
        .unwrap();
    assert!(report.session_id.is_some());
    let store = open_store(&paths).unwrap();
    let session = store.session(report.session_id.unwrap()).unwrap().unwrap();
    assert_eq!(session.ended_at, Some(1_700_000_060));
    drop(held);
}
