//! Unit tests. Everything runs against `:memory:` databases except the re-open test, which uses
//! a `tempfile` path so a second `Store` sees the first one's schema.

use serde_json::Value;

use crate::store::*;
use crate::{SCHEMA_VERSION, StoreError};

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn session_with_segment(store: &Store) -> (i64, i64) {
    let session_id = store
        .insert_session(&NewSession::new(1_700_000_000, "fixture"))
        .unwrap();
    let segment_id = store
        .insert_segment(&NewSegment {
            session_id,
            source: Source::Interviewer,
            start_time: 12.5,
            end_time: 18.0,
            text: "How would you design a rate limiter?".to_string(),
            is_final: true,
            avg_logprob: -0.2,
        })
        .unwrap();
    (session_id, segment_id)
}

fn question_with_suggestion(store: &Store, session_id: i64, segment_id: i64) -> i64 {
    let question_id = store
        .insert_question(&NewQuestion {
            session_id,
            segment_id,
            score: 4,
            created_at: 1_700_000_018,
        })
        .unwrap();
    store
        .set_question_text(
            question_id,
            "How would you design a rate limiter?",
            Some("system_design"),
            Some("distributed rate limiting"),
        )
        .unwrap();
    store
        .insert_suggestion(&NewSuggestion {
            question_id,
            phase: Phase::Fast,
            payload: r#"{"question":"How would you design a rate limiter?"}"#.to_string(),
            created_at: 1_700_000_019,
        })
        .unwrap();
    question_id
}

#[test]
fn fresh_db_migrates_to_the_current_schema() {
    let store = store();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    let objects = store.schema_object_names().unwrap();
    for table in [
        "sessions",
        "segments",
        "questions",
        "suggestions",
        "cpu_samples",
    ] {
        assert!(
            objects.contains(&table.to_string()),
            "missing table {table}"
        );
    }
    for index in [
        "idx_segments_session",
        "idx_questions_session",
        "idx_questions_segment",
        "idx_suggestions_question",
    ] {
        assert!(
            objects.contains(&index.to_string()),
            "missing index {index}"
        );
    }
}

#[test]
fn reopen_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");

    let first = Store::open(&path).unwrap();
    let session_id = first
        .insert_session(&NewSession::new(1_700_000_000, "persisted"))
        .unwrap();
    drop(first);

    let second = Store::open(&path).unwrap();
    assert_eq!(second.schema_version().unwrap(), SCHEMA_VERSION);
    let session = second.session(session_id).unwrap().unwrap();
    assert_eq!(session.label, "persisted");
}

#[test]
fn session_round_trip() {
    let store = store();
    let id = store
        .insert_session(&NewSession::new(1_700_000_000, "live"))
        .unwrap();

    let open = store.session(id).unwrap().unwrap();
    assert_eq!(open.started_at, 1_700_000_000);
    assert_eq!(open.ended_at, None);
    assert_eq!(open.label, "live");

    store.end_session(id, 1_700_003_600).unwrap();
    let closed = store.session(id).unwrap().unwrap();
    assert_eq!(closed.ended_at, Some(1_700_003_600));

    assert_eq!(store.session(id + 1).unwrap(), None);
}

#[test]
fn segment_round_trip() {
    let store = store();
    let (session_id, _) = session_with_segment(&store);
    store
        .insert_segment(&NewSegment {
            session_id,
            source: Source::You,
            start_time: 19.0,
            end_time: 24.5,
            text: "I'd start with a token bucket.".to_string(),
            is_final: true,
            avg_logprob: -0.1,
        })
        .unwrap();

    let segments = store.segments(session_id).unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].source, Source::Interviewer);
    assert_eq!(segments[0].start_time, 12.5);
    assert_eq!(segments[0].end_time, 18.0);
    assert!(segments[0].is_final);
    assert_eq!(segments[0].avg_logprob, -0.2);
    assert_eq!(segments[1].source, Source::You);
    assert_eq!(segments[1].text, "I'd start with a token bucket.");
}

#[test]
fn question_round_trip() {
    let store = store();
    let (session_id, segment_id) = session_with_segment(&store);
    let id = store
        .insert_question(&NewQuestion {
            session_id,
            segment_id,
            score: 4,
            created_at: 1_700_000_018,
        })
        .unwrap();

    // The candidate starts with no extraction; the LLM fills it later.
    let candidate = store.questions(session_id).unwrap().remove(0);
    assert_eq!(candidate.id, id);
    assert_eq!(candidate.segment_id, segment_id);
    assert_eq!(candidate.score, 4);
    assert_eq!(candidate.text, None);
    assert!(!candidate.pinned);

    store
        .set_question_text(
            id,
            "How would you design a rate limiter?",
            Some("system_design"),
            Some("distributed rate limiting"),
        )
        .unwrap();
    store.set_question_pinned(id, true).unwrap();

    let filled = store.questions(session_id).unwrap().remove(0);
    assert_eq!(
        filled.text.as_deref(),
        Some("How would you design a rate limiter?")
    );
    assert_eq!(filled.kind.as_deref(), Some("system_design"));
    assert_eq!(filled.focus.as_deref(), Some("distributed rate limiting"));
    assert!(filled.pinned);
}

#[test]
fn suggestion_round_trip() {
    let store = store();
    let (session_id, segment_id) = session_with_segment(&store);
    let question_id = store
        .insert_question(&NewQuestion {
            session_id,
            segment_id,
            score: 4,
            created_at: 1_700_000_018,
        })
        .unwrap();

    store
        .insert_suggestion(&NewSuggestion {
            question_id,
            phase: Phase::Fast,
            payload: r#"{"question":"How would you design a rate limiter?"}"#.to_string(),
            created_at: 1_700_000_019,
        })
        .unwrap();
    store
        .insert_suggestion(&NewSuggestion {
            question_id,
            phase: Phase::Slow,
            payload: r#"{"deeper":["token bucket vs leaky bucket"]}"#.to_string(),
            created_at: 1_700_000_024,
        })
        .unwrap();

    let suggestions = store.suggestions_for_question(question_id).unwrap();
    assert_eq!(suggestions.len(), 2);
    assert_eq!(suggestions[0].phase, Phase::Fast);
    assert_eq!(suggestions[1].phase, Phase::Slow);
    // The payload round-trips byte-identical; the store never parses it.
    assert_eq!(
        suggestions[0].payload,
        r#"{"question":"How would you design a rate limiter?"}"#
    );
    assert_eq!(store.suggestions_for_session(session_id).unwrap().len(), 2);
}

#[test]
fn export_writes_one_json_object_per_line() {
    let store = store();
    let (session_id, segment_id) = session_with_segment(&store);
    question_with_suggestion(&store, session_id, segment_id);

    let mut out = Vec::new();
    let written = store.export_session_jsonl(session_id, &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(written, 4);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["label"], "fixture");
    assert_eq!(lines[1]["type"], "segment");
    assert_eq!(lines[1]["source"], "interviewer");
    assert_eq!(lines[1]["start_time"], 12.5);
    assert_eq!(lines[2]["type"], "question");
    assert_eq!(lines[2]["kind"], "system_design");
    assert_eq!(lines[3]["type"], "suggestion");
    assert_eq!(lines[3]["phase"], "fast");
    assert_eq!(
        lines[3]["payload"],
        r#"{"question":"How would you design a rate limiter?"}"#
    );
}

#[test]
fn foreign_keys_are_enforced() {
    let store = store();

    // A segment pointing at a missing session is rejected.
    let orphan = store.insert_segment(&NewSegment {
        session_id: 999,
        source: Source::Unknown,
        start_time: 0.0,
        end_time: 1.0,
        text: "nobody said this".to_string(),
        is_final: true,
        avg_logprob: -0.3,
    });
    assert!(matches!(orphan, Err(StoreError::Sqlite(_))));

    // So is a suggestion pointing at a missing question.
    let orphan = store.insert_suggestion(&NewSuggestion {
        question_id: 999,
        phase: Phase::Fast,
        payload: "{}".to_string(),
        created_at: 0,
    });
    assert!(matches!(orphan, Err(StoreError::Sqlite(_))));

    // Deleting a session cascades to its rows.
    let (session_id, segment_id) = session_with_segment(&store);
    let question_id = question_with_suggestion(&store, session_id, segment_id);
    store
        .execute_raw("DELETE FROM sessions WHERE id = ?1", session_id)
        .unwrap();
    assert!(store.segments(session_id).unwrap().is_empty());
    assert!(store.questions(session_id).unwrap().is_empty());
    assert!(
        store
            .suggestions_for_question(question_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn check_constraints_reject_bad_enum_text() {
    let store = store();
    let (session_id, _) = session_with_segment(&store);
    let bad = store.execute_raw(
        "INSERT INTO segments (session_id, source, start_time, end_time, text, is_final, avg_logprob)
         VALUES (?1, 'them', 0.0, 1.0, 'x', 1, 0.0)",
        session_id,
    );
    assert!(matches!(bad, Err(StoreError::Sqlite(_))));
}

#[test]
fn delete_session_removes_its_rows_and_compact_drops_the_text_from_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    let store = Store::open(&path).unwrap();
    let (keep, keep_segment) = session_with_segment(&store);
    let (gone, gone_segment) = session_with_segment(&store);
    question_with_suggestion(&store, keep, keep_segment);
    question_with_suggestion(&store, gone, gone_segment);
    store
        .insert_segment(&NewSegment {
            session_id: gone,
            source: Source::You,
            start_time: 20.0,
            end_time: 22.0,
            text: "purge-me-marker-7f3a".to_string(),
            is_final: true,
            avg_logprob: -0.1,
        })
        .unwrap();
    assert_eq!(store.session_ids().unwrap(), [keep, gone]);

    assert!(store.delete_session(gone).unwrap());
    assert!(!store.delete_session(gone).unwrap(), "already gone");
    store.compact().unwrap();

    assert_eq!(store.session_ids().unwrap(), [keep]);
    assert_eq!(store.counts().unwrap(), (1, 1, 1, 1));
    drop(store);
    let bytes = std::fs::read(&path).unwrap();
    let marker = b"purge-me-marker-7f3a";
    assert!(
        !bytes.windows(marker.len()).any(|w| w == marker),
        "deleted text is not left in free pages"
    );
}

#[test]
fn a_v1_database_upgrades_to_v2_keeping_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    // A database a v1 build wrote: schema v1 only, one session with a question.
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrate::migrate_to(&mut conn, 1).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, started_at, label) VALUES (1, 1700000000, 'v1');
             INSERT INTO segments (id, session_id, source, start_time, end_time, text, is_final,
                                   avg_logprob)
                 VALUES (1, 1, 'interviewer', 12.5, 18.0, 'How would you shard it?', 1, -0.2);
             INSERT INTO questions (id, session_id, segment_id, score, created_at)
                 VALUES (1, 1, 1, 4, 1700000018);",
        )
        .unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), 2);
    let session = store.session(1).unwrap().unwrap();
    assert_eq!((session.label.as_str(), session.overruns), ("v1", 0));
    let q = &store.questions(1).unwrap()[0];
    assert_eq!(q.score, 4);
    assert_eq!(
        (q.emitted_at, q.question_at, q.approach_at),
        (None, None, None),
        "a v1 row reads back as not measured, not as zero"
    );
    assert!(store.cpu_samples(1).unwrap().is_empty());
    assert!(
        store
            .schema_object_names()
            .unwrap()
            .contains(&"idx_cpu_samples_session".to_string())
    );
}

#[test]
fn question_marks_keep_the_first_time_and_cpu_samples_round_trip() {
    let store = store();
    let (session_id, segment_id) = session_with_segment(&store);
    let q = question_with_suggestion(&store, session_id, segment_id);
    store.mark_question(q, Mark::Emitted, 18.05).unwrap();
    store.mark_question(q, Mark::Question, 18.7).unwrap();
    store.mark_question(q, Mark::Approach, 19.1).unwrap();
    // A regenerate produces the lines again later; the first showing stands.
    store.mark_question(q, Mark::Question, 40.0).unwrap();
    store.mark_question(q, Mark::Approach, 40.5).unwrap();
    let row = &store.questions(session_id).unwrap()[0];
    assert_eq!(
        (row.emitted_at, row.question_at, row.approach_at),
        (Some(18.05), Some(18.7), Some(19.1))
    );

    for (t, cpu, whisper) in [(5.0, 0.4, 0.0), (10.0, 0.9, 0.3)] {
        store
            .insert_cpu_sample(&NewCpuSample {
                session_id,
                at: t,
                wall: t,
                cpu,
                whisper,
            })
            .unwrap();
    }
    let cpu = store.cpu_samples(session_id).unwrap();
    assert_eq!(cpu.len(), 2);
    assert_eq!(
        (cpu[1].at, cpu[1].wall, cpu[1].cpu, cpu[1].whisper),
        (10.0, 10.0, 0.9, 0.3)
    );

    store.set_session_overruns(session_id, 3).unwrap();
    assert_eq!(store.session(session_id).unwrap().unwrap().overruns, 3);
    assert_eq!(store.latest_session_id().unwrap(), Some(session_id));

    // The export carries the new fields and the samples.
    let mut out = Vec::new();
    store.export_session_jsonl(session_id, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("\"approach_at\":19.1"), "{text}");
    assert_eq!(text.matches("\"type\":\"cpu\"").count(), 2, "{text}");
    // Deleting the session removes its samples too.
    store.delete_session(session_id).unwrap();
    assert!(store.cpu_samples(session_id).unwrap().is_empty());
}

#[test]
fn no_session_means_no_latest() {
    assert_eq!(store().latest_session_id().unwrap(), None);
}
