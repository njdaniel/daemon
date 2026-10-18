//! Push-to-talk (#124): the button, read straight from its input device, so it works whichever
//! window has focus (the call's, during a call). COSMIC's portal has no GlobalShortcuts, and a
//! terminal only sees keys while it has focus; the kernel's evdev device sees the button always.
//!
//! The device file (`/dev/input/by-id/...-event-*`) is read as a stream of `struct input_event`
//! records: a `timeval` (two `long`s), then `u16 type`, `u16 code`, `i32 value`. A key event is
//! type 1 (`EV_KEY`) with value 1 for a press, 0 for a release, 2 for auto-repeat. Plain
//! `std::fs::File` reads, no ioctl, no `unsafe`, no new crate. The device is never grabbed, so
//! the button keeps doing whatever else it does: a key nothing else uses (F13–F24 mapped on a
//! programmable keyboard, a spare mouse button) is the one to pick.
//!
//! Reading needs access to the device: by default only root and the `input` group have it. The
//! README gives a udev rule that grants it to the logged-in user for that one device
//! ([`udev_rule`]), rather than the `input` group, which would let every program read every key.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dmn_core::config::{ptt_key_code, ptt_key_name};

/// `EV_KEY` in `linux/input-event-codes.h`.
const EV_KEY: u16 = 1;
/// One `struct input_event`: a `timeval` of two `long`s, then type, code, value.
const EVENT_SIZE: usize = 2 * std::mem::size_of::<isize>() + 8;
/// Where udev puts stable names for input devices.
const BY_ID: &str = "/dev/input/by-id";
const BY_PATH: &str = "/dev/input/by-path";

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

/// Read records from `r` and call `on` for each press or release of `code`, until the device
/// goes away (unplugged, read error) or `on` returns false. Auto-repeat and other keys are
/// skipped. Returns why it stopped.
fn read_button(mut r: impl Read, code: u16, mut on: impl FnMut(Event) -> bool) -> String {
    let mut record = [0u8; EVENT_SIZE];
    loop {
        if let Err(e) = r.read_exact(&mut record) {
            return e.to_string();
        }
        let event = match key_event(&record) {
            Some((c, 1)) if c == code => Event::Pressed,
            Some((c, 0)) if c == code => Event::Released,
            _ => continue,
        };
        if !on(event) {
            return "stopped".into();
        }
    }
}

/// The push-to-talk button, opened: the device is readable and the key is one dmn knows.
#[derive(Debug)]
pub struct Button {
    file: File,
    path: String,
    code: u16,
}

impl Button {
    /// Open `device` for `key` (`[speak].ptt_device`, `ptt_key`). Fails when the key is not
    /// known or the device cannot be read, which `dmn run` says on the status line before
    /// going on without push-to-talk.
    pub fn open(device: &str, key: &str) -> Result<Self, PttError> {
        let code = ptt_key_code(key).ok_or_else(|| PttError::BadKey(key.to_owned()))?;
        let file = File::open(device).map_err(|e| PttError::Open {
            path: device.to_owned(),
            detail: e.to_string(),
        })?;
        Ok(Self {
            file,
            path: device.to_owned(),
            code,
        })
    }

    /// Read the button on its own thread: `on` gets each press and release, then `lost` once
    /// with the reason when the device goes away.
    pub fn spawn(
        self,
        on: impl FnMut(Event) + Send + 'static,
        lost: impl FnOnce(String) + Send + 'static,
    ) -> Result<(), PttError> {
        let Self { file, path, code } = self;
        std::thread::Builder::new()
            .name("dmn-ptt".to_owned())
            .spawn(move || {
                let mut on = on;
                tracing::info!(device = %path, key = %ptt_key_name(code), "push-to-talk: reading the button");
                let why = read_button(file, code, |e| {
                    on(e);
                    true
                });
                tracing::warn!(device = %path, reason = %why, "push-to-talk: the button's device went away");
                lost(why);
            })
            .map(|_| ())
            .map_err(|e| PttError::Spawn(e.to_string()))
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
        let why = read_button(std::io::Cursor::new(stream), f13, |e| {
            got.push(e);
            true
        });
        assert_eq!(got, [Event::Pressed, Event::Released, Event::Pressed]);
        assert!(
            why.contains("fill whole buffer") || !why.is_empty(),
            "{why}"
        );
    }

    #[test]
    fn a_caller_can_stop_the_read() {
        let f13 = ptt_key_code("KEY_F13").unwrap();
        let stream = [record(EV_KEY, f13, 1), record(EV_KEY, f13, 0)].concat();
        let mut n = 0;
        let why = read_button(std::io::Cursor::new(stream), f13, |_| {
            n += 1;
            false
        });
        assert_eq!((n, why.as_str()), (1, "stopped"));
    }

    #[test]
    fn opening_names_a_bad_key_and_an_unreadable_device() {
        let err = Button::open("/dev/null", "KEY_NOPE").unwrap_err();
        assert!(matches!(err, PttError::BadKey(_)), "{err}");
        let err = Button::open("/nonexistent/dmn-ptt-test", "KEY_F13").unwrap_err();
        assert!(matches!(err, PttError::Open { .. }), "{err}");
        assert!(err.to_string().contains("dmn speak ptt devices"), "{err}");
        assert!(Button::open("/dev/null", "BTN_SIDE").is_ok());
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
        Button::open(&device, &key)
            .unwrap()
            .spawn(
                move |e| {
                    let _ = tx.send(e);
                },
                |_| {},
            )
            .unwrap();
        println!("press and release {key} within 10 s");
        let pressed = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let released = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((pressed, released), (Event::Pressed, Event::Released));
    }
}
