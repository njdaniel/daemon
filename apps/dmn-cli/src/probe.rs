//! `dmn doctor --probe`: play a quiet 440 Hz tone through `pw-play` and capture it back three
//! ways (docs/SPEC.md § Audio capture on PipeWire, "Verify at M0"):
//!
//! 1. through the tone's own app stream by `target.object` (the per-app capture M1 relies on),
//! 2. through the default sink monitor (the fallback, with the "all system audio" badge),
//! 3. through the `pw-record` emergency fallback (informational).
//!
//! Each capture is judged by level and by how strongly 440 Hz dominates its neighbours
//! (Goertzel), so other audio playing at the same time does not produce a false pass. The tone
//! file lives in the temp dir for the few seconds the probe runs and is deleted after; nothing
//! captured is written anywhere.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dmn_audio::{
    AudioSource, CaptureState, CaptureTarget, FRAME_LEN, Frames, PipeWireContext, PipeWireSource,
    PwRecordSource, SAMPLE_RATE, Source, frame_channel,
};

use crate::doctor::Check;

/// `application.name` of the probe tone, so the probe captures exactly its own stream.
const PROBE_APP: &str = "dmn-probe";
const TONE_HZ: f32 = 440.0;
/// Long enough for all three captures; `pw-play` is killed as soon as the probe is done.
const TONE_SECS: f32 = 5.0;
/// Capture must reach -40 dBFS (the tone plays at volume 0.15 of a -6 dBFS sine, about -26 dBFS).
const MIN_RMS: f32 = 0.01;
/// 440 Hz must carry 100× (20 dB) the power at 300 Hz and 600 Hz.
const MIN_RATIO: f32 = 100.0;

/// Level and tonality of a capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tone {
    pub rms: f32,
    /// Power at 440 Hz over the larger of 300 Hz and 600 Hz.
    pub ratio: f32,
}

impl Tone {
    pub fn detected(&self) -> bool {
        self.rms >= MIN_RMS && self.ratio >= MIN_RATIO
    }

    pub fn dbfs(&self) -> f32 {
        20.0 * self.rms.max(1e-9).log10()
    }

    fn describe(&self) -> String {
        format!(
            "{:.0} dBFS, 440 Hz at {:.0} dB over neighbours",
            self.dbfs(),
            10.0 * self.ratio.max(1e-9).log10()
        )
    }
}

/// Goertzel power of `hz` in `w` at [`SAMPLE_RATE`].
pub fn goertzel(w: &[f32], hz: f32) -> f32 {
    let k = 2.0 * (2.0 * std::f32::consts::PI * hz / SAMPLE_RATE as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in w {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

/// Analyse the loudest 0.25 s window of `samples`.
pub fn analyse(samples: &[f32]) -> Tone {
    let win = SAMPLE_RATE as usize / 4;
    samples
        .chunks(win)
        .filter(|w| w.len() == win)
        .map(|w| {
            let rms = (w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32).sqrt();
            let off = goertzel(w, 300.0).max(goertzel(w, 600.0)).max(1e-12);
            Tone {
                rms,
                ratio: goertzel(w, TONE_HZ) / off,
            }
        })
        .fold(
            Tone {
                rms: 0.0,
                ratio: 0.0,
            },
            |best, t| if t.rms > best.rms { t } else { best },
        )
}

/// A 16-bit mono 48 kHz WAV of the probe tone at -6 dBFS.
pub fn tone_wav_bytes(secs: f32) -> Vec<u8> {
    const RATE: u32 = 48_000;
    let n = (RATE as f32 * secs) as u32;
    let data_len = n * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let v = (2.0 * std::f32::consts::PI * TONE_HZ * i as f32 / RATE as f32).sin();
        out.extend_from_slice(&((v * 0.5 * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

/// Kills `pw-play` and removes the tone file however the probe ends.
struct Player {
    child: Child,
    wav: PathBuf,
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.wav);
    }
}

fn start_tone() -> Result<Player, String> {
    let wav = std::env::temp_dir().join(format!("dmn-probe-{}.wav", std::process::id()));
    std::fs::File::create(&wav)
        .and_then(|mut f| f.write_all(&tone_wav_bytes(TONE_SECS)))
        .map_err(|e| format!("cannot write {}: {e}", wav.display()))?;
    let child = Command::new("pw-play")
        .args(["--volume", "0.15", "-P"])
        .arg(format!(
            "{{ application.name = \"{PROBE_APP}\" media.name = \"dmn probe tone\" }}"
        ))
        .arg(&wav)
        .spawn();
    match child {
        Ok(child) => Ok(Player { child, wav }),
        Err(e) => {
            let _ = std::fs::remove_file(&wav);
            Err(format!("cannot run pw-play: {e}"))
        }
    }
}

/// Drain `frames` for `dur`.
fn collect(frames: &mut Frames, dur: Duration) -> Vec<f32> {
    let mut all = Vec::new();
    let mut out = [0.0; FRAME_LEN];
    let t = Instant::now();
    while t.elapsed() < dur {
        while frames.pop_frame(&mut out) {
            all.extend_from_slice(&out);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    all
}

/// One [`PipeWireSource`] capture.
struct Captured {
    tone: Tone,
    /// The "capturing all system audio" badge was showing.
    badge: bool,
    /// Negotiated `(rate, channels)`.
    format: Option<(u32, u32)>,
}

/// Capture through a [`PipeWireSource`] for `dur` once it streams.
fn capture_pipewire(
    ctx: &Arc<PipeWireContext>,
    target: CaptureTarget,
    dur: Duration,
) -> Result<Captured, String> {
    let mut src = PipeWireSource::new(Arc::clone(ctx), Source::Interviewer, target);
    let status = src.status();
    let (sink, mut frames) = frame_channel(Source::Interviewer, 200);
    src.start(sink).map_err(|e| e.to_string())?;
    let t = Instant::now();
    while status.state() != CaptureState::Streaming {
        if status.state() == CaptureState::Error {
            let why = status.last_error().unwrap_or_default();
            let _ = src.stop();
            return Err(format!("stream error: {why}"));
        }
        if t.elapsed() > Duration::from_secs(2) {
            let state = status.state();
            let _ = src.stop();
            return Err(format!("stream never started ({state:?})"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let samples = collect(&mut frames, dur);
    let badge = status.capturing_all_system_audio();
    let format = status.format();
    src.stop().map_err(|e| e.to_string())?;
    Ok(Captured {
        tone: analyse(&samples),
        badge,
        format,
    })
}

fn capture_pw_record(target: &str, dur: Duration) -> Result<Tone, String> {
    let mut src = PwRecordSource::new(Source::Interviewer, Some(target), false);
    let (sink, mut frames) = frame_channel(Source::Interviewer, 200);
    src.start(sink).map_err(|e| e.to_string())?;
    // pw-record needs a moment to link; skip it rather than judging the start-up silence.
    let _ = collect(&mut frames, Duration::from_millis(200));
    let samples = collect(&mut frames, dur);
    src.stop().map_err(|e| e.to_string())?;
    Ok(analyse(&samples))
}

const APP_FIX: &str = "per-app capture does not work on this PipeWire/WirePlumber; set \
    [audio].app_stream = \"\" to capture the whole sink monitor (docs/SPEC.md § Open questions \
    and risks) and report your `pipewire --version` / `wireplumber --version`";
const MONITOR_FIX: &str = "check the default output device in your sound settings; \
    `pw-record -P '{ stream.capture.sink = true }' /tmp/t.wav` should record what plays";

/// Run the probe. Plays about 3 s of a quiet tone through the default output.
pub fn run() -> Vec<Check> {
    let ctx = match PipeWireContext::connect() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            return vec![Check::fail(
                "probe",
                e.to_string(),
                "systemctl --user start pipewire wireplumber",
            )];
        }
    };
    if !ctx.wait_for_sync(Duration::from_secs(2)).synced {
        return vec![Check::fail(
            "probe",
            "PipeWire registry did not sync within 2 s",
            "systemctl --user restart pipewire wireplumber",
        )];
    }
    let _player = match start_tone() {
        Ok(p) => p,
        Err(e) => {
            return vec![Check::fail(
                "probe",
                e,
                "sudo apt install pipewire-bin   # provides pw-play",
            )];
        }
    };

    // Wait for the tone's own Stream/Output/Audio node.
    let t = Instant::now();
    let mut table = ctx.nodes();
    let stream = loop {
        if let Some(s) = table.streams().find(|s| s.application == PROBE_APP) {
            break Some(s.clone());
        }
        if t.elapsed() > Duration::from_secs(2) {
            break None;
        }
        table = ctx.wait_for_change(table.generation, Duration::from_millis(100));
    };
    let Some(stream) = stream else {
        return vec![Check::fail(
            "probe",
            "the pw-play tone never appeared as an app stream",
            "run `pw-play /usr/share/sounds/freedesktop/stereo/bell.oga` and check `dmn devices`",
        )];
    };
    let target = stream.serial.clone().unwrap_or(stream.node_name.clone());

    let mut checks = Vec::new();

    checks.push(
        match capture_pipewire(
            &ctx,
            CaptureTarget::App {
                application: PROBE_APP.into(),
                monitor_fallback: false,
            },
            Duration::from_millis(800),
        ) {
            Ok(Captured { tone, format, .. }) if tone.detected() => {
                let fmt = format
                    .map(|(r, c)| format!(", {r} Hz × {c} ch"))
                    .unwrap_or_default();
                Check::pass(
                    "probe app stream",
                    format!("target.object={target}: {}{fmt}", tone.describe()),
                )
            }
            Ok(Captured { tone, .. }) => Check::fail(
                "probe app stream",
                format!("target.object={target}: no tone ({})", tone.describe()),
                APP_FIX,
            ),
            Err(e) => Check::fail("probe app stream", e, APP_FIX),
        },
    );

    checks.push(
        match capture_pipewire(&ctx, CaptureTarget::SinkMonitor, Duration::from_millis(600)) {
            Ok(Captured { tone, badge, .. }) if tone.detected() => Check::pass(
                "probe sink monitor",
                format!(
                    "{}; badge: {}",
                    tone.describe(),
                    if badge {
                        "capturing all system audio"
                    } else {
                        "MISSING"
                    }
                ),
            ),
            Ok(Captured { tone, .. }) => Check::fail(
                "probe sink monitor",
                format!("no tone ({})", tone.describe()),
                MONITOR_FIX,
            ),
            Err(e) => Check::fail("probe sink monitor", e, MONITOR_FIX),
        },
    );

    checks.push(
        match capture_pw_record(&target, Duration::from_millis(500)) {
            Ok(tone) if tone.detected() => Check::pass("probe pw-record", tone.describe()),
            Ok(tone) => Check::fail(
                "probe pw-record",
                format!("no tone ({})", tone.describe()),
                "the emergency fallback is unusable; PipeWireSource above is unaffected",
            ),
            Err(e) => Check::fail(
                "probe pw-record",
                e,
                "sudo apt install pipewire-bin   # provides pw-record",
            ),
        }
        .optional(),
    );

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(SAMPLE_RATE as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn a_440_hz_tone_is_detected() {
        let t = analyse(&sine(440.0, 0.05, 1.0));
        assert!(t.detected(), "{t:?}");
        assert!((t.dbfs() - (-29.0)).abs() < 1.0, "{}", t.dbfs());
    }

    #[test]
    fn other_frequencies_silence_and_noise_are_not() {
        assert!(
            !analyse(&sine(1000.0, 0.2, 1.0)).detected(),
            "1 kHz is not the probe"
        );
        assert!(
            !analyse(&sine(440.0, 0.001, 1.0)).detected(),
            "-66 dBFS is too quiet"
        );
        assert!(!analyse(&[0.0; 16_000]).detected(), "silence");
        // Deterministic pseudo-noise (LCG), loud enough to pass the level check.
        let mut x: u32 = 1;
        let noise: Vec<f32> = (0..16_000)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let t = analyse(&noise);
        assert!(
            t.rms > MIN_RMS && !t.detected(),
            "noise is loud but not tonal: {t:?}"
        );
    }

    #[test]
    fn tone_mixed_with_speech_level_noise_is_still_detected() {
        let mut x: u32 = 7;
        let mixed: Vec<f32> = sine(440.0, 0.05, 1.0)
            .into_iter()
            .map(|s| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                s + 0.01 * ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect();
        assert!(analyse(&mixed).detected());
    }

    #[test]
    fn tone_wav_has_a_valid_header_and_length() {
        let b = tone_wav_bytes(0.5);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(&b[36..40], b"data");
        let data_len = u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize;
        assert_eq!(data_len, 24_000 * 2);
        assert_eq!(b.len(), 44 + data_len);
    }

    /// Plays about 3 s of a quiet tone and captures it back. `just probe`.
    #[test]
    #[ignore]
    fn live_probe_passes_on_this_machine() {
        let checks = run();
        for c in &checks {
            assert!(c.ok, "{}: {} (fix: {:?})", c.name, c.detail, c.fix);
        }
        assert_eq!(checks.len(), 3);
    }
}
