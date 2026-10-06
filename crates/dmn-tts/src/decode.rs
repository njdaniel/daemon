//! WAV body → mono f32 chunks, shared by the Chatterbox decoder thread and the mock.
//!
//! A push decoder ([`WavDecoder`]) rather than hound: the bytes arrive in whatever pieces the
//! network hands over, so the header can be split anywhere and a sample can straddle two
//! pieces; and the server's streaming route (`/v1/audio/speech/stream`) sends a header whose
//! `data` length is `0xFFFFFFFF` because it does not know the length yet. hound computes the
//! sample count from that length, and rejects `0xFFFFFFFF` outright for 16-bit audio (not a
//! multiple of the sample size). Here a `data` length of `0` or `0xFFFFFFFF` means "until the
//! body ends"; any other length is honoured, and a body that ends short of it is an error.
//!
//! Supported: PCM integer 8 (unsigned), 16, 24 and 32 bit; IEEE float 32 and 64 bit; the same
//! inside `WAVE_FORMAT_EXTENSIBLE`; any channel count, downmixed by averaging like
//! `dmn-audio`'s `MonoReader`. Chunks other than `fmt ` and `data` (`LIST`, `fact`, `PEAK`)
//! are skipped, with their pad byte.

use crate::provider::{PcmChunk, TtsError};

/// Mono frames per chunk handed on: 200 ms at the server's 24 kHz.
pub(crate) const CHUNK_FRAMES: usize = 4800;
/// Bytes read from the body per `read` call.
const READ_BUF: usize = 16 * 1024;
/// A header (everything before the audio) larger than this is not a speech WAV.
const MAX_HEADER: usize = 1 << 20;
/// `data` lengths that mean "unknown, read to the end".
const STREAMED_LENGTHS: [u32; 2] = [0, 0xFFFF_FFFF];

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Int,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Format {
    encoding: Encoding,
    channels: usize,
    sample_rate: u32,
    /// Bytes per sample of one channel.
    width: usize,
}

impl Format {
    fn frame_bytes(&self) -> usize {
        self.channels * self.width
    }

    fn sample(&self, b: &[u8]) -> f32 {
        match (self.encoding, self.width) {
            (Encoding::Int, 1) => (f32::from(b[0]) - 128.0) / 128.0,
            (Encoding::Int, 2) => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            (Encoding::Int, 3) => {
                // Sign-extend the 24-bit value through the top of an i32.
                let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                v as f32 / 8_388_608.0
            }
            (Encoding::Int, _) => {
                i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
            }
            (Encoding::Float, 4) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (Encoding::Float, _) => {
                f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32
            }
        }
    }
}

#[derive(Debug)]
enum State {
    /// Before the `data` chunk: `fmt` once it has been seen.
    Header { fmt: Option<Format> },
    /// In the audio: `remaining` bytes of it, or `None` to read until the body ends.
    Data { fmt: Format, remaining: Option<u64> },
    /// The declared audio is all here; anything after it (trailing chunks) is ignored.
    Done { fmt: Format },
}

fn malformed(detail: impl Into<String>) -> TtsError {
    TtsError::Decode {
        detail: detail.into(),
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse a `fmt ` chunk body.
fn parse_fmt(b: &[u8]) -> Result<Format, TtsError> {
    if b.len() < 16 {
        return Err(malformed(format!(
            "fmt chunk is {} bytes, needs 16",
            b.len()
        )));
    }
    let mut tag = u16_at(b, 0);
    let channels = usize::from(u16_at(b, 2));
    let sample_rate = u32_at(b, 4);
    let bits = u16_at(b, 14);
    if tag == WAVE_FORMAT_EXTENSIBLE {
        // cbSize (2), valid bits (2), channel mask (4), then the sub-format GUID whose first two
        // bytes are the plain format tag.
        if b.len() < 40 {
            return Err(malformed("WAVE_FORMAT_EXTENSIBLE fmt chunk is short"));
        }
        tag = u16_at(b, 24);
    }
    let encoding = match (tag, bits) {
        (WAVE_FORMAT_PCM, 8 | 16 | 24 | 32) => Encoding::Int,
        (WAVE_FORMAT_IEEE_FLOAT, 32 | 64) => Encoding::Float,
        _ => {
            return Err(malformed(format!(
                "unsupported WAV encoding: format tag {tag:#06x}, {bits}-bit"
            )));
        }
    };
    if channels == 0 || sample_rate == 0 {
        return Err(malformed(format!(
            "WAV declares {channels} channel(s) at {sample_rate} Hz"
        )));
    }
    Ok(Format {
        encoding,
        channels,
        sample_rate,
        width: usize::from(bits / 8),
    })
}

/// Incremental WAV decoder: [`push`](Self::push) bytes as they arrive, get mono samples back.
#[derive(Debug)]
pub(crate) struct WavDecoder {
    state: State,
    /// Bytes not consumed yet: an incomplete header chunk, or a partial frame.
    pending: Vec<u8>,
    /// The `RIFF`/`WAVE` preamble has been checked.
    riff_seen: bool,
}

impl Default for WavDecoder {
    fn default() -> Self {
        Self {
            state: State::Header { fmt: None },
            pending: Vec::new(),
            riff_seen: false,
        }
    }
}

impl WavDecoder {
    /// The sample rate, once the header up to the `data` chunk has been read.
    pub(crate) fn sample_rate(&self) -> Option<u32> {
        match &self.state {
            State::Header { .. } => None,
            State::Data { fmt, .. } | State::Done { fmt } => Some(fmt.sample_rate),
        }
    }

    /// Feed the next bytes of the body; mono samples decoded from them are appended to `out`.
    pub(crate) fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) -> Result<(), TtsError> {
        if matches!(self.state, State::Done { .. }) {
            return Ok(());
        }
        self.pending.extend_from_slice(bytes);
        if matches!(self.state, State::Header { .. }) {
            self.parse_header()?;
        }
        self.decode_frames(out);
        Ok(())
    }

    /// The body has ended. An error when it ended inside the header or short of the declared
    /// audio length; a trailing partial frame of a streamed WAV is dropped.
    pub(crate) fn finish(&self) -> Result<(), TtsError> {
        match &self.state {
            State::Header { .. } if self.pending.is_empty() && !self.riff_seen => {
                Err(malformed("the server sent an empty body, not a WAV"))
            }
            State::Header { .. } => Err(malformed("the body ended inside the WAV header")),
            State::Data {
                remaining: Some(n), ..
            } if *n > 0 => Err(malformed(format!(
                "the body ended {n} bytes short of the WAV's declared audio length"
            ))),
            State::Data { .. } => {
                if !self.pending.is_empty() {
                    tracing::debug!(
                        bytes = self.pending.len(),
                        "streamed WAV ended mid-frame; dropped the partial frame"
                    );
                }
                Ok(())
            }
            State::Done { .. } => Ok(()),
        }
    }

    /// Consume header chunks from `pending` until the `data` chunk starts or more bytes are
    /// needed.
    fn parse_header(&mut self) -> Result<(), TtsError> {
        let mut at = 0usize;
        if !self.riff_seen {
            if self.pending.len() < 12 {
                return self.check_prefix();
            }
            if &self.pending[0..4] != b"RIFF" || &self.pending[8..12] != b"WAVE" {
                return Err(malformed(not_a_wav(&self.pending)));
            }
            self.riff_seen = true;
            at = 12;
        }
        let mut fmt = match self.state {
            State::Header { fmt } => fmt,
            _ => return Ok(()),
        };
        loop {
            if self.pending.len() < at + 8 {
                break;
            }
            let id = [
                self.pending[at],
                self.pending[at + 1],
                self.pending[at + 2],
                self.pending[at + 3],
            ];
            let len = u32_at(&self.pending, at + 4);
            if &id == b"data" {
                let Some(f) = fmt else {
                    return Err(malformed("the data chunk comes before the fmt chunk"));
                };
                let remaining = (!STREAMED_LENGTHS.contains(&len)).then_some(u64::from(len));
                at += 8;
                self.state = State::Data { fmt: f, remaining };
                self.pending.drain(..at);
                return Ok(());
            }
            if len == u32::MAX {
                return Err(malformed(format!(
                    "chunk {:?} before the audio has no length",
                    String::from_utf8_lossy(&id)
                )));
            }
            // Chunk bodies are padded to an even length.
            let body = len as usize + (len as usize & 1);
            if self.pending.len() < at + 8 + body {
                break;
            }
            if &id == b"fmt " {
                fmt = Some(parse_fmt(&self.pending[at + 8..at + 8 + len as usize])?);
            }
            at += 8 + body;
        }
        self.pending.drain(..at);
        self.state = State::Header { fmt };
        if self.pending.len() > MAX_HEADER {
            return Err(malformed("no audio within the first MiB of the WAV header"));
        }
        Ok(())
    }

    /// Fewer than 12 bytes so far: fail early only when what is there can no longer be a WAV.
    fn check_prefix(&self) -> Result<(), TtsError> {
        let n = self.pending.len().min(4);
        if self.pending[..n] != b"RIFF"[..n]
            || (self.pending.len() > 8 && self.pending[8..] != b"WAVE"[..self.pending.len() - 8])
        {
            return Err(malformed(not_a_wav(&self.pending)));
        }
        Ok(())
    }

    fn decode_frames(&mut self, out: &mut Vec<f32>) {
        let State::Data { fmt, remaining } = &mut self.state else {
            return;
        };
        let fmt = *fmt;
        let frame = fmt.frame_bytes();
        let mut usable = self.pending.len();
        if let Some(n) = remaining {
            usable = usable.min(usize::try_from(*n).unwrap_or(usize::MAX));
        }
        let frames = usable / frame;
        let used = frames * frame;
        out.reserve(frames);
        let scale = 1.0 / fmt.channels as f32;
        for f in self.pending[..used].chunks_exact(frame) {
            let sum: f32 = f.chunks_exact(fmt.width).map(|s| fmt.sample(s)).sum();
            out.push(sum * scale);
        }
        self.pending.drain(..used);
        if let Some(n) = remaining {
            *n -= used as u64;
            // A declared length that is not a whole number of frames leaves a sliver no frame
            // can use: the audio is over either way.
            if *n < frame as u64 {
                self.state = State::Done { fmt };
                self.pending.clear();
            }
        }
    }
}

/// What the first bytes of a non-WAV body were, for the error message.
fn not_a_wav(head: &[u8]) -> String {
    let shown: String = String::from_utf8_lossy(&head[..head.len().min(40)])
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("the body is not a WAV (it starts {:?})", shown.trim())
}

/// Decode the WAV body on `reader` as it arrives. `on_rate` fires once, as soon as the header
/// has been read: `Ok` with the sample rate, or `Err` when the body is not a supported WAV or
/// ends first. Each chunk then goes to `on_chunk`, which returns `false` when its receiver is
/// gone, which stops the read. A failure after the header is one `Err` chunk, then the end.
pub(crate) fn decode_reader<R, G, F>(mut reader: R, on_rate: G, mut on_chunk: F)
where
    R: std::io::Read,
    G: FnOnce(Result<u32, TtsError>),
    F: FnMut(Result<PcmChunk, TtsError>) -> bool,
{
    let mut decoder = WavDecoder::default();
    let mut on_rate = Some(on_rate);
    let mut buf = vec![0u8; READ_BUF];
    let mut samples: Vec<f32> = Vec::with_capacity(CHUNK_FRAMES * 2);
    // An error goes to `on_rate` while the header is still unread, to `on_chunk` after.
    let fail = |e: TtsError, on_rate: &mut Option<G>, on_chunk: &mut F| match on_rate.take() {
        Some(r) => r(Err(e)),
        None => {
            on_chunk(Err(e));
        }
    };
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return fail(
                    TtsError::BodyRead {
                        detail: crate::chatterbox::error_chain(&e),
                    },
                    &mut on_rate,
                    &mut on_chunk,
                );
            }
        };
        if let Err(e) = decoder.push(&buf[..n], &mut samples) {
            return fail(e, &mut on_rate, &mut on_chunk);
        }
        let Some(rate) = decoder.sample_rate() else {
            continue;
        };
        if let Some(r) = on_rate.take() {
            r(Ok(rate));
        }
        while samples.len() >= CHUNK_FRAMES {
            let rest = samples.split_off(CHUNK_FRAMES);
            let chunk = std::mem::replace(&mut samples, rest);
            if !on_chunk(Ok(PcmChunk {
                samples: chunk,
                sample_rate: rate,
            })) {
                return;
            }
        }
    }
    let finished = decoder.finish();
    if let (Some(rate), false) = (decoder.sample_rate(), samples.is_empty()) {
        if let Some(r) = on_rate.take() {
            r(Ok(rate));
        }
        if !on_chunk(Ok(PcmChunk {
            samples,
            sample_rate: rate,
        })) {
            return;
        }
    }
    match finished {
        Err(e) => fail(e, &mut on_rate, &mut on_chunk),
        Ok(()) => {
            // A complete header with no audio after it: the rate is still news.
            if let (Some(r), Some(rate)) = (on_rate.take(), decoder.sample_rate()) {
                r(Ok(rate));
            }
        }
    }
}

/// An in-memory sine WAV: `amps[c]` on channel `c`, 16-bit. The mock's default answer and the
/// tests' fixture. Writes to a `Vec` cannot fail; if hound ever says otherwise the empty bytes
/// decode as a `Decode` error rather than a panic.
pub(crate) fn sine_wav(rate: u32, amps: &[f32], hz: f32, secs: f32) -> Vec<u8> {
    fn build(rate: u32, amps: &[f32], hz: f32, secs: f32) -> Result<Vec<u8>, hound::Error> {
        let spec = hound::WavSpec {
            channels: amps.len() as u16,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
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

    /// Decode `wav` by pushing it in pieces of `piece` bytes; the decoded mono samples, or the
    /// first error.
    fn push_in_pieces(wav: &[u8], piece: usize) -> (Option<u32>, Result<Vec<f32>, TtsError>) {
        let mut d = WavDecoder::default();
        let mut out = Vec::new();
        for p in wav.chunks(piece) {
            if let Err(e) = d.push(p, &mut out) {
                return (d.sample_rate(), Err(e));
            }
        }
        (d.sample_rate(), d.finish().map(|()| out))
    }

    /// Through `decode_reader`, the path the provider uses.
    fn decode(wav: Vec<u8>) -> (Result<u32, TtsError>, Vec<Result<PcmChunk, TtsError>>) {
        let mut rate = None;
        let mut chunks = Vec::new();
        decode_reader(
            std::io::Cursor::new(wav),
            |r| rate = Some(r),
            |c| {
                chunks.push(c);
                true
            },
        );
        (rate.expect("on_rate always fires"), chunks)
    }

    /// A hand-built WAV: `fmt` body as given, optional chunks before `data`, the `data` length
    /// field as given (so tests can write 0 or 0xFFFFFFFF), then `audio`.
    fn wav(fmt: &[u8], before_data: &[(&[u8; 4], &[u8])], data_len: u32, audio: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&0x7FFF_FFDBu32.to_le_bytes()); // what the server's stream route writes
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        b.extend_from_slice(fmt);
        for (id, body) in before_data {
            b.extend_from_slice(*id);
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(body);
            if body.len() % 2 == 1 {
                b.push(0);
            }
        }
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        b.extend_from_slice(audio);
        b
    }

    fn pcm_fmt(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let align = channels * bits / 8;
        let mut f = Vec::new();
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        f.extend_from_slice(&align.to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f
    }

    fn extensible_fmt(sub: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut f = pcm_fmt(WAVE_FORMAT_EXTENSIBLE, channels, rate, bits);
        f.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        f.extend_from_slice(&bits.to_le_bytes()); // valid bits
        f.extend_from_slice(&0u32.to_le_bytes()); // channel mask
        f.extend_from_slice(&sub.to_le_bytes());
        f.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
        f
    }

    fn i16_audio(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn mono_chunks_carry_the_rate_and_every_frame() {
        let (rate, chunks) = decode(sine_wav(24_000, &[0.5], 220.0, 1.0));
        assert_eq!(rate.unwrap(), 24_000);
        let total: usize = chunks
            .iter()
            .map(|c| c.as_ref().unwrap().samples.len())
            .sum();
        assert_eq!(total, 24_000);
        assert_eq!(chunks.len(), 5, "four full 4800-frame chunks + a tail");
        assert!(
            chunks
                .iter()
                .all(|c| c.as_ref().unwrap().sample_rate == 24_000)
        );
        let samples: Vec<f32> = chunks
            .into_iter()
            .flat_map(|c| c.unwrap().samples)
            .collect();
        assert!((rms(&samples) - 0.5 / 2f32.sqrt()).abs() < 0.01);
    }

    /// The header split at every byte, and samples straddling odd piece boundaries, decode to
    /// exactly what the whole body decodes to.
    #[test]
    fn any_split_of_the_body_decodes_the_same() {
        let body = sine_wav(24_000, &[0.5], 220.0, 0.1);
        let (_, whole) = push_in_pieces(&body, body.len());
        let whole = whole.unwrap();
        assert_eq!(whole.len(), 2_400);
        for piece in [1, 2, 3, 7, 13, 44, 45, 4097] {
            let (rate, got) = push_in_pieces(&body, piece);
            assert_eq!(rate, Some(24_000), "piece {piece}");
            assert_eq!(got.unwrap(), whole, "piece {piece}");
        }
    }

    /// The server's streaming route: data length 0xFFFFFFFF (and RIFF length 0x7FFFFFDB), 16-bit
    /// mono, the body simply ends. A 0 length is read the same way; a partial last frame drops.
    #[test]
    fn a_streamed_header_with_unknown_length_reads_to_the_end() {
        let fmt = pcm_fmt(WAVE_FORMAT_PCM, 1, 24_000, 16);
        let mut audio = i16_audio(&[16_384, -16_384, 8_192]);
        audio.push(0x7F); // half a sample: the stream was cut mid-frame
        for len in [0xFFFF_FFFF, 0] {
            for piece in [1, 3, 64] {
                let (rate, got) = push_in_pieces(&wav(&fmt, &[], len, &audio), piece);
                assert_eq!(rate, Some(24_000));
                assert_eq!(
                    got.unwrap(),
                    vec![0.5, -0.5, 0.25],
                    "len {len:#x} piece {piece}"
                );
            }
        }
    }

    #[test]
    fn a_declared_length_stops_at_the_audio_and_ignores_trailing_chunks() {
        let fmt = pcm_fmt(WAVE_FORMAT_PCM, 1, 16_000, 16);
        let mut body = wav(&fmt, &[], 4, &i16_audio(&[16_384, -16_384]));
        body.extend_from_slice(b"LIST\x04\x00\x00\x00abcd");
        let (_, got) = push_in_pieces(&body, 5);
        assert_eq!(got.unwrap(), vec![0.5, -0.5]);
    }

    #[test]
    fn a_body_short_of_its_declared_length_streams_what_arrived_then_fails() {
        let mut body = sine_wav(24_000, &[0.5], 220.0, 1.0);
        body.truncate(body.len() / 2);
        let (rate, chunks) = decode(body);
        assert_eq!(rate.unwrap(), 24_000);
        let (ok, err): (Vec<_>, Vec<_>) = chunks.into_iter().partition(Result::is_ok);
        assert!(!ok.is_empty(), "what arrived was streamed");
        assert_eq!(err.len(), 1);
        let e = err.into_iter().next().unwrap().unwrap_err();
        assert!(matches!(e, TtsError::Decode { .. }), "{e}");
        assert!(e.to_string().contains("short"), "{e}");
    }

    #[test]
    fn float_and_extensible_and_wide_integer_wavs_decode() {
        // 32-bit float with a `fact` chunk, as libsndfile writes it.
        let floats: Vec<u8> = [0.5f32, -0.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let fact: &[u8] = &2u32.to_le_bytes();
        let body = wav(
            &pcm_fmt(WAVE_FORMAT_IEEE_FLOAT, 1, 24_000, 32),
            &[(b"fact", fact)],
            8,
            &floats,
        );
        assert_eq!(push_in_pieces(&body, 3).1.unwrap(), vec![0.5, -0.25]);

        // 64-bit float.
        let doubles: Vec<u8> = [0.75f64].iter().flat_map(|v| v.to_le_bytes()).collect();
        let body = wav(
            &pcm_fmt(WAVE_FORMAT_IEEE_FLOAT, 1, 24_000, 64),
            &[],
            8,
            &doubles,
        );
        assert_eq!(push_in_pieces(&body, 5).1.unwrap(), vec![0.75]);

        // WAVE_FORMAT_EXTENSIBLE around float, then around 16-bit PCM, behind an odd-sized LIST
        // chunk (its pad byte must be skipped).
        let list: &[u8] = b"INFOISFT";
        let odd: &[u8] = b"odd";
        let body = wav(
            &extensible_fmt(WAVE_FORMAT_IEEE_FLOAT, 1, 48_000, 32),
            &[(b"LIST", list), (b"junk", odd)],
            0xFFFF_FFFF,
            &floats,
        );
        let (rate, got) = push_in_pieces(&body, 1);
        assert_eq!((rate, got.unwrap()), (Some(48_000), vec![0.5, -0.25]));
        let body = wav(
            &extensible_fmt(WAVE_FORMAT_PCM, 1, 22_050, 16),
            &[],
            2,
            &i16_audio(&[-16_384]),
        );
        assert_eq!(push_in_pieces(&body, 2).1.unwrap(), vec![-0.5]);

        // 24-bit (negative, so the sign extension matters), 32-bit and unsigned 8-bit.
        let body = wav(
            &pcm_fmt(WAVE_FORMAT_PCM, 1, 24_000, 24),
            &[],
            3,
            &[0x00, 0x00, 0xC0],
        );
        assert_eq!(push_in_pieces(&body, 2).1.unwrap(), vec![-0.5]);
        let body = wav(
            &pcm_fmt(WAVE_FORMAT_PCM, 1, 24_000, 32),
            &[],
            4,
            &0x4000_0000i32.to_le_bytes(),
        );
        assert_eq!(push_in_pieces(&body, 3).1.unwrap(), vec![0.5]);
        let body = wav(&pcm_fmt(WAVE_FORMAT_PCM, 1, 8_000, 8), &[], 2, &[192, 64]);
        assert_eq!(push_in_pieces(&body, 1).1.unwrap(), vec![0.5, -0.5]);
    }

    /// What chatterbox-tts-api's `/v1/audio/speech` sent on 2026-10-05 (torchaudio's writer, via
    /// libsndfile): IEEE float, 32-bit, mono, 24 kHz, a 16-byte `fmt `, then `fact` (the frame
    /// count) and `PEAK` (version, timestamp, then a peak value and position per channel)
    /// before `data`. Split anywhere, including inside those chunks, it decodes the same.
    #[test]
    fn the_real_servers_float_wav_with_fact_and_peak_chunks_decodes() {
        let values = [0.5f32, -0.25, 0.125, -1.0];
        let audio: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let fact = (values.len() as u32).to_le_bytes();
        let mut peak = Vec::new();
        peak.extend_from_slice(&1u32.to_le_bytes()); // version
        peak.extend_from_slice(&1_759_680_000u32.to_le_bytes()); // timestamp
        peak.extend_from_slice(&1.0f32.to_le_bytes()); // peak value, channel 1
        peak.extend_from_slice(&3u32.to_le_bytes()); // its position
        let mut body = Vec::new();
        body.extend_from_slice(b"RIFF");
        body.extend_from_slice(&(4 + 24 + 12 + 24 + 8 + audio.len() as u32).to_le_bytes());
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&16u32.to_le_bytes());
        body.extend_from_slice(&pcm_fmt(WAVE_FORMAT_IEEE_FLOAT, 1, 24_000, 32));
        body.extend_from_slice(b"fact");
        body.extend_from_slice(&4u32.to_le_bytes());
        body.extend_from_slice(&fact);
        body.extend_from_slice(b"PEAK");
        body.extend_from_slice(&(peak.len() as u32).to_le_bytes());
        body.extend_from_slice(&peak);
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(audio.len() as u32).to_le_bytes());
        body.extend_from_slice(&audio);
        for piece in 1..=body.len() {
            let (rate, got) = push_in_pieces(&body, piece);
            assert_eq!(rate, Some(24_000), "piece {piece}");
            assert_eq!(got.unwrap(), values, "piece {piece}");
        }
        let (rate, chunks) = decode_in_reader(&body, 5);
        assert_eq!(rate.unwrap(), 24_000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].samples, values);
    }

    #[test]
    fn stereo_is_downmixed_by_averaging() {
        let (rate, chunks) = decode(sine_wav(16_000, &[0.6, 0.2], 300.0, 0.4));
        assert_eq!(rate.unwrap(), 16_000);
        let samples: Vec<f32> = chunks
            .into_iter()
            .flat_map(|c| c.unwrap().samples)
            .collect();
        assert_eq!(samples.len(), 6_400);
        assert!((rms(&samples) - 0.4 / 2f32.sqrt()).abs() < 0.01);

        // Exact on hand-made frames, streamed and split mid-frame.
        let fmt = pcm_fmt(WAVE_FORMAT_PCM, 2, 24_000, 16);
        let body = wav(
            &fmt,
            &[],
            0xFFFF_FFFF,
            &i16_audio(&[16_384, 0, -16_384, -16_384]),
        );
        assert_eq!(push_in_pieces(&body, 3).1.unwrap(), vec![0.25, -0.5]);
    }

    #[test]
    fn malformed_bodies_are_decode_errors_before_any_audio() {
        let bad: Vec<(&str, Vec<u8>)> = vec![
            ("garbage", b"this is not a wav at all".to_vec()),
            ("json", br#"{"detail":"x"}"#.to_vec()),
            ("empty", Vec::new()),
            (
                "cut in the header",
                sine_wav(24_000, &[0.5], 220.0, 0.1)[..30].to_vec(),
            ),
            ("ADPCM", wav(&pcm_fmt(2, 1, 24_000, 4), &[], 0, &[0; 8])),
            (
                "12-bit",
                wav(&pcm_fmt(WAVE_FORMAT_PCM, 1, 24_000, 12), &[], 0, &[0; 8]),
            ),
            (
                "no channels",
                wav(&pcm_fmt(WAVE_FORMAT_PCM, 0, 24_000, 16), &[], 0, &[0; 8]),
            ),
            ("short fmt", wav(&[1, 0, 1, 0], &[], 0, &[0; 8])),
            ("data before fmt", {
                let mut b = b"RIFF\0\0\0\0WAVEdata\x02\0\0\0\0\0".to_vec();
                b.extend_from_slice(b"fmt ");
                b
            }),
        ];
        for (name, body) in bad {
            for piece in [1, 4096] {
                let (rate, chunks) = decode_in_reader(&body, piece);
                let e = rate.expect_err(name);
                assert!(matches!(e, TtsError::Decode { .. }), "{name}: {e}");
                assert!(chunks.is_empty(), "{name}: nothing streams");
            }
        }
    }

    /// `decode_reader` over a reader that returns at most `piece` bytes per read.
    fn decode_in_reader(body: &[u8], piece: usize) -> (Result<u32, TtsError>, Vec<PcmChunk>) {
        struct Trickle<'a>(&'a [u8], usize);
        impl std::io::Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(self.1).min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let mut rate = None;
        let mut chunks = Vec::new();
        decode_reader(
            Trickle(body, piece),
            |r| rate = Some(r),
            |c| {
                if let Ok(c) = c {
                    chunks.push(c);
                }
                true
            },
        );
        (rate.expect("on_rate always fires"), chunks)
    }

    #[test]
    fn a_receiver_that_is_gone_stops_the_read() {
        let body = sine_wav(24_000, &[0.5], 220.0, 2.0);
        let mut calls = 0;
        decode_reader(
            std::io::Cursor::new(body),
            |_| {},
            |_| {
                calls += 1;
                false
            },
        );
        assert_eq!(calls, 1, "no chunk after the receiver said it was gone");
    }

    #[test]
    fn a_header_with_no_audio_still_reports_its_rate() {
        let body = wav(&pcm_fmt(WAVE_FORMAT_PCM, 1, 24_000, 16), &[], 0, &[]);
        let (rate, chunks) = decode(body);
        assert_eq!(rate.unwrap(), 24_000);
        assert!(chunks.is_empty());
    }
}
