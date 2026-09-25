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
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
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
fn a_v2_database_upgrades_to_v3_and_questions_record_their_coding_solutions_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrate::migrate_to(&mut conn, 2).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, started_at, label) VALUES (1, 1700000000, 'v2');
             INSERT INTO segments (id, session_id, source, start_time, end_time, text, is_final,
                                   avg_logprob)
                 VALUES (1, 1, 'interviewer', 12.5, 18.0, 'Reverse a linked list.', 1, -0.2);
             INSERT INTO questions (id, session_id, segment_id, score, created_at)
                 VALUES (1, 1, 1, 4, 1700000018);",
        )
        .unwrap();
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(
        store.questions(1).unwrap()[0].coding_solutions,
        None,
        "a v2 row reads back as unknown"
    );

    store.set_question_coding_solutions(1, "full").unwrap();
    assert_eq!(
        store.questions(1).unwrap()[0].coding_solutions.as_deref(),
        Some("full")
    );
    assert!(
        store.set_question_coding_solutions(1, "solutions").is_err(),
        "only hints / full are stored"
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

// --- Context sources (v4, #53) -----------------------------------------------------------------

fn doc(kind: &str, path: &str) -> crate::NewSourceDoc {
    crate::NewSourceDoc {
        kind: kind.into(),
        path: path.into(),
        added_at: 1_700_000_000,
        bytes: 1234,
        tokens: 400,
    }
}

fn chunks(texts: &[&str]) -> Vec<crate::NewChunk> {
    texts
        .iter()
        .map(|t| crate::NewChunk {
            text: (*t).into(),
            tokens: 3,
        })
        .collect()
}

#[test]
fn a_v3_database_upgrades_to_v4_keeping_its_sessions_and_gaining_sources() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrate::migrate_to(&mut conn, 3).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, started_at, label) VALUES (1, 1700000000, 'v3');
             INSERT INTO segments (id, session_id, source, start_time, end_time, text, is_final,
                                   avg_logprob)
                 VALUES (1, 1, 'interviewer', 1.0, 2.0, 'Tell me about yourself.', 1, -0.2);",
        )
        .unwrap();
        let names: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!names.contains(&"sources".to_string()), "v3 has no sources");
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.session(1).unwrap().unwrap().label, "v3");
    assert_eq!(store.segments(1).unwrap().len(), 1);
    assert_eq!(store.source_counts().unwrap(), (0, 0));
    let objects = store.schema_object_names().unwrap();
    for name in ["sources", "chunks", "idx_sources_kind", "idx_chunks_source"] {
        assert!(objects.contains(&name.to_string()), "missing {name}");
    }
    store
        .replace_source(&doc("resume", "/r.md"), &chunks(&["a", "b"]), true)
        .unwrap();
    assert_eq!(store.source_counts().unwrap(), (1, 2));
}

#[test]
fn a_source_round_trips_with_its_chunks_in_order() {
    let store = store();
    let w = store
        .replace_source(
            &doc("notes", "/n/a.md"),
            &chunks(&["one", "two", "three"]),
            false,
        )
        .unwrap();
    assert!(w.replaced.is_empty());
    let all = store.sources().unwrap();
    assert_eq!(all.len(), 1);
    let s = &all[0];
    assert_eq!(
        (
            s.id,
            s.kind.as_str(),
            s.path.as_str(),
            s.bytes,
            s.tokens,
            s.chunks
        ),
        (w.id, "notes", "/n/a.md", 1234, 400, 3)
    );
    let texts: Vec<(i64, String)> = store
        .source_chunks(w.id)
        .unwrap()
        .into_iter()
        .map(|c| (c.idx, c.text))
        .collect();
    assert_eq!(
        texts,
        [(0, "one".into()), (1, "two".into()), (2, "three".into())]
    );
}

#[test]
fn re_adding_a_path_replaces_its_chunks() {
    let store = store();
    let first = store
        .replace_source(&doc("notes", "/n/a.md"), &chunks(&["old", "old"]), false)
        .unwrap();
    let second = store
        .replace_source(&doc("notes", "/n/a.md"), &chunks(&["new"]), false)
        .unwrap();
    assert_eq!(second.replaced.len(), 1);
    assert_eq!(second.replaced[0].id, first.id);
    assert_ne!(second.id, first.id, "a stale id never names the new source");
    assert_eq!(store.source_counts().unwrap(), (1, 1));
    assert_eq!(store.source_chunks(second.id).unwrap()[0].text, "new");
    assert!(store.source_chunks(first.id).unwrap().is_empty());
}

#[test]
fn a_single_document_kind_replaces_the_previous_one_and_folders_accumulate() {
    let store = store();
    store
        .replace_source(&doc("resume", "/old.pdf"), &chunks(&["x"]), true)
        .unwrap();
    store
        .replace_source(&doc("notes", "/n/a.md"), &chunks(&["x"]), false)
        .unwrap();
    store
        .replace_source(&doc("notes", "/n/b.md"), &chunks(&["x"]), false)
        .unwrap();
    let w = store
        .replace_source(&doc("resume", "/new.md"), &chunks(&["y"]), true)
        .unwrap();
    assert_eq!(w.replaced.len(), 1);
    assert_eq!(w.replaced[0].path, "/old.pdf");
    let paths: Vec<String> = store
        .sources()
        .unwrap()
        .into_iter()
        .map(|s| s.path)
        .collect();
    assert_eq!(
        paths,
        ["/n/a.md", "/n/b.md", "/new.md"],
        "kind order, then path"
    );
}

#[test]
fn deleting_a_source_deletes_its_chunks_and_nothing_else() {
    let store = store();
    let a = store
        .replace_source(&doc("notes", "/n/a.md"), &chunks(&["1", "2"]), false)
        .unwrap();
    store
        .replace_source(&doc("notes", "/n/b.md"), &chunks(&["3"]), false)
        .unwrap();
    assert!(store.delete_source(a.id).unwrap());
    assert!(!store.delete_source(a.id).unwrap(), "already gone");
    assert_eq!(store.source_counts().unwrap(), (1, 1));
}

#[test]
fn deleting_sessions_leaves_sources_alone() {
    let store = store();
    let (session_id, _) = session_with_segment(&store);
    store
        .replace_source(&doc("resume", "/r.md"), &chunks(&["a"]), true)
        .unwrap();
    assert!(store.delete_session(session_id).unwrap());
    store.compact().unwrap();
    assert_eq!(store.source_counts().unwrap(), (1, 1));
}

#[test]
fn an_unknown_source_kind_is_rejected_and_nothing_is_half_written() {
    let store = store();
    assert!(matches!(
        store.replace_source(&doc("diary", "/d.md"), &chunks(&["a"]), false),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(store.source_counts().unwrap(), (0, 0));
}

// --- Job-description summary cache and context reads (v5, #54) ----------------------------------

#[test]
fn a_v4_database_upgrades_to_v5_keeping_its_sources_and_gaining_the_summary_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrate::migrate_to(&mut conn, 4).unwrap();
        conn.execute_batch(
            "INSERT INTO sources (id, kind, path, added_at, bytes, tokens)
                 VALUES (1, 'resume', '/r.md', 1700000000, 10, 4);
             INSERT INTO chunks (source_id, idx, text, tokens) VALUES (1, 0, 'Rust and Kafka', 4);",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.source_counts().unwrap(), (1, 1));
    assert!(
        store
            .schema_object_names()
            .unwrap()
            .contains(&"jd_summaries".to_string())
    );
    assert_eq!(store.jd_summary("abc").unwrap(), None);
}

#[test]
fn the_jd_summary_round_trips_by_hash_and_a_new_one_replaces_the_old() {
    let store = store();
    let bullets = vec![
        "Go or Rust services".to_string(),
        "Postgres sharding,\nRedis caching".to_string(),
        "   ".to_string(),
    ];
    store.put_jd_summary("h1", &bullets, 1).unwrap();
    assert_eq!(
        store.jd_summary("h1").unwrap(),
        Some(vec![
            "Go or Rust services".to_string(),
            "Postgres sharding, Redis caching".to_string(),
        ]),
        "a newline inside a bullet does not split it; blank bullets are dropped"
    );
    store
        .put_jd_summary("h2", &["Kafka".to_string()], 2)
        .unwrap();
    assert_eq!(store.jd_summary("h1").unwrap(), None, "one JD at a time");
    assert_eq!(
        store.jd_summary("h2").unwrap(),
        Some(vec!["Kafka".to_string()])
    );
    store
        .put_jd_summary("h2", &["Kafka streams".to_string()], 3)
        .unwrap();
    assert_eq!(
        store.jd_summary("h2").unwrap(),
        Some(vec!["Kafka streams".to_string()]),
        "the same hash is overwritten, not duplicated"
    );
}

#[test]
fn all_chunks_carry_their_source_and_the_signature_moves_on_every_change() {
    let store = store();
    let empty = store.sources_signature().unwrap();
    assert_eq!(empty, crate::SourcesSignature::default());
    store
        .replace_source(&doc("resume", "/r.md"), &chunks(&["a", "b"]), true)
        .unwrap();
    store
        .replace_source(&doc("notes", "/n/x.md"), &chunks(&["c"]), false)
        .unwrap();
    let all = store.all_chunks().unwrap();
    let got: Vec<(&str, &str, i64, &str)> = all
        .iter()
        .map(|c| (c.kind.as_str(), c.path.as_str(), c.idx, c.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("resume", "/r.md", 0, "a"),
            ("resume", "/r.md", 1, "b"),
            ("notes", "/n/x.md", 0, "c"),
        ]
    );
    let two = store.sources_signature().unwrap();
    assert_eq!((two.sources, two.chunks), (2, 3));
    // Replacing the resume keeps the counts equal but raises the id.
    store
        .replace_source(&doc("resume", "/r.md"), &chunks(&["x", "y"]), true)
        .unwrap();
    let replaced = store.sources_signature().unwrap();
    assert_eq!((replaced.sources, replaced.chunks), (2, 3));
    assert_ne!(replaced, two, "a replacement is a change");
}

#[test]
fn a_read_only_open_reads_the_context_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dmn.sqlite");
    assert!(
        Store::open_read_only(&path).unwrap().is_none(),
        "no database, no store, and none is created"
    );
    assert!(!path.exists());
    {
        let rw = Store::open(&path).unwrap();
        rw.replace_source(&doc("resume", "/r.md"), &chunks(&["a"]), true)
            .unwrap();
        rw.put_jd_summary("h", &["Rust".to_string()], 1).unwrap();
    }
    let ro = Store::open_read_only(&path).unwrap().unwrap();
    assert_eq!(ro.all_chunks().unwrap().len(), 1);
    assert_eq!(ro.jd_summary("h").unwrap(), Some(vec!["Rust".to_string()]));
    assert!(
        ro.put_jd_summary("h", &["x".to_string()], 2).is_err(),
        "read-only means read-only"
    );
    assert!(ro.insert_session(&NewSession::new(1, "x")).is_err());

    // An older database is not migrated by a read-only open.
    let old = dir.path().join("old.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&old).unwrap();
        crate::migrate::migrate_to(&mut conn, 4).unwrap();
    }
    assert!(Store::open_read_only(&old).unwrap().is_none());
    let conn = rusqlite::Connection::open(&old).unwrap();
    let v: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 4, "left alone");
}
