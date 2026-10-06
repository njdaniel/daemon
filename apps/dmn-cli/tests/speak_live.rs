//! The live end-to-end check of #108, as the issue's Verify section runs it by hand: enroll a
//! test voice, `dmn speak "Gracias por la oportunidad."` into `dmn-voice`, record the node with
//! `pw-record --target dmn-voice` while it speaks, and check what the call would have heard.
//! Prints the measured command-to-first-audio and total times for the PR.
//!
//! Needs a running PipeWire server and the TTS server at `http://localhost:4123`
//! (chatterbox-tts-api; README § Speak), so it is `#[ignore]`d (`just probe`); without the
//! server it says so and passes, since the server is an optional sidecar. Everything lives in
//! a tempdir (XDG dirs included: `~/.config/dmn` and `~/.local/share/dmn` are never touched),
//! the voice is enrolled as `dmn-test-108` from 15 s of the Spanish fixture's candidate and
//! deleted from the server's library afterwards, and the recording stays on a pipe.

// CLAUDE.md: unwrap/expect are allowed in tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TTS_URL: &str = "http://localhost:4123";
const VOICE: &str = "dmn-test-108";
const TEXT: &str = "Gracias por la oportunidad.";
const RATE: u32 = 48_000;

fn server_up() -> bool {
    Command::new("curl")
        .args(["-sf", "-m", "2", &format!("{TTS_URL}/health")])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn dmn(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dmn"));
    c.env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"));
    c
}

/// Seconds 47–62 of the fixture: continuous speech in the Piper Spanish voice.
fn cut_clip(from: &Path, to: &Path) {
    let mut r = hound::WavReader::open(from).unwrap();
    let spec = r.spec();
    assert_eq!(spec.channels, 1);
    let (start, len) = (
        47 * spec.sample_rate as usize,
        15 * spec.sample_rate as usize,
    );
    let samples: Vec<i16> = r
        .samples::<i16>()
        .skip(start)
        .take(len)
        .map(|s| s.unwrap())
        .collect();
    let mut w = hound::WavWriter::create(to, spec).unwrap();
    for s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
}

fn vmic_nodes() -> usize {
    let out = Command::new("pw-dump").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .matches("\"node.name\": \"dmn-voice\"")
        .count()
}

#[test]
#[ignore]
fn live_dmn_speak_plays_spanish_into_dmn_voice_and_pw_record_hears_it() {
    if !server_up() {
        eprintln!("skipped: no TTS server answering at {TTS_URL}/health (README § Speak)");
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/es/candidate.wav");
    if !fixture.exists() {
        eprintln!(
            "skipped: {} is missing (just fixtures-fetch)",
            fixture.display()
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config/dmn");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!("[speak]\nenabled = true\ntts_url = \"{TTS_URL}\"\n"),
    )
    .unwrap();
    let clip = dir.path().join("clip.wav");
    cut_clip(&fixture, &clip);

    // Enroll the test voice (replaces a leftover of the same name), and make sure it is
    // deleted again whatever happens below.
    struct DeleteVoice;
    impl Drop for DeleteVoice {
        fn drop(&mut self) {
            let _ = Command::new("curl")
                .args([
                    "-s",
                    "-X",
                    "DELETE",
                    &format!("{TTS_URL}/v1/voices/{VOICE}"),
                ])
                .stdout(Stdio::null())
                .status();
        }
    }
    let _cleanup = DeleteVoice;
    let out = dmn(dir.path())
        .args(["speak", "enroll", "--wav"])
        .arg(&clip)
        .args(["--name", VOICE, "--language", "es"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "enroll: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(vmic_nodes(), 0, "no dmn-voice before the command");
    let t0 = Instant::now();
    let speak = dmn(dir.path())
        .args(["speak", TEXT])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The node must be there well before the first audio (the server takes ~11 s), so that
    // the call, or here pw-record, can select it.
    let node_at = loop {
        if vmic_nodes() == 1 {
            break t0.elapsed();
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "dmn-voice never appeared"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut rec = Command::new("pw-record")
        .args(["--target", "dmn-voice", "--raw"])
        .args([
            "--rate",
            &RATE.to_string(),
            "--channels",
            "1",
            "--format",
            "f32",
        ])
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut rec_out = rec.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = rec_out.read_to_end(&mut buf);
        buf
    });

    let out = speak.wait_with_output().unwrap();
    let total = t0.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "dmn speak failed:\n{stderr}");
    std::thread::sleep(Duration::from_millis(300));
    let _ = rec.kill();
    let _ = rec.wait();
    let raw = reader.join().unwrap();
    assert_eq!(vmic_nodes(), 0, "dmn-voice is gone after the command");

    let samples: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let loud: Vec<usize> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.abs() > 0.005)
        .map(|(i, _)| i)
        .collect();
    assert!(!loud.is_empty(), "pw-record heard nothing from dmn-voice");
    let (first, last) = (loud[0], *loud.last().unwrap());
    let heard_secs = (last - first) as f32 / RATE as f32;
    let speech = &samples[first..=last];
    let rms = (speech.iter().map(|s| s * s).sum::<f32>() / speech.len() as f32).sqrt();
    // The report: "spoke N.N s of speech into dmn-voice at 48000 Hz in voice "...": first
    // audio after X s, done after Y s".
    let spoke: f32 = stdout
        .split("spoke ")
        .nth(1)
        .and_then(|s| s.split(" s").next())
        .and_then(|s| s.parse().ok())
        .expect("the report names the speech length");
    eprintln!(
        "dmn-voice appeared after {:.2} s; recording: {:.1} s heard (reported {spoke} s), rms {rms:.3}; wall clock {:.1} s\n{stdout}",
        node_at.as_secs_f64(),
        heard_secs,
        total.as_secs_f64()
    );
    assert!(stdout.contains("first audio after"), "{stdout}");
    assert!(rms > 0.02, "speech, not silence: rms {rms}");
    // The command plays the server's whole WAV, which opens and closes with some silence;
    // `heard_secs` spans only the audible part. The tone test in dmn-audio holds the exact
    // ±10 % on a signal with no silence of its own.
    assert!(
        heard_secs >= spoke * 0.6 && heard_secs <= spoke + 0.3,
        "heard {heard_secs:.2} s, the command spoke {spoke} s"
    );
}
