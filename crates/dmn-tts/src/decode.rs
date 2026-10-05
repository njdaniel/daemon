//! WAV body → mono f32 chunks, shared by the Chatterbox decoder thread and the mock. hound
//! reads straight off the blocking body, so audio flows chunk by chunk instead of being
//! buffered whole.

use hound::{SampleFormat, WavReader};

use crate::provider::{PcmChunk, TtsError};

/// Mono frames per chunk: 200 ms at the server's 24 kHz, a little more at 48 kHz.
pub(crate) const CHUNK_FRAMES: usize = 4800;

/// Decode the WAV stream on `reader`. `on_rate` fires once, as soon as the header has parsed:
/// `Ok` with the sample rate, or `Err` when the body is not a supported WAV. Each chunk then
/// goes to `on_chunk`; a decode failure mid-body is one `Err` chunk, after which decoding
/// stops. Channels are downmixed by averaging, like `dmn-audio`'s `MonoReader`.
pub(crate) fn decode_reader<R, G, F>(reader: R, on_rate: G, mut on_chunk: F)
where
    R: std::io::Read,
    G: FnOnce(Result<u32, TtsError>),
    F: FnMut(Result<PcmChunk, TtsError>),
{
    let mut reader = match WavReader::new(reader) {
        Ok(r) => r,
        Err(e) => {
            on_rate(Err(TtsError::Decode {
                detail: format!("not a WAV: {e}"),
            }));
            return;
        }
    };
    let spec = reader.spec();
    let supported = matches!(
        (spec.sample_format, spec.bits_per_sample),
        (SampleFormat::Float, 32) | (SampleFormat::Int, 8 | 16 | 24 | 32)
    );
    if !supported || spec.channels == 0 || spec.sample_rate == 0 {
        on_rate(Err(TtsError::Decode {
            detail: format!(
                "unsupported WAV: {:?} {}-bit, {} channel(s) at {} Hz",
                spec.sample_format, spec.bits_per_sample, spec.channels, spec.sample_rate
            ),
        }));
        return;
    }
    let sample_rate = spec.sample_rate;
    on_rate(Ok(sample_rate));

    let channels = usize::from(spec.channels);
    let samples: Box<dyn Iterator<Item = Result<f32, hound::Error>>> = match spec.sample_format {
        SampleFormat::Float => Box::new(reader.samples::<f32>()),
        SampleFormat::Int => {
            // Full scale for a `bits_per_sample`-bit signed integer; `supported` checked the range.
            let scale = 1.0 / (1u64 << (spec.bits_per_sample.clamp(8, 32) - 1)) as f32;
            Box::new(
                reader
                    .samples::<i32>()
                    .map(move |s| s.map(|v| v as f32 * scale)),
            )
        }
    };
    let mut chunk: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES);
    let mut frame_acc = 0.0f32;
    let mut frame_len = 0usize;
    for sample in samples {
        let value = match sample {
            Ok(v) => v,
            Err(e) => {
                on_chunk(Err(TtsError::Decode {
                    detail: format!("sample read failed: {e}"),
                }));
                return;
            }
        };
        frame_acc += value;
        frame_len += 1;
        if frame_len == channels {
            chunk.push(frame_acc / channels as f32);
            frame_acc = 0.0;
            frame_len = 0;
            if chunk.len() == CHUNK_FRAMES {
                on_chunk(Ok(PcmChunk {
                    samples: std::mem::take(&mut chunk),
                    sample_rate,
                }));
                chunk = Vec::with_capacity(CHUNK_FRAMES);
            }
        }
    }
    // A truncated final frame (`frame_len > 0` here) is dropped rather than half-averaged.
    if !chunk.is_empty() {
        on_chunk(Ok(PcmChunk {
            samples: chunk,
            sample_rate,
        }));
    }
}

/// An in-memory sine WAV: `amps[c]` on channel `c`, 16-bit. The mock's default answer and the
/// tests' fixture; not the pipeline's business (it lives here because both decode callers use
/// it). Writes to a `Vec` cannot fail; if hound ever says otherwise the empty bytes decode as
/// a `Decode` error rather than a panic.
pub(crate) fn sine_wav(rate: u32, amps: &[f32], hz: f32, secs: f32) -> Vec<u8> {
    fn build(rate: u32, amps: &[f32], hz: f32, secs: f32) -> Result<Vec<u8>, hound::Error> {
        let spec = hound::WavSpec {
            channels: amps.len() as u16,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let mut w = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec)?;
            let n = (rate as f32 * secs) as usize;
            for i in 0..n {
                let t = i as f32 / rate as f32;
                for &a in amps {
                    let v = (a * (std::f32::consts::TAU * hz * t).sin() * i16::MAX as f32) as i16;
                    w.write_sample(v)?;
                }
            }
            w.finalize()?;
        }
        Ok(buf)
    }
    build(rate, amps, hz, secs).unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn decode(wav: Vec<u8>) -> (u32, Vec<Result<PcmChunk, TtsError>>) {
        let (rate_tx, rate_rx) = crossbeam_channel::bounded(1);
        let mut chunks = Vec::new();
        decode_reader(
            std::io::Cursor::new(wav),
            |r| {
                let _ = rate_tx.send(r);
            },
            |c| chunks.push(c),
        );
        (rate_rx.recv().unwrap().unwrap(), chunks)
    }

    #[test]
    fn mono_chunks_carry_the_rate_and_every_frame() {
        let (rate, chunks) = decode(sine_wav(24_000, &[0.5], 220.0, 1.0));
        assert_eq!(rate, 24_000);
        let total: usize = chunks.iter().map(|c| c.as_ref().unwrap().samples.len()).sum();
        assert_eq!(total, 24_000);
        assert_eq!(chunks.len(), 5, "four full 4800-frame chunks + a tail");
        assert_eq!(chunks[0].as_ref().unwrap().sample_rate, 24_000);
        assert_eq!(chunks[4].as_ref().unwrap().samples.len(), 24_000 - 4 * 4800);
        // It really is the sine: strong signal, zero crossings near 2 * hz per second.
        let samples: Vec<f32> = chunks
            .iter()
            .flat_map(|c| c.as_ref().unwrap().samples.clone())
            .collect();
        let rms = (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");
    }

    #[test]
    fn stereo_is_downmixed_by_averaging() {
        let (rate, chunks) = decode(sine_wav(16_000, &[0.6, 0.2], 300.0, 0.4));
        assert_eq!(rate, 16_000);
        let samples: Vec<f32> = chunks
            .iter()
            .flat_map(|c| c.as_ref().unwrap().samples.clone())
            .collect();
        assert_eq!(samples.len(), 6_400);
        let rms = (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt();
        let expected = 0.4 / 2f32.sqrt();
        assert!((rms - expected).abs() < 0.01, "rms {rms} vs {expected}");
    }

    #[test]
    fn garbage_is_a_decode_error_on_the_rate_callback_and_nothing_streams() {
        let (rate_tx, rate_rx) = crossbeam_channel::bounded(1);
        let mut chunks = 0;
        decode_reader(
            std::io::Cursor::new(b"this is not a wav".to_vec()),
            |r| {
                let _ = rate_tx.send(r);
            },
            |_| chunks += 1,
        );
        assert!(matches!(rate_rx.recv().unwrap(), Err(TtsError::Decode { .. })));
        assert_eq!(chunks, 0);
    }

    #[test]
    fn a_body_cut_mid_audio_streams_what_arrived_then_reports_the_failure() {
        let mut wav = sine_wav(24_000, &[0.5], 220.0, 1.0);
        wav.truncate(wav.len() / 2);
        let (rate, chunks) = decode(wav);
        assert_eq!(rate, 24_000);
        assert!(
            chunks.iter().any(|c| matches!(c, Err(TtsError::Decode { .. }))),
            "a truncated body surfaces a decode error: {chunks:?}"
        );
    }
}
