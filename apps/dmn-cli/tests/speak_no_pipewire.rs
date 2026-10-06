//! `dmn speak "text"` without a PipeWire server (#108): the error names the problem and the
//! `--out` alternative, nothing panics, and nothing is asked of the TTS server. Runs the real
//! binary with `PIPEWIRE_REMOTE` pointed at a socket that does not exist, which is how a
//! machine without PipeWire looks to libpipewire, and with every XDG directory under a
//! tempdir, so `~/.config/dmn` and `~/.local/share/dmn` are never touched. Works with or
//! without a server on the machine, so it is not `#[ignore]`d.

// CLAUDE.md: unwrap/expect are allowed in tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn speak_without_pipewire_names_the_problem_and_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config/dmn");
    std::fs::create_dir_all(&config_dir).unwrap();
    // A closed port: PipeWire must be reported before the server is ever contacted.
    std::fs::write(
        config_dir.join("config.toml"),
        "[speak]\nenabled = true\ntts_url = \"http://127.0.0.1:1\"\nvoice_ref = \"/nowhere/voice.wav\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dmn"))
        .args(["speak", "hola"])
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_STATE_HOME", dir.path().join("state"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env(
            "PIPEWIRE_REMOTE",
            dir.path().join("no-such-pipewire-socket"),
        )
        .output()
        .expect("the dmn binary runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "exit status: {:?}\n{stderr}",
        out.status
    );
    assert!(
        !stderr.contains("panicked"),
        "a missing PipeWire is an error, not a panic:\n{stderr}"
    );
    assert!(
        stderr.contains("dmn: cannot reach PipeWire ("),
        "names the problem:\n{stderr}"
    );
    assert!(stderr.contains("dmn-voice"), "names the node:\n{stderr}");
    assert!(
        stderr.contains("--out <file.wav>"),
        "offers the file alternative:\n{stderr}"
    );
    assert!(
        !stderr.contains("127.0.0.1:1"),
        "the TTS server was never contacted:\n{stderr}"
    );
}
