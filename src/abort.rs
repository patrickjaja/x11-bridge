//! `abort-watch`: report the user's Escape over XInput2 raw key events.
//!
//! The watcher selects `XI_RawKeyPress` / `XI_RawKeyRelease` and
//! `XI_HierarchyChanged` on the root window for every device. Raw events are
//! delivered to every client that selected them, also while another client
//! holds a keyboard grab (XI 2.1+, which is why the version is checked), and
//! they never consume the key: the user's Escape still reaches the focused
//! window. Nothing is grabbed, so nothing has to be undone at exit beyond
//! closing the connection.
//!
//! The model's own Escape is told apart by its source device: x11-bridge
//! injects with XTEST, and the X server routes XTEST input through the master
//! keyboard's "XTEST keyboard" slave. Raw events carry that slave as
//! `sourceid`, so an event whose source is an XTEST device is never an abort.
//! XTEST devices are identified by the server's boolean `XTEST Device`
//! property (falling back to the `... XTEST keyboard` / `... XTEST pointer`
//! name), at arm and again on every hierarchy change (hotplug, new masters).
//!
//! The protocol, signals, parent death and exit codes are the shared
//! `cu-abort-watch` crate's; this module is only the [`Backend`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cu_abort_watch::{AbortSink, AbortWatchDoctor, Backend, BoxError, Mechanism, Ready};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{
    self, ConnectionExt as _, DeviceId, DeviceType, EventMask, KeyEventFlags, XIEventMask,
    XIGetPropertyItems,
};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keysym, Mapping, Window};
use x11rb::rust_connection::RustConnection;

/// `XIAllDevices`: select on every device, master and slave.
const XI_ALL_DEVICES: DeviceId = 0;
/// The XInput version this watcher announces and needs at least (2.1: raw
/// events are delivered regardless of grabs).
const XI_WANT: (u16, u16) = (2, 2);
const XI_MIN: (u16, u16) = (2, 1);
/// Device property the X server sets to 1 on its XTEST devices.
const XTEST_PROPERTY: &[u8] = b"XTEST Device";
/// Name suffixes of the XTEST slave devices the server creates per master.
const XTEST_NAME_SUFFIXES: [&[u8]; 2] = [b" XTEST keyboard", b" XTEST pointer"];
const ESCAPE: Keysym = xkeysym::key::Escape;

// ---------------------------------------------------------------------------
// Pure filter (unit tested)
// ---------------------------------------------------------------------------

/// One device as the filter needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceEntry {
    pub id: DeviceId,
    pub xtest: bool,
}

/// Whether a device is an XTEST (synthetic) device: the server's `XTEST
/// Device` property when it could be read, else the server-assigned name.
pub(crate) fn is_xtest_device(name: &[u8], property: Option<bool>) -> bool {
    if let Some(p) = property {
        return p;
    }
    XTEST_NAME_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Keycodes that produce Escape in any column of the core keyboard mapping
/// (`keysyms` is `count * per` entries starting at `min_keycode`).
pub(crate) fn escape_keycodes(min_keycode: u8, per: usize, keysyms: &[Keysym]) -> HashSet<u32> {
    if per == 0 {
        return HashSet::new();
    }
    keysyms
        .chunks(per)
        .enumerate()
        .filter(|(_, syms)| syms.contains(&ESCAPE))
        .map(|(i, _)| u32::from(min_keycode) + i as u32)
        .collect()
}

/// A held key older than this no longer suppresses a press: its release
/// may have been lost (device disabled while held, VT switch, suspend).
pub(crate) const HELD_EXPIRY: Duration = Duration::from_secs(2);

/// Decides which raw key events are the user's Escape.
///
/// Errs toward reporting: a duplicate `abort` is harmless (the sink
/// coalesces and the caller's abort is idempotent), a missed one is not.
#[derive(Debug, Default)]
pub(crate) struct KeyFilter {
    escape: HashSet<u32>,
    xtest: HashSet<DeviceId>,
    /// Per source device: the last key pressed and when, until released.
    /// Only suppresses a press of the same key that comes before any other
    /// key press from that device and within [`HELD_EXPIRY`].
    last: HashMap<DeviceId, (u32, Instant)>,
}

impl KeyFilter {
    pub(crate) fn set_escape(&mut self, keycodes: HashSet<u32>) {
        self.escape = keycodes;
    }

    /// New device list (arm, every hierarchy event). Held-key state is
    /// dropped: a device that was removed, disabled or re-attached may never
    /// send its release.
    pub(crate) fn set_devices(&mut self, devices: &[DeviceEntry]) {
        self.xtest = devices.iter().filter(|d| d.xtest).map(|d| d.id).collect();
        self.last.clear();
    }

    /// A raw key press. True when it is a physical Escape press to report.
    ///
    /// Events are keyed by `sourceid` (the physical slave), so the slave copy
    /// and the master copy of one press (with `XIAllDevices` the server sends
    /// both) are the same press: the first reports, the second is a repeat.
    /// A server sending only one of them still reports.
    pub(crate) fn press(
        &mut self,
        sourceid: DeviceId,
        keycode: u32,
        repeat_flag: bool,
        now: Instant,
    ) -> bool {
        if self.xtest.contains(&sourceid) {
            return false;
        }
        // An unknown source (added after the last hierarchy scan, its event
        // not yet seen) is a real device: XTEST devices only appear with a
        // new master, whose hierarchy event precedes any of its input.
        let held = self.last.insert(sourceid, (keycode, now));
        if repeat_flag || !self.escape.contains(&keycode) {
            return false;
        }
        !matches!(held, Some((k, t)) if k == keycode && now.saturating_duration_since(t) < HELD_EXPIRY)
    }

    /// A raw key release (either copy).
    pub(crate) fn release(&mut self, sourceid: DeviceId, keycode: u32) {
        if self.last.get(&sourceid).is_some_and(|(k, _)| *k == keycode) {
            self.last.remove(&sourceid);
        }
    }
}

/// The negotiated XInput version must be at least 2.1.
pub(crate) fn require_xi_version(got: (u16, u16)) -> Result<()> {
    if got < XI_MIN {
        bail!(
            "XInput {}.{} is older than 2.1 (raw key events would stop during grabs)",
            got.0,
            got.1
        );
    }
    Ok(())
}

/// Run `f`, retrying a failure twice (50 ms, then 200 ms later): a device
/// that vanishes mid-scan answers BadDevice once.
fn with_retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut last = None;
    for delay in [0, 50, 200] {
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .expect("at least one attempt")
        .context("failed after 3 attempts"))
}

// ---------------------------------------------------------------------------
// X side
// ---------------------------------------------------------------------------

/// Check the XInput extension and announce version 2.2; the server answers
/// with the version both support, which must be at least 2.1.
fn check_xi2(conn: &RustConnection) -> Result<(u16, u16)> {
    let present = conn
        .extension_information(xinput::X11_EXTENSION_NAME)
        .context("failed to query the XInput extension")?
        .is_some();
    if !present {
        bail!("the X server has no XInput extension");
    }
    let reply = conn
        .xinput_xi_query_version(XI_WANT.0, XI_WANT.1)
        .context("failed to query the XInput version")?
        .reply()
        .context("failed to read the XInput version")?;
    let got = (reply.major_version, reply.minor_version);
    require_xi_version(got)?;
    Ok(got)
}

fn connect() -> Result<(RustConnection, Window)> {
    let (conn, screen) =
        x11rb::connect(None).context("failed to connect to the X server (is $DISPLAY set?)")?;
    let root = conn
        .setup()
        .roots
        .get(screen)
        .ok_or_else(|| anyhow!("X server reported no screen at index {screen}"))?
        .root;
    Ok((conn, root))
}

/// Whether the server is Xwayland (it advertises the `XWAYLAND` extension).
fn is_xwayland(conn: &RustConnection) -> Result<bool> {
    Ok(conn
        .extension_information("XWAYLAND")
        .context("failed to query the XWAYLAND extension")?
        .is_some())
}

/// `syntheticSafe` for the `ready` line. On a real X server XTEST input
/// always carries the XTEST slave as its source, so the model's Escape can
/// never look physical. Xwayland started with `-enable-ei-portal` (GNOME,
/// KDE) forwards XTEST through libei to the compositor, which sends the key
/// back as a Wayland key event: it then arrives on `xwayland-keyboard`, a
/// non-XTEST device, and would read as the user's Escape. That cannot be
/// detected from the X side, so Xwayland is reported as not synthetic-safe
/// (the caller then suspends around the model's Escape).
fn synthetic_safe(xwayland: bool) -> bool {
    !xwayland
}

/// `doctor`'s `abortWatch` key.
pub fn doctor() -> AbortWatchDoctor {
    match connect().and_then(|(conn, _)| check_xi2(&conn)) {
        Ok(_) => AbortWatchDoctor::available(Mechanism::Xi2),
        Err(e) => AbortWatchDoctor::unavailable(Some(Mechanism::Xi2), format!("{e:#}")),
    }
}

/// All devices, each classified as XTEST or not.
fn scan_devices(conn: &RustConnection) -> Result<Vec<DeviceEntry>> {
    let prop = conn
        .intern_atom(true, XTEST_PROPERTY)
        .context("failed to look up the XTEST Device property")?
        .reply()
        .context("failed to read the XTEST Device property atom")?
        .atom;
    let reply = conn
        .xinput_xi_query_device(XI_ALL_DEVICES)
        .context("failed to list input devices")?
        .reply()
        .context("failed to read the input device list")?;
    let mut out = Vec::with_capacity(reply.infos.len());
    for info in &reply.infos {
        if info.type_ == DeviceType::MASTER_POINTER || info.type_ == DeviceType::MASTER_KEYBOARD {
            // Masters never are a raw event's source.
            out.push(DeviceEntry {
                id: info.deviceid,
                xtest: false,
            });
            continue;
        }
        let property = if prop == u32::from(AtomEnum::NONE) {
            None
        } else {
            xtest_property(conn, info.deviceid, prop)
        };
        out.push(DeviceEntry {
            id: info.deviceid,
            xtest: is_xtest_device(&info.name, property),
        });
    }
    Ok(out)
}

/// The device's `XTEST Device` value, `None` when unset or unreadable (a
/// device that vanished mid-scan answers BadDevice).
fn xtest_property(conn: &RustConnection, device: DeviceId, prop: u32) -> Option<bool> {
    let reply = conn
        .xinput_xi_get_property(device, false, prop, u32::from(AtomEnum::ANY), 0, 1)
        .ok()?
        .reply()
        .ok()?;
    if reply.type_ == u32::from(AtomEnum::NONE) {
        return None;
    }
    match reply.items {
        XIGetPropertyItems::Data8(v) => v.first().map(|b| *b != 0),
        XIGetPropertyItems::Data16(v) => v.first().map(|b| *b != 0),
        XIGetPropertyItems::Data32(v) => v.first().map(|b| *b != 0),
        _ => None,
    }
}

fn fetch_escape_keycodes(conn: &RustConnection) -> Result<HashSet<u32>> {
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let reply = conn
        .get_keyboard_mapping(min, max - min + 1)
        .context("failed to request the keyboard mapping")?
        .reply()
        .context("failed to read the keyboard mapping")?;
    Ok(escape_keycodes(
        min,
        usize::from(reply.keysyms_per_keycode),
        &reply.keysyms,
    ))
}

fn select_events(conn: &RustConnection, root: Window, mask: XIEventMask) -> Result<()> {
    conn.xinput_xi_select_events(
        root,
        &[EventMask {
            deviceid: XI_ALL_DEVICES,
            mask: vec![mask],
        }],
    )
    .context("failed to select XInput2 events")?
    .check()
    .context("the X server refused the XInput2 event selection")
}

/// The `abort-watch` backend: mechanism `xi2`, synthetic-safe except on
/// Xwayland (see [`synthetic_safe`]).
#[derive(Default)]
pub struct Xi2Backend {
    running: Option<Running>,
}

struct Running {
    stop: Arc<AtomicBool>,
}

impl Xi2Backend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Backend for Xi2Backend {
    fn arm(&mut self, sink: AbortSink) -> std::result::Result<Ready, BoxError> {
        let (conn, root) = connect()?;
        let version = check_xi2(&conn)?;
        let xwayland = is_xwayland(&conn)?;
        // Select first, then read the device list and keymap: a change in
        // between arrives as an event and is re-read.
        select_events(
            &conn,
            root,
            XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE | XIEventMask::HIERARCHY,
        )?;
        let mut filter = KeyFilter::default();
        filter.set_devices(&scan_devices(&conn)?);
        filter.set_escape(fetch_escape_keycodes(&conn)?);
        if filter.escape.is_empty() {
            // Not fatal: a later keymap change may add it.
            eprintln!("x11-bridge abort-watch: no keycode maps to Escape yet");
        }
        eprintln!(
            "x11-bridge abort-watch: XInput {}.{}{}, XTEST devices {:?}, Escape keycodes {:?}",
            version.0,
            version.1,
            if xwayland { " (Xwayland)" } else { "" },
            filter.xtest,
            filter.escape
        );

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("xi2-abort-watch".into())
            .spawn(move || event_loop(conn, filter, sink, thread_stop))
            .context("failed to start the XInput2 event thread")?;
        self.running = Some(Running { stop });
        Ok(Ready {
            mechanism: Mechanism::Xi2,
            synthetic_safe: synthetic_safe(xwayland),
        })
    }

    fn disarm(&mut self) {
        // Passive selection only: the server drops it with the connection
        // when the process exits. Keep the thread from reporting the
        // connection teardown as a failure.
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::SeqCst);
        }
    }
}

fn event_loop(conn: RustConnection, mut filter: KeyFilter, sink: AbortSink, stop: Arc<AtomicBool>) {
    let fail = |msg: String| {
        if !stop.load(Ordering::SeqCst) {
            sink.fail(msg);
        }
    };
    loop {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                fail(format!("X connection lost: {e}"));
                return;
            }
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match event {
            Event::XinputRawKeyPress(e) => {
                let repeat = e.flags & KeyEventFlags::KEY_REPEAT == KeyEventFlags::KEY_REPEAT;
                if filter.press(e.sourceid, e.detail, repeat, Instant::now()) {
                    sink.abort();
                }
            }
            Event::XinputRawKeyRelease(e) => filter.release(e.sourceid, e.detail),
            Event::XinputHierarchy(_) => match with_retry(|| scan_devices(&conn)) {
                Ok(devices) => {
                    filter.set_devices(&devices);
                    eprintln!(
                        "x11-bridge abort-watch: devices changed, XTEST devices {:?}",
                        filter.xtest
                    );
                }
                Err(e) => {
                    fail(format!("failed to re-read input devices: {e:#}"));
                    return;
                }
            },
            Event::MappingNotify(e) if e.request == Mapping::KEYBOARD => {
                match with_retry(|| fetch_escape_keycodes(&conn)) {
                    Ok(k) => filter.set_escape(k),
                    Err(e) => {
                        fail(format!("failed to re-read the keyboard mapping: {e:#}"));
                        return;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> KeyFilter {
        let mut f = KeyFilter::default();
        // Xvfb layout: 2/3 masters, 4/5 XTEST slaves, 6/7 real slaves.
        f.set_devices(&[
            DeviceEntry {
                id: 2,
                xtest: false,
            },
            DeviceEntry {
                id: 3,
                xtest: false,
            },
            DeviceEntry { id: 4, xtest: true },
            DeviceEntry { id: 5, xtest: true },
            DeviceEntry {
                id: 6,
                xtest: false,
            },
            DeviceEntry {
                id: 7,
                xtest: false,
            },
        ]);
        f.set_escape([9].into_iter().collect());
        f
    }

    #[test]
    fn classifies_xtest_devices() {
        assert!(is_xtest_device(b"Virtual core XTEST keyboard", None));
        assert!(is_xtest_device(b"Virtual core XTEST pointer", None));
        assert!(is_xtest_device(b"second XTEST keyboard", None));
        assert!(!is_xtest_device(b"AT Translated Set 2 keyboard", None));
        assert!(!is_xtest_device(b"xwayland-keyboard:13", None));
        assert!(!is_xtest_device(b"Xvfb keyboard", None));
        // The property wins over the name, both ways.
        assert!(is_xtest_device(b"renamed", Some(true)));
        assert!(!is_xtest_device(
            b"Virtual core XTEST keyboard",
            Some(false)
        ));
    }

    /// The server really exposes the `XTEST Device` property (so the name
    /// fallback is not what classifies devices on a stock server).
    #[test]
    #[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
    fn xtest_property_is_read_from_the_server() {
        let (conn, _) = connect().expect("connect to Xvfb");
        check_xi2(&conn).unwrap();
        let prop = conn
            .intern_atom(true, XTEST_PROPERTY)
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        assert_ne!(prop, u32::from(AtomEnum::NONE));
        let devices = conn
            .xinput_xi_query_device(XI_ALL_DEVICES)
            .unwrap()
            .reply()
            .unwrap()
            .infos;
        let mut seen_xtest = 0;
        let mut seen_real = 0;
        for d in devices.iter().filter(|d| {
            d.type_ == DeviceType::SLAVE_KEYBOARD || d.type_ == DeviceType::SLAVE_POINTER
        }) {
            let named_xtest = XTEST_NAME_SUFFIXES.iter().any(|s| d.name.ends_with(s));
            let p = xtest_property(&conn, d.deviceid, prop);
            if named_xtest {
                assert_eq!(p, Some(true), "{:?}", String::from_utf8_lossy(&d.name));
                seen_xtest += 1;
            } else {
                assert_ne!(p, Some(true), "{:?}", String::from_utf8_lossy(&d.name));
                seen_real += 1;
            }
        }
        assert!(seen_xtest >= 2 && seen_real >= 1);
        let scanned = scan_devices(&conn).unwrap();
        assert_eq!(scanned.iter().filter(|d| d.xtest).count(), seen_xtest);
    }

    fn t(ms: u64) -> Instant {
        // A fixed base so tests are independent of the clock.
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    #[test]
    fn physical_escape_reported_once_per_press() {
        let mut f = filter();
        // Slave copy reports, master copy (same sourceid) is the same press.
        assert!(f.press(7, 9, false, t(0)));
        assert!(!f.press(7, 9, false, t(0)));
        // Held key: repeats (with or without the flag) do not report, and an
        // unflagged repeat keeps the hold fresh past the expiry.
        assert!(!f.press(7, 9, true, t(30)));
        assert!(!f.press(7, 9, false, t(1500)));
        assert!(!f.press(7, 9, false, t(3000)));
        f.release(7, 9);
        f.release(7, 9); // master copy of the release
        assert!(f.press(7, 9, false, t(3100)));
    }

    #[test]
    fn only_one_copy_of_each_event_still_works() {
        // A server that sends only the master copy (sourceid is the slave).
        let mut f = filter();
        assert!(f.press(7, 9, false, t(0)));
        f.release(7, 9);
        assert!(f.press(7, 9, false, t(100)));
    }

    #[test]
    fn repeat_flag_never_reports() {
        let mut f = filter();
        assert!(!f.press(7, 9, true, t(0)));
        assert!(!f.press(7, 9, true, t(5000)));
    }

    #[test]
    fn missed_release_expires() {
        let mut f = filter();
        assert!(f.press(7, 9, false, t(0)));
        // The release never arrives (device disabled while held, VT
        // switch, suspend). A press soon after is still taken as repeat ...
        assert!(!f.press(7, 9, false, t(500)));
        // ... but once the hold is older than HELD_EXPIRY a press is new.
        assert!(f.press(7, 9, false, t(500) + HELD_EXPIRY));
    }

    #[test]
    fn missed_release_cleared_by_other_key() {
        let mut f = filter();
        assert!(f.press(7, 9, false, t(0)));
        // Release lost, then the user types something else on that keyboard.
        assert!(!f.press(7, 38, false, t(100)));
        assert!(f.press(7, 9, false, t(200)), "Escape after another key");
        // Other devices do not interfere with each other's hold.
        assert!(f.press(6, 9, false, t(250)));
        assert!(!f.press(7, 9, false, t(300)));
    }

    #[test]
    fn missed_release_cleared_by_hierarchy_event() {
        let mut f = filter();
        assert!(f.press(7, 9, false, t(0)));
        // Keyboard disabled and re-enabled while held: still listed.
        f.set_devices(&[
            DeviceEntry { id: 5, xtest: true },
            DeviceEntry {
                id: 7,
                xtest: false,
            },
        ]);
        assert!(f.press(7, 9, false, t(100)));
    }

    #[test]
    fn xtest_and_other_keys_ignored() {
        let mut f = filter();
        assert!(!f.press(5, 9, false, t(0)), "XTEST keyboard (either copy)");
        assert!(!f.press(7, 38, false, t(0)), "not Escape");
        // An unknown, newly plugged keyboard counts as physical.
        assert!(f.press(12, 9, false, t(0)));
        // Floating slave (no master copy) still reports.
        assert!(f.press(6, 9, false, t(0)));
    }

    #[test]
    fn hierarchy_change_updates_xtest_set() {
        let mut f = filter();
        // A new master adds an XTEST slave 9 and a keyboard appears as 10.
        f.set_devices(&[
            DeviceEntry { id: 5, xtest: true },
            DeviceEntry { id: 9, xtest: true },
            DeviceEntry {
                id: 10,
                xtest: false,
            },
        ]);
        assert!(!f.press(9, 9, false, t(0)));
        assert!(f.press(10, 9, false, t(0)));
    }

    #[test]
    fn keymap_change_moves_escape() {
        let mut f = filter();
        f.set_escape([66].into_iter().collect()); // caps:escape-like remap
        assert!(!f.press(7, 9, false, t(0)));
        assert!(f.press(7, 66, false, t(10)));
    }

    #[test]
    fn xi_version_floor() {
        assert!(require_xi_version((2, 0)).is_err());
        assert!(require_xi_version((1, 5)).is_err());
        assert!(require_xi_version((2, 1)).is_ok());
        assert!(require_xi_version((2, 4)).is_ok());
        let msg = require_xi_version((2, 0)).unwrap_err().to_string();
        assert!(msg.contains("2.0") && msg.contains("2.1"), "{msg}");
    }

    #[test]
    fn retry_is_bounded_and_recovers() {
        let mut n = 0;
        let r = with_retry(|| {
            n += 1;
            if n < 2 { bail!("transient") } else { Ok(n) }
        });
        assert_eq!(r.unwrap(), 2);
        let mut n = 0;
        let r: Result<()> = with_retry(|| {
            n += 1;
            bail!("dead")
        });
        assert!(r.is_err());
        assert_eq!(n, 3);
    }

    #[test]
    fn escape_keycodes_from_mapping() {
        let none = 0;
        let a = xkeysym::key::a;
        // min keycode 8, 2 columns: 8=a, 9=Escape, 10=none/Escape (2nd col)
        let syms = [a, a, ESCAPE, none, none, ESCAPE];
        let got = escape_keycodes(8, 2, &syms);
        assert_eq!(got, [9, 10].into_iter().collect());
        assert!(escape_keycodes(8, 0, &syms).is_empty());
    }
}
