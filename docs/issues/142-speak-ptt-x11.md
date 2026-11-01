# #142 — speak: push-to-talk reads the button from XWayland, no udev rule needed

Labels: `size:M, model:opus, m4, lane:audio, backlog`

Proposed 2026-10-31, while setting up #124's push-to-talk (PR #127) on the dev box. #124 reads the button straight from its `/dev/input` device, which needs a udev `uaccess` rule written with `sudo` before the first use. The owner asked: "cant we run it in x11?" On the same day Discord's own push-to-talk was made to work while unfocused on COSMIC by running Discord under XWayland (`--ozone-platform=x11` plus `ELECTRON_OZONE_PLATFORM_HINT=x11`) and setting COSMIC Settings → Applications → X11 applications compatibility → Global shortcuts in X11 applications to all keys (`~/.config/cosmic/com.system76.CosmicComp/v1/xwayland_eavesdropping` = `(keyboard: All, pointer: true)`). dmn can read the button the same way, with no udev rule and no `sudo`.

**Checked 2026-10-31** on the dev box (COSMIC, Wayland, Xwayland on `:1`): `xinput test-xi2 --root` saw XI2 `RawKeyPress`/`RawKeyRelease` (Tab, Return) and `RawButtonPress`/`RawButtonRelease` (left button) while a Wayland terminal had focus. The events report `child 0x0`, so they were forwarded by COSMIC rather than delivered to a focused X11 window. cosmic-comp (`src/xwayland.rs`, `xwayland_notify_key_event`) forwards each key to Xwayland's `wl_keyboard` when no X11 window has focus, filtered by the `keyboard` mode (`None` default, `Modifiers`, `Combinations`, `All`); mouse buttons go through `wl_pointer` when `pointer` is true.

## Goal
`[speak].ptt_source = "x11"` reads #124's push-to-talk button from XWayland as XInput2 raw events on the root window, so push-to-talk works without a udev rule on COSMIC (with the X11 global-shortcuts setting on), KDE Plasma (its equivalent "Legacy X11 App Support" setting) and X11 sessions. The device read stays the default and the only way on desktops that never pass keys to X11 apps (GNOME on Wayland).

## Size / Model / Turns
M | Opus | 45 (docs/PROCESS.md). One new `ButtonSource` (PR #127 already separates reading the button from the push-to-talk logic); no changes to `Speaking`, the hold matching or the VAD path.

## Scope
`apps/dmn-cli/src/ptt.rs` (a second `ButtonSource`), `run.rs` / `doctor.rs` / `speak.rs` (choose it, check it, learn with it), `crates/dmn-core/src/config.rs` and its template, README, SPEC, docs/DEPENDENCIES.md.

### Reading
- `ptt_source = "device"` (default; today's evdev read of `ptt_device`) or `"x11"`. With `"x11"`, `ptt_device` is not used and may be empty; `mode = "ptt"` needs only `ptt_key`.
- `ptt_key` keeps #124's evdev names and numbers, so a config moves between sources unchanged. Keys map to X11 keycodes as evdev code + 8; buttons map `BTN_LEFT`→1, `BTN_MIDDLE`→2, `BTN_RIGHT`→3, `BTN_SIDE`→8, `BTN_EXTRA`→9, and the rest as Xwayland numbers them (check Xwayland's `xwl_seat` button mapping and test the table).
- Connect to `$DISPLAY` with x11rb's pure-Rust connection (no libxcb, no `unsafe`), select `XI_RawKeyPress`/`XI_RawKeyRelease`/`XI_RawButtonPress`/`XI_RawButtonRelease` for all master devices on the root window, and turn the matching code into #124's press/release `Event`s. Raw events arrive whichever window has focus, so the button also works while Discord itself (running under X11) is focused. A repeated press without a release between (autorepeat, if Xwayland sends it) is ignored, as the device reader ignores value 2.
- As with #124, the connection is open only while push-to-talk is the voice mode; leaving it drops the `Reading` and closes the connection. A closed connection (Xwayland gone) calls `lost`, the status line says so and the run goes on in translate mode.
- Known COSMIC behaviour, documented rather than worked around: moving focus to an X11 window while the button is held makes cosmic-comp send a release (`xwayland_reset_eavesdropping`), which ends that hold.

### Learn and doctor
- `dmn speak ptt learn --x11` waits for the next raw key or button press on XWayland and saves `ptt_source = "x11"` and `ptt_key` (the evdev name when the code has one). Without `--x11` it is #124's device learn and saves `ptt_source = "device"`.
- `dmn doctor` with `ptt_source = "x11"`: `$DISPLAY` set and reachable, XInput 2 present (else the fix: run under XWayland or use the device source). On COSMIC, read `xwayland_eavesdropping` and warn with the Settings path when it cannot pass the configured key: `None`, or `Modifiers`/`Combinations` for a non-modifier key, or `pointer: false` for a button. Other desktops: say that the desktop must pass keys to X11 apps and name KDE's setting.
- `dmn speak ptt devices` ends with one line pointing at `learn --x11` as the no-`sudo` alternative when XWayland is reachable.

### Docs
README's push-to-talk section offers both sources: the device read (any Linux desktop, one udev rule) and X11 (no `sudo`; COSMIC/KDE setting or an X11 session; not GNOME Wayland). It says plainly that the COSMIC/KDE setting lets every X11 app see all keys while in the background. SPEC and the config template gain `ptt_source`. DEPENDENCIES.md gains the x11rb line (why: XInput2 raw events without libxcb or `unsafe`).

Out of scope: the GlobalShortcuts portal (COSMIC has none yet: pop-os/xdg-desktop-portal-cosmic#4), changing desktop settings from dmn, XRecord, grabbing the key away from other apps, and changing #124's push-to-talk behaviour.

## Depends on
#124 (PR #127): `ButtonSource`, `Reading`, `[speak].ptt_key`, `dmn speak ptt learn`, the doctor check.

## Acceptance
- [ ] `ptt_source = "x11"` reads `ptt_key` from XWayland raw events; `"device"` (default) is unchanged, and #124's tests pass untouched.
- [ ] Unit tests: the evdev → X11 key and button table, raw-event decoding to press/release (repeats ignored), and config validation (`x11` needs `ptt_key`, not `ptt_device`; an unknown source is a config error).
- [ ] An `#[ignore]` live test against `$DISPLAY` injects a press and release with XTEST and sees one `Event` each; dropping the `Reading` closes the connection.
- [ ] `dmn speak ptt learn --x11` saves `ptt_source` and `ptt_key`; `dmn doctor` reports an unreachable display, missing XInput 2 and (on COSMIC) an `xwayland_eavesdropping` setting that cannot pass the key, each with its fix.
- [ ] Losing the X connection mid-run is said on the status line and the run goes on in translate mode, as for an unplugged device.
- [ ] Checked by hand on the dev box: push-to-talk via `"x11"` with a Wayland terminal focused and with Discord (X11) focused; result in the PR body.
- [ ] README, SPEC, config template and DEPENDENCIES.md updated as above.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-cli ptt
cargo test -p dmn-cli doctor
cargo test -p dmn-cli -- --ignored x11      # dev box with Xwayland
just check
just test
just lint
```
CI has no X server; everything that needs one is `#[ignore]`.
