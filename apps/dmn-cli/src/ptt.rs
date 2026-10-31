//! Push-to-talk (#124): the button, read straight from its input device, so it works whichever
//! window has focus (the call's, during a call). COSMIC's portal has no GlobalShortcuts, and a
//! terminal only sees keys while it has focus; the kernel's evdev device sees the button always.
//!
//! The device file (`/dev/input/by-id/...-event-*`) is read as a stream of `struct input_event`
//! records: a `timeval` (two `long`s), then `u16 type`, `u16 code`, `i32 value`. A key event is
//! type 1 (`EV_KEY`) with value 1 for a press, 0 for a release, 2 for auto-repeat. Plain
//! `std::fs::File` reads, no ioctl, no `unsafe`. The device is never grabbed, so the button
//! keeps doing whatever else it does: a key nothing else uses (F13–F24 mapped on a programmable
//! keyboard, a spare mouse button) is the one to pick.
//!
//! The device is read only while push-to-talk is the voice mode: [`Button::check`] opens it at
//! start and closes it again, reading nothing, so `l` knows whether push-to-talk can be offered;
//! [`ButtonSource::start`] opens it for reading when push-to-talk comes on, and dropping the
//! [`Reading`] it returns closes it when push-to-talk goes off. The file is opened non-blocking
//! (`libc::O_NONBLOCK`), so the reader notices the stop within [`POLL`] even when no key moves.
//!
//! Reading needs access to the device: by default only root and the `input` group have it. The
//! README gives a udev rule that grants it to the logged-in user for that one device
//! ([`udev_rule`]), rather than the `input` group, which would let every program read every key.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dmn_core::config::{ptt_key_code, ptt_key_name};

/// `EV_KEY` in `linux/input-event-codes.h`.
const EV_KEY: u16 = 1;
/// One `struct input_event`: a `timeval` of two `long`s, then type, code, value.
const EVENT_SIZE: usize = 2 * std::mem::size_of::<isize>() + 8;
/// Where udev puts stable names for input devices.
const BY_ID: &str = "/dev/input/by-id";
const BY_PATH: &str = "/dev/input/by-path";
/// How often a reader with nothing to read looks again (and at its stop flag): the most a
/// press or release can wait before it is seen.
const POLL: Duration = Duration::from_millis(5);

#[derive(Debug, thiserror::Error)]
pub enum PttError {
    #[error("[speak].ptt_key {0:?} is not a key dmn knows (KEY_F13, BTN_SIDE, or a number)")]
    BadKey(String),
    #[error(
        "cannot read the push-to-talk device {path}: {detail}; give your login access to it (README § Push-to-talk: `dmn speak ptt devices` prints the udev rule)"
    )]
    Open { path: String, detail: String },
    #[error("cannot start the push-to-talk thread: {0}")]
    Spawn(String),
    #[error(
        "no input device is readable: {0}. Grant access to the button's device first (`dmn speak ptt devices` prints the udev rule), then run this again"
    )]
    NothingReadable(String),
    #[error("no button was pressed within {0} s")]
    Timeout(u64),
}

/// What the button did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Pressed,
    Released,
}

/// One key event from a device: its code and value (1 press, 0 release, 2 repeat).
fn key_event(record: &[u8]) -> Option<(u16, i32)> {
    let off = 2 * std::mem::size_of::<isize>();
    let kind = u16::from_ne_bytes([record[off], record[off + 1]]);
    if kind != EV_KEY {
        return None;
    }
    let code = u16::from_ne_bytes([record[off + 2], record[off + 3]]);
    let value = i32::from_ne_bytes([
        record[off + 4],
        record[off + 5],
        record[off + 6],
        record[off + 7],
    ]);
    Some((code, value))
}

/// Read records from `r` and call `on` for each press or release of `code`, until `stop` is
/// set (`None`) or the device goes away (unplugged, read error: `Some` with the reason).
/// Auto-repeat and other keys are skipped. `r` may be non-blocking: with nothing to read it
/// waits [`POLL`] and looks at `stop` again.
fn read_button(
    mut r: impl Read,
    code: u16,
    stop: &AtomicBool,
    mut on: impl FnMut(Event),
) -> Option<String> {
    let mut buf = [0u8; EVENT_SIZE * 64];
    let mut have = 0;
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match r.read(&mut buf[have..]) {
            Ok(0) => return Some("end of file".into()),
            Ok(n) => have += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(POLL);
                continue;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Some(e.to_string()),
        }
        let whole = have - have % EVENT_SIZE;
        for record in buf[..whole].chunks_exact(EVENT_SIZE) {
            match key_event(record) {
                Some((c, 1)) if c == code => on(Event::Pressed),
                Some((c, 0)) if c == code => on(Event::Released),
                _ => {}
            }
        }
        buf.copy_within(whole..have, 0);
        have -= whole;
    }
}

/// Where button events come from: the device ([`Button`]), or a test's own presses.
pub trait ButtonSource: Send {
    /// Start reading: `on` gets each press and release, `lost` once with the reason if the
    /// device goes away. Reading stops when the [`Reading`] is dropped (and `lost` is not
    /// called then).
    fn start(
        &self,
        on: Box<dyn FnMut(Event) + Send>,
        lost: Box<dyn FnOnce(String) + Send>,
    ) -> Result<Reading, PttError>;
}

/// A button being read. Dropping it stops the reader and closes the device.
#[derive(Debug)]
pub struct Reading {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Reading {
    /// A reading with no thread behind it, for test sources: `stop` is set when it is dropped.
    #[cfg(test)]
    pub fn flag(stop: Arc<AtomicBool>) -> Self {
        Self { stop, thread: None }
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The push-to-talk button: a device that could be opened and a key dmn knows. Nothing is
/// read until [`ButtonSource::start`].
#[derive(Debug, Clone)]
pub struct Button {
    path: String,
    code: u16,
}

impl Button {
    /// Check `device` for `key` (`[speak].ptt_device`, `ptt_key`): the key is known and the
    /// device opens. The file is closed again at once, unread. Fails when either is wrong,
    /// which `dmn run` says on the status line before going on without push-to-talk.
    pub fn check(device: &str, key: &str) -> Result<Self, PttError> {
        let code = ptt_key_code(key).ok_or_else(|| PttError::BadKey(key.to_owned()))?;
        open_device(device)?;
        Ok(Self {
            path: device.to_owned(),
            code,
        })
    }
}

/// Open `device` for reading, non-blocking (module doc).
fn open_device(device: &str) -> Result<File, PttError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(device)
        .map_err(|e| PttError::Open {
            path: device.to_owned(),
            detail: e.to_string(),
        })
}

impl ButtonSource for Button {
    fn start(
        &self,
        mut on: Box<dyn FnMut(Event) + Send>,
        lost: Box<dyn FnOnce(String) + Send>,
    ) -> Result<Reading, PttError> {
        let file = open_device(&self.path)?;
        let (path, code) = (self.path.clone(), self.code);
        let stop = Arc::new(AtomicBool::new(false));
        let t_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("dmn-ptt".to_owned())
            .spawn(move || {
                tracing::info!(device = %path, key = %ptt_key_name(code), "push-to-talk: reading the button");
                match read_button(file, code, &t_stop, &mut on) {
                    None => tracing::info!(device = %path, "push-to-talk: stopped reading the button"),
                    Some(why) => {
                        tracing::warn!(device = %path, reason = %why, "push-to-talk: the button's device went away");
                        lost(why);
                    }
                }
            })
            .map_err(|e| PttError::Spawn(e.to_string()))?;
        Ok(Reading {
            stop,
            thread: Some(thread),
        })
    }
}

/// One input device, for `dmn speak ptt devices` and `learn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The stable path (`/dev/input/by-id/...-event-kbd`), what `[speak].ptt_device` holds.
    pub path: String,
    /// The kernel's name for it (`Logitech Gaming Mouse G402`), when sysfs has one.
    pub name: Option<String>,
    /// The `eventN` node it points at.
    pub node: String,
    pub readable: bool,
}

/// The event devices under `/dev/input/by-id` and `/dev/input/by-path`, one entry per node
/// (by-id first: its names say what the device is).
pub fn devices() -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for dir in [BY_ID, BY_PATH] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let shown = path.display().to_string();
            if !shown.contains("event") {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue;
            };
            let node = target.display().to_string();
            if out.iter().any(|d| d.node == node) {
                continue;
            }
            let event = target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = std::fs::read_to_string(format!("/sys/class/input/{event}/device/name"))
                .ok()
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty());
            out.push(Device {
                readable: File::open(&target).is_ok(),
                path: shown,
                name,
                node,
            });
        }
    }
    out
}

/// The udev rule that lets the logged-in user read `device` (an `/dev/input/eventN` node),
/// from the USB vendor and product ids sysfs gives for it; `None` when it has none.
pub fn udev_rule(node: &str) -> Option<String> {
    let event = Path::new(node).file_name()?.to_string_lossy().into_owned();
    let id = |what: &str| {
        std::fs::read_to_string(format!("/sys/class/input/{event}/device/id/{what}"))
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty() && v != "0000")
    };
    Some(format!(
        "SUBSYSTEM==\"input\", KERNEL==\"event*\", ATTRS{{idVendor}}==\"{}\", ATTRS{{idProduct}}==\"{}\", TAG+=\"uaccess\"",
        id("vendor")?,
        id("product")?
    ))
}

/// `dmn speak ptt learn`: wait up to `timeout` for the first key or button press on any
/// readable device, and return that device's stable path and the key's name.
pub fn learn(timeout: Duration) -> Result<(String, String), PttError> {
    let readable: Vec<Device> = devices().into_iter().filter(|d| d.readable).collect();
    if readable.is_empty() {
        return Err(PttError::NothingReadable(format!(
            "none of the devices under {BY_ID} or {BY_PATH} can be opened"
        )));
    }
    let (tx, rx) = crossbeam_channel::bounded::<(String, u16)>(1);
    for dev in readable {
        let tx = tx.clone();
        let Ok(file) = File::open(&dev.node) else {
            continue;
        };
        let spawned = std::thread::Builder::new()
            .name("dmn-ptt-learn".to_owned())
            .spawn(move || {
                let mut r = file;
                let mut record = [0u8; EVENT_SIZE];
                while r.read_exact(&mut record).is_ok() {
                    if let Some((code, 1)) = key_event(&record) {
                        let _ = tx.try_send((dev.path.clone(), code));
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            return Err(PttError::Spawn(e.to_string()));
        }
    }
    drop(tx);
    let started = Instant::now();
    match rx.recv_timeout(timeout) {
        Ok((path, code)) => {
            tracing::info!(device = %path, code, ms = started.elapsed().as_millis() as u64, "push-to-talk: learned");
            Ok((path, ptt_key_name(code)))
        }
        Err(_) => Err(PttError::Timeout(timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let mut r = vec![0u8; 2 * std::mem::size_of::<isize>()];
        r.extend_from_slice(&kind.to_ne_bytes());
        r.extend_from_slice(&code.to_ne_bytes());
        r.extend_from_slice(&value.to_ne_bytes());
        assert_eq!(r.len(), EVENT_SIZE);
        r
    }

    /// Presses and releases of the configured key come out in order; other keys, auto-repeat
    /// and non-key events (a mouse moving: `EV_REL`, a sync report: `EV_SYN`) do not; the end
    /// of the stream (the device unplugged) ends the read with its reason.
    #[test]
    fn only_the_buttons_press_and_release_come_through() {
        let f13 = ptt_key_code("KEY_F13").unwrap();
        let stream: Vec<u8> = [
            record(2, 0, 5),       // EV_REL: the mouse moved
            record(EV_KEY, 30, 1), // KEY_A pressed
            record(EV_KEY, f13, 1),
            record(0, 0, 0),        // EV_SYN
            record(EV_KEY, f13, 2), // auto-repeat
            record(EV_KEY, f13, 2),
            record(EV_KEY, 30, 0),
            record(EV_KEY, f13, 0),
            record(EV_KEY, f13, 1),
        ]
        .concat();
        let mut got = Vec::new();
        let stop = AtomicBool::new(false);
        let why = read_button(std::io::Cursor::new(stream), f13, &stop, |e| got.push(e));
        assert_eq!(got, [Event::Pressed, Event::Released, Event::Pressed]);
        assert_eq!(why.as_deref(), Some("end of file"));
    }

    /// A record split across two reads still comes through whole.
    #[test]
    fn a_record_split_across_reads_is_put_back_together() {
        struct Dribble(Vec<u8>, usize);
        impl Read for Dribble {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = 5.min(buf.len()).min(self.0.len() - self.1);
                buf[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let f13 = ptt_key_code("KEY_F13").unwrap();
        let stream = [record(EV_KEY, f13, 1), record(EV_KEY, f13, 0)].concat();
        let mut got = Vec::new();
        let stop = AtomicBool::new(false);
        read_button(Dribble(stream, 0), f13, &stop, |e| got.push(e));
        assert_eq!(got, [Event::Pressed, Event::Released]);
    }

    /// With nothing to read (a quiet non-blocking device), the reader still stops when asked.
    #[test]
    fn a_quiet_reader_stops_when_asked() {
        struct Quiet;
        impl Read for Quiet {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(ErrorKind::WouldBlock.into())
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let t_stop = Arc::clone(&stop);
        let t = std::thread::spawn(move || read_button(Quiet, 1, &t_stop, |_| {}));
        std::thread::sleep(Duration::from_millis(20));
        let asked = Instant::now();
        stop.store(true, Ordering::Release);
        assert_eq!(t.join().unwrap(), None);
        assert!(asked.elapsed() < Duration::from_millis(200));
    }

    /// Checking names a bad key and an unreadable device, and reads nothing.
    #[test]
    fn checking_names_a_bad_key_and_an_unreadable_device() {
        let err = Button::check("/dev/null", "KEY_NOPE").unwrap_err();
        assert!(matches!(err, PttError::BadKey(_)), "{err}");
        let err = Button::check("/nonexistent/dmn-ptt-test", "KEY_F13").unwrap_err();
        assert!(matches!(err, PttError::Open { .. }), "{err}");
        assert!(err.to_string().contains("dmn speak ptt devices"), "{err}");
        assert!(Button::check("/dev/null", "BTN_SIDE").is_ok());
    }

    /// A started button's reader stops, and its thread ends, when the reading is dropped; a
    /// device that ends (here `/dev/null`) is reported as lost.
    #[test]
    fn dropping_the_reading_stops_the_reader_and_an_ended_device_is_lost() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let reading = Button::check("/dev/null", "KEY_F13")
            .unwrap()
            .start(
                Box::new(|_| {}),
                Box::new(move |why| {
                    let _ = tx.send(why);
                }),
            )
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).as_deref(),
            Ok("end of file")
        );
        drop(reading);

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("events");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        // A writer keeps the fifo open, so the reader sees "nothing yet", not the end.
        let _writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let reading = Button::check(fifo.to_str().unwrap(), "KEY_F13")
            .unwrap()
            .start(
                Box::new(|_| {}),
                Box::new(move |why| {
                    let _ = tx.send(why);
                }),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let asked = Instant::now();
        drop(reading);
        assert!(
            asked.elapsed() < Duration::from_millis(200),
            "joined at once"
        );
        assert!(rx.try_recv().is_err(), "a stop is not a lost button");
    }

    /// The real button, by hand: `DMN_PTT_DEVICE=/dev/input/by-id/... DMN_PTT_KEY=KEY_F13
    /// cargo test -p dmn-cli ptt::tests::live -- --ignored --nocapture`, then press and release.
    #[test]
    #[ignore = "needs a readable input device and a hand on the button"]
    fn live_press_and_release() {
        let (Ok(device), Ok(key)) = (
            std::env::var("DMN_PTT_DEVICE"),
            std::env::var("DMN_PTT_KEY"),
        ) else {
            println!("set DMN_PTT_DEVICE and DMN_PTT_KEY");
            return;
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let _reading = Button::check(&device, &key)
            .unwrap()
            .start(
                Box::new(move |e| {
                    let _ = tx.send(e);
                }),
                Box::new(|_| {}),
            )
            .unwrap();
        println!("press and release {key} within 10 s");
        let pressed = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let released = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((pressed, released), (Event::Pressed, Event::Released));
    }
}
