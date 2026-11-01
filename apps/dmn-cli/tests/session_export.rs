//! Readable and JSONL exports through the real CLI, using isolated XDG directories and SQLite.
//! No microphone, PipeWire or model is needed, including for crash-interrupted sessions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

use dmn_store::{NewSegment, NewSession, Source, Store};

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dmn"))
        .args(args)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .output()
        .unwrap()
}

#[test]
fn readable_export_and_default_jsonl_work_from_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let data = root.join("data/dmn");
    std::fs::create_dir_all(&data).unwrap();
    let store = Store::open(data.join("dmn.sqlite")).unwrap();
    let id = store
        .insert_session(&NewSession::new(1_700_000_000, "recovered"))
        .unwrap();
    store
        .insert_segment(&NewSegment {
            session_id: id,
            source: Source::You,
            start_time: 65.0,
            end_time: 68.0,
            text: "These words were saved before the crash.".into(),
            is_final: true,
            avg_logprob: -0.2,
            language: None,
        })
        .unwrap();
    let id = id.to_string();
    let text_path = data.join(format!("sessions/{id}.txt"));
    let out = cli(root, &["sessions", "export", &id, "--format", "text"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(&text_path.display().to_string()),
        "{stdout}"
    );
    assert!(
        std::fs::read_to_string(&text_path)
            .unwrap()
            .ends_with("[01:05] You: These words were saved before the crash.\n")
    );

    let jsonl_path = data.join(format!("sessions/{id}.jsonl"));
    let out = cli(root, &["sessions", "export", &id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let original = std::fs::read_to_string(&jsonl_path).unwrap();
    let rows: Vec<serde_json::Value> = original
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[0]["type"], "session");
    assert_eq!(rows[1]["type"], "segment");
    assert_eq!(rows[0]["ended_at"], serde_json::Value::Null);
    let out = cli(root, &["sessions", "export", &id, "--format", "jsonl"]);
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&jsonl_path).unwrap(), original);

    let out = cli(root, &["sessions", "export", "999", "--format", "text"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no session 999"));
    assert!(!data.join("sessions/999.txt").exists());
    let out = cli(root, &["sessions", "purge", &id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text_path.exists());
    assert!(!jsonl_path.exists());
}
