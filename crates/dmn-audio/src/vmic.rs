//! The virtual microphone (#108): a PipeWire node named [`VMIC_NODE_NAME`] with
//! `media.class = Audio/Source`, so a call (Meet, Zoom, Teams, `pw-record --target dmn-voice`)
//! lists it as a microphone and hears whatever is written into it. `dmn speak` writes the
//! synthesized speech there; the user's own speakers stay silent.
//!
//! Three pieces, each on the thread that owns it:
//!
//! - [`VirtualMic`], the caller's handle. It lives on the thread that feeds the speech (the
//!   `dmn speak` main thread): `write` resamples a chunk from the TTS rate to the node's rate
//!   with [`LinearResampler`] and pushes it into the ring, waiting while the ring is full;
//!   `drain` waits until PipeWire has pulled the last sample; `close` (and `Drop`) takes the
//!   node out of the graph.
//! - `LoopVmic`, the `pw_stream` and its listeners, on the PipeWire loop thread, reached
//!   through [`crate::pipewire::Command`]s like the capture streams.
//! - [`VoiceReader`], the real-time side. The process callback dequeues an output buffer,
//!   pops samples from the lock-free ring into it, zero-fills what the ring could not supply
//!   (an underrun is silence, never a stall) and requeues. It allocates nothing, locks nothing
//!   and logs nothing; counters are relaxed atomics the feeding thread reads.
//!
//! The ring never drops queued speech. A lock-free SPSC producer cannot touch the consumer's
//! read index (SPEC § Threading rules says the same of capture), and dropping words from the
//! user's own sentence would be the wrong trade anyway: the writer waits, the decoder thread
//! behind it blocks on its bounded channel, and that blocks the HTTP body read. Backpressure
//! all the way to the server; `stalls` counts how often the writer had to wait.
//!
//! Node lifetime: the node exists from [`VirtualMic::open`] to `close`/`Drop`. `dmn speak`
//! opens it before sending the request, so the call (or `pw-record`) can pick it up during the
//! seconds the server takes to render, and closes it when the process ends; nothing lingers in
//! the audio settings between runs. A session that speaks many times (#109) opens one and
//! keeps it, which is what keeps a call's microphone selection stable.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::capture::enum_format_pod;
use crate::pipewire::{Command, PipeWireContext};
use crate::source::AudioError;

/// `node.name` of the virtual microphone: what a call and `pw-record --target` select.
pub const VMIC_NODE_NAME: &str = "dmn-voice";
/// `node.description`: what the desktop's sound panel shows.
pub const VMIC_DESCRIPTION: &str = "Daemon voice (dmn speak)";
/// Bytes per F32LE mono frame.
const STRIDE: usize = 4;
/// Ring capacity handed out by default: 2 s at 48 kHz. The writer waits when it is full, so
/// this is how far ahead of real time the TTS output may run, not a latency.
pub const DEFAULT_RING_SECS: u32 = 2;
/// Largest quantum PipeWire's default `clock.quantum-limit` allows; the callback's fixed
/// scratch is this big and copies larger buffers in passes.
const SCRATCH_FRAMES: usize = 8192;
/// How long `open` waits for the stream to be connected (or refused).
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `close` waits for the loop thread to destroy the stream.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long `write` waits for the ring to make room before deciding nothing is pulling.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Writer and drain poll interval while waiting on the real-time side.
const WAIT_TICK: Duration = Duration::from_millis(5);

type Prod = <HeapRb<f32> as Split>::Prod;
type Cons = <HeapRb<f32> as Split>::Cons;

/// Counters shared by the ring's two ends and read by whoever reports. The real-time side
/// only does relaxed atomic loads, stores and adds on these.
#[derive(Debug)]
pub struct RingStats {
    /// Callbacks that found the ring empty while `speaking` was set: a whole buffer of silence,
    /// a gap in the speech. A short fill is not counted (the clip's last buffer looks the same),
    /// so a producer only slightly slower than real time shows as fewer gaps than it made.
    underruns: AtomicU64,
    /// Writer pushes that found the ring too full to take everything. The writer retries every
    /// 5 ms, so one wait of 100 ms counts about 20; it is a count of polls, not of waits.
    stalls: AtomicU64,
    /// Samples handed to PipeWire so far (real samples, not padding).
    pulled: AtomicU64,
    /// Set by the writer from its first push until the clip has drained; gates `underruns`
    /// so the silence before and after a clip is not counted as gaps in it.
    speaking: AtomicBool,
    /// Nanoseconds after `epoch` when the first real sample was pulled; 0 until then.
    first_pull_ns: AtomicU64,
    epoch: Instant,
}

impl RingStats {
    fn new() -> Self {
        Self {
            underruns: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            pulled: AtomicU64::new(0),
            speaking: AtomicBool::new(false),
            first_pull_ns: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn stalls(&self) -> u64 {
        self.stalls.load(Ordering::Relaxed)
    }

    pub fn pulled(&self) -> u64 {
        self.pulled.load(Ordering::Relaxed)
    }

    /// When PipeWire pulled the first real sample, if it has.
    pub fn first_pull(&self) -> Option<Instant> {
        let ns = self.first_pull_ns.load(Ordering::Relaxed);
        (ns != 0).then(|| self.epoch + Duration::from_nanos(ns))
    }
}

/// Producer end: held by the feeding thread. Never blocks; `push` returns how much fit.
pub struct VoiceWriter {
    prod: Prod,
    stats: Arc<RingStats>,
}

impl VoiceWriter {
    /// Push as many of `samples` as fit and return that count. Counts a stall when it is less
    /// than all of them; the caller pushes the rest later. Nothing already queued is touched.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let n = self.prod.push_slice(samples);
        if n < samples.len() {
            self.stats.stalls.fetch_add(1, Ordering::Relaxed);
        }
        n
    }

    /// Samples the ring can still take.
    pub fn vacant(&self) -> usize {
        self.prod.vacant_len()
    }

    /// Open or close the underrun gate (see [`RingStats`]).
    pub fn set_speaking(&self, speaking: bool) {
        self.stats.speaking.store(speaking, Ordering::Relaxed);
    }

    pub fn stats(&self) -> Arc<RingStats> {
        Arc::clone(&self.stats)
    }
}

/// Consumer end: held by the process callback. `fill` is safe to call from a real-time
/// callback: no allocation, no lock, no logging, bounded time.
pub struct VoiceReader {
    cons: Cons,
    stats: Arc<RingStats>,
}

impl VoiceReader {
    /// Fill `out` from the ring and zero the rest. Returns how many real samples it had.
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let got = self.cons.pop_slice(out);
        if got < out.len() {
            out[got..].fill(0.0);
            // A whole buffer of silence while speaking is a gap. A short fill is not: it is
            // the clip's last buffer, or a gap that the next callback counts as a whole one.
            if got == 0 && self.stats.speaking.load(Ordering::Relaxed) {
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        if got > 0 {
            if self.stats.first_pull_ns.load(Ordering::Relaxed) == 0 {
                // `Instant::elapsed` is a monotonic clock read (vDSO, no syscall); never 0 in
                // practice, and 0 would only mean "measure again next time".
                let ns = self.stats.epoch.elapsed().as_nanos() as u64;
                self.stats.first_pull_ns.store(ns, Ordering::Relaxed);
            }
            self.stats.pulled.fetch_add(got as u64, Ordering::Relaxed);
        }
        got
    }

    /// Samples queued.
    pub fn queued(&self) -> usize {
        self.cons.occupied_len()
    }
}

/// Build a ring of `capacity` samples and split it into its two ends.
pub fn voice_ring(capacity: usize) -> (VoiceWriter, VoiceReader) {
    let (prod, cons) = HeapRb::<f32>::new(capacity.max(1)).split();
    let stats = Arc::new(RingStats::new());
    (
        VoiceWriter {
            prod,
            stats: Arc::clone(&stats),
        },
        VoiceReader { cons, stats },
    )
}

/// Linear-interpolation resampler for speech, `from` Hz to `to` Hz, continuous across chunks
/// (the last input sample is kept so the first output of the next chunk interpolates towards
/// it). Chatterbox emits 24 kHz; the graph runs at 48 kHz on the dev box, so this is a plain
/// 2× upsample there. Good enough for speech; a sine keeps its frequency (tested below).
#[derive(Debug, Clone)]
pub struct LinearResampler {
    from: u32,
    to: u32,
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output in the virtual input `[prev, chunk...]`: 0 is `prev`,
    /// 1 is the chunk's first sample. Starts at 1 so the first output is the first input.
    pos: f64,
    prev: f32,
}

impl LinearResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 1.0,
            prev: 0.0,
        }
    }

    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn to_rate(&self) -> u32 {
        self.to
    }

    /// Resample `input` and append the output to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let len = input.len() as f64;
        // The virtual input is `[prev, input...]`, indices 0..=len. Every output whose
        // position is at or before the chunk's last sample is computable now; the rest waits
        // for the next chunk (or stays unproduced at the very end: under one sample).
        let at = |i: usize| if i == 0 { self.prev } else { input[i - 1] };
        while self.pos <= len {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = at(i);
            // frac > 0 implies pos < len, so i + 1 <= len is a real sample.
            let value = if frac > 0.0 {
                a + (at(i + 1) - a) * frac
            } else {
                a
            };
            out.push(value);
            self.pos += self.step;
        }
        self.prev = input[input.len() - 1];
        self.pos -= len;
    }
}

/// Lifecycle of the virtual microphone's stream, as the status reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmicState {
    /// Not created, or closed.
    Idle,
    /// Connecting to the graph.
    Connecting,
    /// In the graph, with nothing linked to it yet: PipeWire does not run it until a call or
    /// `pw-record` selects it, so nothing is pulled from the ring meanwhile.
    Ready,
    /// Something is linked and samples are being pulled.
    Streaming,
    /// PipeWire reported an error; see [`VmicStatus::last_error`].
    Error,
}

impl VmicState {
    fn to_u8(self) -> u8 {
        match self {
            VmicState::Idle => 0,
            VmicState::Connecting => 1,
            VmicState::Ready => 2,
            VmicState::Streaming => 3,
            VmicState::Error => 4,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => VmicState::Connecting,
            2 => VmicState::Ready,
            3 => VmicState::Streaming,
            4 => VmicState::Error,
            _ => VmicState::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VmicState::Idle => "idle",
            VmicState::Connecting => "connecting",
            VmicState::Ready => "ready (nothing linked)",
            VmicState::Streaming => "streaming",
            VmicState::Error => "in error",
        }
    }
}

/// Shared, lock-free view of the stream for the feeding thread. The real-time callback reads
/// only `format_ok`.
#[derive(Debug)]
pub struct VmicStatus {
    state: AtomicU8,
    /// Set once PipeWire settled on F32LE mono at the requested rate; the callback emits
    /// nothing until then, so no buffer is ever filled with bytes of the wrong shape.
    format_ok: AtomicBool,
    rate: AtomicU32,
    last_error: Mutex<Option<String>>,
}

impl Default for VmicStatus {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(VmicState::Idle.to_u8()),
            format_ok: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            last_error: Mutex::new(None),
        }
    }
}

impl VmicStatus {
    pub fn state(&self) -> VmicState {
        VmicState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// The negotiated rate, once PipeWire has settled it.
    pub fn rate(&self) -> Option<u32> {
        let rate = self.rate.load(Ordering::Acquire);
        (rate != 0).then_some(rate)
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_state(&self, s: VmicState) {
        self.state.store(s.to_u8(), Ordering::Release);
    }

    pub(crate) fn set_error(&self, why: String) {
        *self.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
        self.set_state(VmicState::Error);
    }
}

/// What the loop thread needs to create the stream.
pub(crate) struct VmicSpec {
    pub(crate) node_name: String,
    pub(crate) description: String,
    pub(crate) rate: u32,
    pub(crate) status: Arc<VmicStatus>,
}

/// The real-time side's state: the ring's consumer and a fixed scratch block.
struct Outlet {
    reader: VoiceReader,
    scratch: Box<[f32]>,
    status: Arc<VmicStatus>,
}

/// REAL-TIME CALLBACK (runs on PipeWire's data thread; `RT_PROCESS`).
/// Rules (CLAUDE.md, SPEC § Threading rules), reviewed call by call:
/// - no allocation: `dequeue_buffer` wraps a pointer, `datas_mut`/`data`/`chunk_mut` borrow
///   the mapped buffer, `VoiceReader::fill` pops into the preallocated `scratch`, and the
///   samples are copied from there into the buffer as little-endian bytes;
/// - no locks: the ring is lock-free SPSC, counters and the format gate are relaxed atomics;
/// - no logging, no syscalls (one monotonic clock read for the first-sample timestamp,
///   which is a vDSO call), bounded time (one pass over the buffer).
///
/// Dequeue, fill, requeue (the `Buffer` guard requeues on drop). An empty ring fills the
/// buffer with silence: the graph never waits on the server.
fn process(stream: &pw::stream::Stream, out: &mut Outlet) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    // `requested` is how many frames the graph wants this cycle (0 on servers that do not
    // say, in which case the whole buffer is filled).
    let requested = buffer.requested() as usize;
    let format_ok = out.status.format_ok.load(Ordering::Acquire);
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let mut frames = 0;
    if let Some(bytes) = data.data()
        && format_ok
    {
        let max_frames = bytes.len() / STRIDE;
        frames = if requested == 0 {
            max_frames
        } else {
            requested.min(max_frames)
        };
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(out.scratch.len());
            out.reader.fill(&mut out.scratch[..n]);
            let dst = &mut bytes[done * STRIDE..(done + n) * STRIDE];
            for (slot, sample) in dst.chunks_exact_mut(STRIDE).zip(&out.scratch[..n]) {
                slot.copy_from_slice(&sample.to_le_bytes());
            }
            done += n;
        }
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = STRIDE as i32;
    *chunk.size_mut() = (frames * STRIDE) as u32;
}

/// The stream and its two listeners, owned by the PipeWire loop thread. The process listener
/// is the only one holding the [`Outlet`]; state and format events go to a second listener
/// with no user data, so the real-time thread and the loop thread never share a `&mut`.
pub(crate) struct LoopVmic {
    // Field order is drop order: listeners before the stream they are attached to.
    _process: pw::stream::StreamListener<Outlet>,
    _events: pw::stream::StreamListener<()>,
    stream: pw::stream::StreamRc,
    status: Arc<VmicStatus>,
}

impl LoopVmic {
    /// Create and connect the stream on the loop thread.
    pub(crate) fn open(
        core: &pw::core::CoreRc,
        spec: VmicSpec,
        reader: VoiceReader,
    ) -> Result<Self, String> {
        let mut props = pw::properties::PropertiesBox::new();
        props.insert(*pw::keys::MEDIA_TYPE, "Audio");
        props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
        props.insert(*pw::keys::MEDIA_ROLE, "Communication");
        // The one property that makes a playback stream a microphone: apps enumerate it with
        // the real capture devices and WirePlumber links them to it on request.
        props.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
        props.insert(*pw::keys::NODE_NAME, spec.node_name.as_str());
        props.insert(*pw::keys::NODE_DESCRIPTION, spec.description.as_str());
        props.insert(*pw::keys::NODE_NICK, spec.node_name.as_str());
        // What keeps the node off the speakers is the missing AUTOCONNECT flag on `connect`
        // below: the session manager never links it to a sink, only whatever selects it as a
        // microphone links to it. This flag only stops a later move to another target.
        props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
        let stream = pw::stream::StreamRc::new(core.clone(), &spec.node_name, props)
            .map_err(|e| e.to_string())?;

        let outlet = Outlet {
            reader,
            scratch: vec![0.0; SCRATCH_FRAMES].into_boxed_slice(),
            status: Arc::clone(&spec.status),
        };
        let process_listener = stream
            .add_local_listener_with_user_data(outlet)
            .process(process)
            .register()
            .map_err(|e| e.to_string())?;

        let state_status = Arc::clone(&spec.status);
        let format_status = Arc::clone(&spec.status);
        let want_rate = spec.rate;
        let events_listener = stream
            .add_local_listener::<()>()
            .state_changed(move |_, _, _old, new| match new {
                pw::stream::StreamState::Streaming => state_status.set_state(VmicState::Streaming),
                pw::stream::StreamState::Paused => state_status.set_state(VmicState::Ready),
                pw::stream::StreamState::Connecting => {
                    state_status.set_state(VmicState::Connecting)
                }
                pw::stream::StreamState::Error(why) => state_status.set_error(why),
                pw::stream::StreamState::Unconnected => {}
            })
            .param_changed(move |_, _, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    format_status.format_ok.store(false, Ordering::Release);
                    return;
                };
                let mut info = spa::param::audio::AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    format_status.rate.store(info.rate(), Ordering::Release);
                    let ok = info.rate() == want_rate
                        && info.channels() == 1
                        && info.format() == spa::param::audio::AudioFormat::F32LE;
                    format_status.format_ok.store(ok, Ordering::Release);
                    if !ok {
                        format_status.set_error(format!(
                            "negotiated {:?} {} Hz × {} ch, need F32LE {want_rate} Hz mono",
                            info.format(),
                            info.rate(),
                            info.channels()
                        ));
                    }
                } else {
                    format_status.format_ok.store(false, Ordering::Release);
                }
            })
            .register()
            .map_err(|e| e.to_string())?;

        let pod = enum_format_pod(spec.rate)?;
        let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or("bad EnumFormat pod")?];
        // Output direction, no AUTOCONNECT: a source is linked *from*, by whoever selects it.
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .map_err(|e| e.to_string())?;
        spec.status.set_state(VmicState::Connecting);

        Ok(Self {
            _process: process_listener,
            _events: events_listener,
            stream,
            status: spec.status,
        })
    }
}

impl Drop for LoopVmic {
    fn drop(&mut self) {
        self.status.format_ok.store(false, Ordering::Release);
        // After this returns the data thread no longer calls `process`; destroying the stream
        // (the fields drop next) removes the node from the graph.
        let _ = self.stream.disconnect();
        self.status.set_state(VmicState::Idle);
    }
}

/// What a clip's playback came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmicStats {
    /// Samples written at the node's rate.
    pub written: u64,
    /// Samples PipeWire pulled.
    pub pulled: u64,
    pub underruns: u64,
    pub stalls: u64,
    /// When PipeWire pulled the first real sample.
    pub first_pull: Option<Instant>,
}

/// The virtual microphone, from the feeding thread's side. See the module doc for the node's
/// lifetime and the ring's rules.
pub struct VirtualMic {
    ctx: Arc<PipeWireContext>,
    key: Option<u64>,
    status: Arc<VmicStatus>,
    writer: VoiceWriter,
    rate: u32,
    resampler: Option<LinearResampler>,
    scratch: Vec<f32>,
    written: u64,
}

impl VirtualMic {
    /// Create the node at `rate` Hz (the graph rate, [`crate::NodeTable::graph_rate`]) with a
    /// ring of [`DEFAULT_RING_SECS`], and wait until it is in the graph. An error names what
    /// PipeWire refused; the node is gone again when this returns `Err`.
    pub fn open(ctx: Arc<PipeWireContext>, rate: u32) -> Result<Self, AudioError> {
        Self::open_with(ctx, rate, (rate * DEFAULT_RING_SECS) as usize)
    }

    /// [`Self::open`] with the ring capacity in samples.
    pub fn open_with(
        ctx: Arc<PipeWireContext>,
        rate: u32,
        ring_capacity: usize,
    ) -> Result<Self, AudioError> {
        let (writer, reader) = voice_ring(ring_capacity);
        let status = Arc::new(VmicStatus::default());
        let key = ctx.next_key();
        ctx.send(Command::OpenVmic {
            key,
            spec: VmicSpec {
                node_name: VMIC_NODE_NAME.to_owned(),
                description: VMIC_DESCRIPTION.to_owned(),
                rate,
                status: Arc::clone(&status),
            },
            reader,
        })?;
        let mut mic = Self {
            ctx,
            key: Some(key),
            status,
            writer,
            rate,
            resampler: None,
            scratch: Vec::new(),
            written: 0,
        };
        let t = Instant::now();
        loop {
            match mic.status.state() {
                VmicState::Ready | VmicState::Streaming => return Ok(mic),
                VmicState::Error => {
                    let why = mic.status.last_error().unwrap_or_default();
                    let _ = mic.close_inner();
                    return Err(AudioError::VirtualMic(why));
                }
                VmicState::Idle | VmicState::Connecting => {
                    if t.elapsed() > OPEN_TIMEOUT {
                        let _ = mic.close_inner();
                        return Err(AudioError::VirtualMic(format!(
                            "the stream was still {} after {} ms",
                            mic.status.state().label(),
                            OPEN_TIMEOUT.as_millis()
                        )));
                    }
                    std::thread::sleep(WAIT_TICK);
                }
            }
        }
    }

    /// The rate the node plays at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn status(&self) -> Arc<VmicStatus> {
        Arc::clone(&self.status)
    }

    pub fn stats(&self) -> VmicStats {
        let s = self.writer.stats();
        VmicStats {
            written: self.written,
            pulled: s.pulled(),
            underruns: s.underruns(),
            stalls: s.stalls(),
            first_pull: s.first_pull(),
        }
    }

    /// Queue `samples` (mono f32 at `sample_rate`) for playback, resampled to the node's rate.
    /// Waits while the ring is full; fails if PipeWire reports an error, or if nothing pulled
    /// from a full ring for [`STALL_TIMEOUT`] (the node is in the graph but nothing is linked
    /// to it, so the speech would never be heard).
    pub fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
        if self
            .resampler
            .as_ref()
            .is_none_or(|r| r.from_rate() != sample_rate)
        {
            self.resampler = Some(LinearResampler::new(sample_rate, self.rate));
        }
        self.scratch.clear();
        if let Some(r) = self.resampler.as_mut() {
            r.process(samples, &mut self.scratch);
        }
        let mut rest: &[f32] = &self.scratch;
        let mut last_progress = Instant::now();
        while !rest.is_empty() {
            let n = self.writer.push(rest);
            rest = &rest[n..];
            if n > 0 {
                self.written += n as u64;
                self.writer.set_speaking(true);
                last_progress = Instant::now();
            }
            if rest.is_empty() {
                break;
            }
            self.check()?;
            if last_progress.elapsed() > STALL_TIMEOUT {
                return Err(AudioError::VirtualMic(format!(
                    "nothing pulled from {VMIC_NODE_NAME} for {} s while it was full ({}); is it selected as the microphone anywhere?",
                    STALL_TIMEOUT.as_secs(),
                    self.status.state().label()
                )));
            }
            std::thread::sleep(WAIT_TICK);
        }
        Ok(())
    }

    /// Wait until PipeWire has pulled everything written, or `timeout`. Returns whether it
    /// did; either way the underrun gate closes, so the silence after the clip is not counted.
    pub fn drain(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        let stats = self.writer.stats();
        let t = Instant::now();
        let drained = loop {
            if stats.pulled() >= self.written {
                break true;
            }
            self.check()?;
            if t.elapsed() > timeout {
                break false;
            }
            std::thread::sleep(WAIT_TICK);
        };
        self.writer.set_speaking(false);
        Ok(drained)
    }

    /// Take the node out of the graph. `Drop` does the same without reporting.
    pub fn close(mut self) -> Result<(), AudioError> {
        self.close_inner()
    }

    fn check(&self) -> Result<(), AudioError> {
        match self.status.state() {
            VmicState::Error => Err(AudioError::VirtualMic(
                self.status.last_error().unwrap_or_default(),
            )),
            VmicState::Idle => Err(AudioError::VirtualMic(
                "the stream was closed (PipeWire loop gone?)".into(),
            )),
            _ => Ok(()),
        }
    }

    fn close_inner(&mut self) -> Result<(), AudioError> {
        let Some(key) = self.key.take() else {
            return Ok(());
        };
        if self.ctx.send(Command::CloseVmic { key }).is_err() {
            // The loop is gone, and the stream and node went with it.
            return Ok(());
        }
        let t = Instant::now();
        while self.status.state() != VmicState::Idle && t.elapsed() < CLOSE_TIMEOUT {
            std::thread::sleep(Duration::from_millis(2));
        }
        if self.status.state() != VmicState::Idle {
            return Err(AudioError::StopTimeout {
                state: self.status.state().label(),
                ms: CLOSE_TIMEOUT.as_millis() as u64,
            });
        }
        Ok(())
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        let _ = self.close_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underrun_emits_silence_and_counts_only_while_speaking() {
        let (writer, mut reader) = voice_ring(100);
        let stats = writer.stats();
        let mut out = [1.0f32; 16];
        // Empty ring, gate closed: silence, no underrun (nothing is playing yet).
        let t = Instant::now();
        assert_eq!(reader.fill(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0.0), "padding is silence");
        assert!(t.elapsed() < Duration::from_millis(5), "returned at once");
        assert_eq!(stats.underruns(), 0);
        assert_eq!(stats.pulled(), 0);
        assert!(stats.first_pull().is_none());

        // Gate open: a short fill pads with silence (the clip's last buffer, not a gap); an
        // empty fill while speaking is a gap and counts once per fill.
        writer.set_speaking(true);
        let mut writer = writer;
        assert_eq!(writer.push(&[0.5; 10]), 10);
        out.fill(1.0);
        assert_eq!(reader.fill(&mut out), 10);
        assert!(out[..10].iter().all(|&s| s == 0.5));
        assert!(out[10..].iter().all(|&s| s == 0.0));
        assert_eq!(stats.underruns(), 0, "a short last buffer is not a gap");
        assert_eq!(stats.pulled(), 10);
        assert!(
            stats.first_pull().is_some(),
            "first real sample timestamped"
        );
        out.fill(1.0);
        assert_eq!(reader.fill(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0.0), "a gap is silence");
        assert_eq!(stats.underruns(), 1);
        assert_eq!(reader.fill(&mut out), 0);
        assert_eq!(stats.underruns(), 2);
        writer.set_speaking(false);
        assert_eq!(reader.fill(&mut out), 0);
        assert_eq!(stats.underruns(), 2, "gate closed: the tail is not a gap");
    }

    #[test]
    fn a_full_ring_takes_what_fits_keeps_what_is_queued_and_counts_the_stall() {
        let (mut writer, mut reader) = voice_ring(8);
        assert_eq!(writer.push(&[1.0; 6]), 6);
        assert_eq!(writer.stats().stalls(), 0);
        let t = Instant::now();
        assert_eq!(writer.push(&[2.0; 6]), 2, "only two slots were free");
        assert!(t.elapsed() < Duration::from_millis(5), "never blocks");
        assert_eq!(writer.stats().stalls(), 1);
        assert_eq!(writer.push(&[3.0; 1]), 0);
        assert_eq!(writer.stats().stalls(), 2);
        assert_eq!(writer.vacant(), 0);
        // Nothing queued was lost: the oldest samples come out first, in order.
        let mut out = [0.0f32; 8];
        assert_eq!(reader.fill(&mut out), 8);
        assert_eq!(&out[..6], &[1.0; 6]);
        assert_eq!(&out[6..], &[2.0; 2]);
        assert_eq!(reader.queued(), 0);
        // Draining frees exactly that many slots.
        assert_eq!(writer.push(&[4.0; 9]), 8);
    }

    /// 24 kHz → 48 kHz: a 440 Hz sine keeps its frequency and the length doubles.
    #[test]
    fn resampler_doubles_the_length_and_keeps_the_frequency() {
        let (from, to, hz) = (24_000u32, 48_000u32, 440.0f32);
        let secs = 0.5;
        let n_in = (from as f32 * secs) as usize;
        let input: Vec<f32> = (0..n_in)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / from as f32).sin())
            .collect();
        let mut r = LinearResampler::new(from, to);
        let mut out = Vec::new();
        // Fed in uneven chunks, as the decoder delivers them.
        for chunk in input.chunks(1_333) {
            r.process(chunk, &mut out);
        }
        let expect = n_in * to as usize / from as usize;
        assert!(
            out.len().abs_diff(expect) <= 2,
            "length {} ≈ {expect}",
            out.len()
        );
        // Frequency from zero crossings (the sine starts at 0 and the output is continuous).
        let crossings = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let measured = crossings as f32 / (out.len() as f32 / to as f32);
        assert!(
            (measured - hz).abs() <= 2.0,
            "{measured} Hz at {to} Hz, expected {hz}"
        );
        // Amplitude survives (linear interpolation of a 440 Hz sine at 24 kHz loses almost
        // nothing).
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.99, "peak {peak}");
        // Chunk boundaries are seamless: no sample-to-sample jump larger than the sine's own
        // slope allows.
        let max_step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        let slope = std::f32::consts::TAU * hz / to as f32;
        assert!(
            max_step <= slope * 1.05,
            "max step {max_step} vs slope {slope}"
        );
        assert_eq!((r.from_rate(), r.to_rate()), (from, to));
    }

    #[test]
    fn resampler_at_the_same_rate_is_the_identity() {
        let mut r = LinearResampler::new(48_000, 48_000);
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&input[..300], &mut out);
        r.process(&input[300..], &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn resampler_downsamples_too() {
        // 48 kHz → 16 kHz: a third of the samples, the ramp intact.
        let mut r = LinearResampler::new(48_000, 16_000);
        let input: Vec<f32> = (0..3000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out.len(), 1000);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 3.0);
        assert_eq!(out[999], 2997.0);
    }

    #[test]
    fn state_labels_and_status_round_trip() {
        for s in [
            VmicState::Idle,
            VmicState::Connecting,
            VmicState::Ready,
            VmicState::Streaming,
            VmicState::Error,
        ] {
            assert_eq!(VmicState::from_u8(s.to_u8()), s);
            assert!(!s.label().is_empty());
        }
        let st = VmicStatus::default();
        assert_eq!(st.state(), VmicState::Idle);
        assert_eq!(st.rate(), None);
        st.set_error("refused".into());
        assert_eq!(st.state(), VmicState::Error);
        assert_eq!(st.last_error().as_deref(), Some("refused"));
    }

    // ---- Live tests: need a running PipeWire server and pw-record. `just probe`.

    mod live {
        use super::*;
        use std::io::Read as _;
        use std::process::{Command as Proc, Stdio};

        const TONE_HZ: f32 = 440.0;

        /// Both tests create the one `dmn-voice`; `pw-record --target dmn-voice` would pick
        /// either, so they run one at a time.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

        fn connect() -> Arc<PipeWireContext> {
            let ctx = Arc::new(PipeWireContext::connect().expect("PipeWire running"));
            assert!(ctx.wait_for_sync(Duration::from_secs(2)).synced);
            ctx
        }

        fn node_count() -> usize {
            let out = Proc::new("pw-dump").output().expect("pw-dump on PATH");
            String::from_utf8_lossy(&out.stdout)
                .matches(&format!("\"node.name\": \"{VMIC_NODE_NAME}\""))
                .count()
        }

        /// Record the virtual microphone as raw 48 kHz mono f32 on a pipe: nothing on disk,
        /// even in tests (CLAUDE.md).
        fn record(rate: u32) -> std::process::Child {
            Proc::new("pw-record")
                .args(["--target", VMIC_NODE_NAME, "--raw"])
                .args([
                    "--rate",
                    &rate.to_string(),
                    "--channels",
                    "1",
                    "--format",
                    "f32",
                ])
                .arg("-")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("pw-record on PATH")
        }

        fn goertzel(w: &[f32], hz: f32, rate: f32) -> f32 {
            let k = 2.0 * (2.0 * std::f32::consts::PI * hz / rate).cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in w {
                let s0 = x + k * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            s1 * s1 + s2 * s2 - k * s1 * s2
        }

        /// Speak a 1.5 s 440 Hz tone generated at 24 kHz (Chatterbox's rate) through
        /// `dmn-voice`, record the node with `pw-record --target dmn-voice`, and check what
        /// came out: loud enough, the right length, the right pitch at 48 kHz, and the node
        /// gone afterwards.
        #[test]
        #[ignore]
        fn live_vmic_plays_a_tone_pw_record_hears_and_the_node_leaves_the_graph() {
            let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            let ctx = connect();
            let graph_rate = ctx.nodes().graph_rate();
            let before = node_count();
            let mut mic = VirtualMic::open(Arc::clone(&ctx), graph_rate).expect("vmic opens");
            assert_eq!(mic.rate(), graph_rate);
            assert_eq!(node_count(), before + 1, "dmn-voice is in the graph");
            assert!(matches!(
                mic.status().state(),
                VmicState::Ready | VmicState::Streaming
            ));

            let mut rec = record(graph_rate);
            let mut stdout = rec.stdout.take().unwrap();
            let reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                buf
            });
            // Let pw-record link before the tone starts, so its start is in the recording.
            std::thread::sleep(Duration::from_millis(500));

            let (tts_rate, secs) = (24_000u32, 1.5f32);
            let n = (tts_rate as f32 * secs) as usize;
            let tone: Vec<f32> = (0..n)
                .map(|i| 0.5 * (std::f32::consts::TAU * TONE_HZ * i as f32 / tts_rate as f32).sin())
                .collect();
            let t = Instant::now();
            for chunk in tone.chunks(4_800) {
                mic.write(chunk, tts_rate).expect("write");
            }
            assert!(
                mic.drain(Duration::from_secs(5)).expect("drain"),
                "all pulled"
            );
            let played = t.elapsed();
            let stats = mic.stats();
            std::thread::sleep(Duration::from_millis(300));
            let _ = rec.kill();
            let _ = rec.wait();
            let raw = reader.join().unwrap();
            mic.close().expect("close");
            assert_eq!(node_count(), before, "dmn-voice left the graph");

            // Real time: 1.5 s of audio took about 1.5 s to play (the ring holds 2 s, so the
            // writes themselves never waited; drain did).
            assert!(
                played >= Duration::from_millis(1_200) && played <= Duration::from_millis(2_500),
                "played in {played:?}"
            );
            let expect = (graph_rate as f32 * secs) as u64;
            assert!(
                stats.written.abs_diff(expect) <= 2,
                "written {} ≈ {expect} (the resampler holds back under one sample)",
                stats.written
            );
            assert_eq!(stats.pulled, stats.written);
            assert!(stats.first_pull.is_some());
            assert_eq!(stats.stalls, 0);

            let samples: Vec<f32> = raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            assert!(!samples.is_empty(), "pw-record heard the node");
            // Duration of the non-silent part, within 10 % of the tone.
            let loud: Vec<usize> = samples
                .iter()
                .enumerate()
                .filter(|(_, s)| s.abs() > 0.05)
                .map(|(i, _)| i)
                .collect();
            let (first, last) = (loud[0], *loud.last().unwrap());
            let heard = (last - first) as f32 / graph_rate as f32;
            assert!(
                (heard - secs).abs() <= secs * 0.1,
                "heard {heard:.3} s of tone, expected {secs} ± 10 %"
            );
            let tone_part = &samples[first..=last];
            let rms =
                (tone_part.iter().map(|s| s * s).sum::<f32>() / tone_part.len() as f32).sqrt();
            assert!(rms > 0.2, "rms {rms} (a 0.5 sine is 0.354)");
            // Pitch: 440 Hz dominates its neighbours (880 Hz would mean the 24 kHz samples
            // were played at 48 kHz, i.e. no resampling).
            let rate = graph_rate as f32;
            let on = goertzel(tone_part, TONE_HZ, rate);
            let off = goertzel(tone_part, 880.0, rate).max(goertzel(tone_part, 220.0, rate));
            assert!(on / off.max(1e-9) > 100.0, "440 Hz dominates: {}", on / off);
        }

        /// Whether a recorder linked to `dmn-voice` is linked again after the node is closed
        /// and a new one opened on a fresh connection, as a dmn restart does: (linked at
        /// first, linked again within 5 s).
        fn relinked_after_a_restart(recorder: &[&str]) -> (bool, bool) {
            let wait_streaming = |mic: &VirtualMic, within: Duration| {
                let t = Instant::now();
                while mic.status().state() != VmicState::Streaming && t.elapsed() < within {
                    std::thread::sleep(Duration::from_millis(20));
                }
                mic.status().state() == VmicState::Streaming
            };
            let ctx = connect();
            let rate = ctx.nodes().graph_rate();
            let mic = VirtualMic::open(Arc::clone(&ctx), rate).expect("vmic opens");
            // Raw samples on a pipe that is read and dropped: nothing on disk.
            let mut rec = Proc::new(recorder[0])
                .args(&recorder[1..])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("recorder on PATH");
            let mut out = rec.stdout.take().unwrap();
            std::thread::spawn(move || {
                let mut buf = [0u8; 65_536];
                while out.read(&mut buf).is_ok_and(|n| n > 0) {}
            });
            let linked = wait_streaming(&mic, Duration::from_secs(3));
            drop(mic);
            drop(ctx);
            std::thread::sleep(Duration::from_millis(1_000));
            let ctx = connect();
            let mic = VirtualMic::open(Arc::clone(&ctx), rate).expect("vmic reopens");
            let relinked = wait_streaming(&mic, Duration::from_secs(5));
            let _ = rec.kill();
            let _ = rec.wait();
            (linked, relinked)
        }

        /// #113: `dmn-voice` across a dmn restart. The node is a new one each run (same
        /// `node.name`, new `object.serial`); WirePlumber relinks a stream to it only when the
        /// stream names it by name in its own properties (`target.object = "dmn-voice"`, as
        /// `pw-record --target` and a PulseAudio app opening the device by name do). A stream
        /// with `node.dont-reconnect` (what a PulseAudio app gets when it asks not to be moved,
        /// as Chromium-based apps such as Discord do for a chosen device) is never linked again
        /// once handled (`linking/prepare-link.lua`), and a stream moved by hand is kept in the
        /// `default` metadata by serial, which a new node never matches. No property of the
        /// node changes either, so `dmn run` says so on the status line instead.
        #[test]
        #[ignore]
        fn live_a_reopened_dmn_voice_is_relinked_only_to_a_stream_that_names_it() {
            let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            let by_name = ["pw-record", "--target", VMIC_NODE_NAME, "--raw", "-"];
            assert_eq!(relinked_after_a_restart(&by_name), (true, true));
            let dont_move = [
                "pw-record",
                "--target",
                VMIC_NODE_NAME,
                "-P",
                "{ node.dont-reconnect = true }",
                "--raw",
                "-",
            ];
            assert_eq!(
                relinked_after_a_restart(&dont_move),
                (true, false),
                "if this now relinks, PipeWire keeps a call's choice across a restart and the \
                 start-up notice in dmn-cli's speaker can go"
            );
        }

        /// Nothing linked: the node sits in the graph, nothing is pulled, and `write` reports
        /// that instead of blocking forever.
        #[test]
        #[ignore]
        fn live_vmic_with_no_listener_reports_the_stall() {
            let _serial = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            let ctx = connect();
            let rate = ctx.nodes().graph_rate();
            // A 0.1 s ring so the stall shows within the timeout.
            let mut mic = VirtualMic::open_with(Arc::clone(&ctx), rate, rate as usize / 10)
                .expect("vmic opens");
            let silence = vec![0.0f32; rate as usize];
            let t = Instant::now();
            let err = mic.write(&silence, rate).unwrap_err();
            assert!(err.to_string().contains("nothing pulled"), "{err}");
            assert!(t.elapsed() >= STALL_TIMEOUT);
            assert_eq!(mic.stats().pulled, 0);
            mic.close().expect("close");
            assert_eq!(node_count(), 0);
        }
    }
}
