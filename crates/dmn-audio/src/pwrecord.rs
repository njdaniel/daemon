//! Emergency fallback: capture through a `pw-record` subprocess behind the same [`AudioSource`]
//! trait (docs/SPEC.md § Decisions, Audio: "`pw-record` subprocess kept only as the `dmn doctor`
//! sanity check and emergency fallback"). Raw f32 PCM arrives on the child's stdout; a reader
//! thread cuts it into 20 ms frames. Nothing touches disk.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::frames::FrameSink;
use crate::source::{AudioError, AudioSource, Source};
use crate::{FRAME_LEN, SAMPLE_RATE};

/// A `pw-record` child writing 16 kHz mono F32LE to stdout.
pub struct PwRecordSource {
    source: Source,
    command: Option<Command>,
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    finished: Arc<AtomicBool>,
}

impl PwRecordSource {
    /// Capture `target` (an `object.serial` or `node.name`; `None` for the default source) or,
    /// with `capture_sink`, the default sink monitor.
    pub fn new(source: Source, target: Option<&str>, capture_sink: bool) -> Self {
        let mut cmd = Command::new("pw-record");
        cmd.args(["--rate", &SAMPLE_RATE.to_string()])
            .args(["--channels", "1", "--format", "f32", "--container", "raw"])
            .args(["--media-role", "Communication"]);
        if let Some(t) = target {
            cmd.args(["--target", t]);
        }
        if capture_sink {
            cmd.args(["-P", "{ stream.capture.sink = true }"]);
        }
        cmd.arg("-");
        Self::from_command(source, cmd)
    }

    /// Any command that writes raw little-endian f32 samples to stdout. Tests use this.
    pub fn from_command(source: Source, command: Command) -> Self {
        Self {
            source,
            command: Some(command),
            child: None,
            reader: None,
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AudioSource for PwRecordSource {
    fn source(&self) -> Source {
        self.source
    }

    fn start(&mut self, mut sink: FrameSink) -> Result<(), AudioError> {
        let mut cmd = self.command.take().ok_or(AudioError::AlreadyStarted)?;
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| AudioError::Subprocess(e.to_string()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| AudioError::Subprocess("no stdout".into()))?;
        let finished = Arc::clone(&self.finished);
        let reader = std::thread::Builder::new()
            .name("dmn-pw-record".into())
            .spawn(move || {
                let mut bytes = [0u8; 4096];
                let mut carry = [0u8; 4];
                let mut carried = 0usize;
                let mut frame = [0.0f32; FRAME_LEN];
                let mut fill = 0usize;
                loop {
                    let n = match stdout.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    for &b in &bytes[..n] {
                        carry[carried] = b;
                        carried += 1;
                        if carried == 4 {
                            frame[fill] = f32::from_le_bytes(carry);
                            carried = 0;
                            fill += 1;
                            if fill == FRAME_LEN {
                                sink.push_frame(&frame);
                                fill = 0;
                            }
                        }
                    }
                }
                finished.store(true, Ordering::Release);
            })
            .map_err(|e| AudioError::Spawn(e.to_string()))?;
        self.child = Some(child);
        self.reader = Some(reader);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        match self.reader.take().map(JoinHandle::join) {
            Some(Err(_)) => Err(AudioError::ThreadPanicked),
            _ => Ok(()),
        }
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

impl Drop for PwRecordSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::frame_channel;
    use std::time::{Duration, Instant};

    #[test]
    fn raw_f32_stdout_becomes_frames_and_eof_finishes() {
        // 10 frames of silence: 10 × 320 samples × 4 bytes.
        let mut cmd = Command::new("head");
        cmd.args(["-c", &(10 * FRAME_LEN * 4).to_string(), "/dev/zero"]);
        let mut src = PwRecordSource::from_command(Source::Interviewer, cmd);
        let (sink, mut frames) = frame_channel(Source::Interviewer, 16);
        src.start(sink).unwrap();
        let t = Instant::now();
        while !src.is_finished() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(src.is_finished());
        src.stop().unwrap();
        assert_eq!(frames.queued_frames(), 10);
        let mut out = [1.0; FRAME_LEN];
        assert!(frames.pop_frame(&mut out));
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn samples_split_across_reads_are_reassembled() {
        // printf writes 1.0f32 (00 00 80 3f) twice per frame slot; two frames' worth.
        let script = format!(
            "for i in $(seq 1 {}); do printf '\\000\\000\\200\\077'; done",
            2 * FRAME_LEN
        );
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]);
        let mut src = PwRecordSource::from_command(Source::You, cmd);
        let (sink, mut frames) = frame_channel(Source::You, 4);
        src.start(sink).unwrap();
        let t = Instant::now();
        while !src.is_finished() && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        src.stop().unwrap();
        let mut out = [0.0; FRAME_LEN];
        assert!(frames.pop_frame(&mut out));
        assert!(out.iter().all(|&s| s == 1.0), "{:?}", &out[..4]);
        assert!(frames.pop_frame(&mut out));
    }

    #[test]
    fn missing_program_is_a_typed_error() {
        let mut src = PwRecordSource::from_command(
            Source::You,
            Command::new("/nonexistent/dmn-no-such-binary"),
        );
        let (sink, _frames) = frame_channel(Source::You, 1);
        assert!(matches!(src.start(sink), Err(AudioError::Subprocess(_))));
    }
}
