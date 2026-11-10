//! `abort-watch` against a live X server.
//!
//! `#[ignore]`d: they need a throwaway X server and inject keys into it. Run
//! them only under `scripts/test-xvfb.sh` (a private Xvfb), never against a
//! real session.
//!
//! A "physical" Escape is produced with XTEST's XInput-1 device path
//! (`FakeInput` with a `DeviceKeyPress` type and the device id of Xvfb's own
//! keyboard slave): the server then routes the key through that slave
//! instead of the master's "XTEST keyboard" slave, so the raw event's
//! `sourceid` is the real keyboard, exactly what a hardware key produces. The
//! model's Escape is the bridge's own `key-sequence --keys escape` (core
//! XTEST, source = the XTEST slave).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::Value;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{
    self, ChangeMode, ConnectionExt as _, DeviceType, HierarchyChange, HierarchyChangeData,
    HierarchyChangeDataAddMaster, HierarchyChangeDataRemoveMaster,
};
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus, ModMask};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const BIN: &str = env!("CARGO_BIN_EXE_x11-bridge");
const QUIET: Duration = Duration::from_millis(600);
const WAIT: Duration = Duration::from_secs(3);

struct Watcher {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Watcher {
    fn start() -> Self {
        let mut child = Command::new(BIN)
            .arg("abort-watch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn abort-watch");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut w = Self {
            child,
            stdin,
            lines,
        };
        assert_eq!(
            w.next(WAIT).as_deref(),
            Some(r#"{"event":"ready","mechanism":"xi2","syntheticSafe":true}"#)
        );
        w
    }

    fn next(&mut self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    fn expect_abort(&mut self, what: &str) {
        assert_eq!(
            self.next(WAIT).as_deref(),
            Some(r#"{"event":"abort"}"#),
            "{what}: expected an abort"
        );
    }

    fn expect_quiet(&mut self, what: &str) {
        if let Some(l) = self.next(QUIET) {
            panic!("{what}: expected no output, got {l}");
        }
    }

    fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{line}").unwrap();
        s.flush().unwrap();
    }

    /// Close stdin and return the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st.code();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("abort-watch did not exit after stdin EOF");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct X {
    conn: RustConnection,
    root: u32,
    xi_first_event: u8,
}

impl X {
    fn connect() -> Self {
        let (conn, screen) = x11rb::connect(None).expect("connect to the test X server");
        let root = conn.setup().roots[screen].root;
        let xi_first_event = conn
            .extension_information(xinput::X11_EXTENSION_NAME)
            .unwrap()
            .expect("XInput")
            .first_event;
        // Announce XI 2.2 (needed for the hierarchy requests below).
        conn.xinput_xi_query_version(2, 2).unwrap().reply().unwrap();
        Self {
            conn,
            root,
            xi_first_event,
        }
    }

    fn device_id(&self, name: &str) -> Option<u16> {
        let reply = self
            .conn
            .xinput_xi_query_device(0u16)
            .unwrap()
            .reply()
            .unwrap();
        reply
            .infos
            .iter()
            .find(|d| d.name == name.as_bytes())
            .map(|d| d.deviceid)
    }

    /// The (non-XTEST) slave keyboard Xvfb creates.
    fn real_keyboard(&self) -> u16 {
        let reply = self
            .conn
            .xinput_xi_query_device(0u16)
            .unwrap()
            .reply()
            .unwrap();
        reply
            .infos
            .iter()
            .find(|d| {
                d.type_ == DeviceType::SLAVE_KEYBOARD && !d.name.windows(5).any(|w| w == b"XTEST")
            })
            .map(|d| d.deviceid)
            .expect("a non-XTEST slave keyboard")
    }

    fn keycode_for(&self, keysym: u32) -> u8 {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let r = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .unwrap()
            .reply()
            .unwrap();
        let per = usize::from(r.keysyms_per_keycode);
        r.keysyms
            .chunks(per)
            .position(|c| c.first() == Some(&keysym))
            .map(|i| min + i as u8)
            .expect("keycode for keysym")
    }

    /// Press and release `keycode` on `device` (XInput-1 XTEST path).
    fn device_key(&self, device: u16, keycode: u8) {
        let press = self.xi_first_event + 1; // DeviceKeyPress
        let release = self.xi_first_event + 2; // DeviceKeyRelease
        let dev = device as u8;
        self.conn
            .xtest_fake_input(press, keycode, 0, self.root, 0, 0, dev)
            .unwrap()
            .check()
            .expect("XTEST device key press");
        self.conn
            .xtest_fake_input(release, keycode, 0, self.root, 0, 0, dev)
            .unwrap()
            .check()
            .expect("XTEST device key release");
    }

    fn sync(&self) {
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }
}

fn escape_keycode(x: &X) -> u8 {
    x.keycode_for(0xff1b)
}

fn model_escape() {
    let out = Command::new(BIN)
        .args(["key-sequence", "--keys", "escape"])
        .output()
        .expect("run key-sequence");
    assert!(out.status.success(), "key-sequence failed: {out:?}");
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn model_escape_ignored_physical_escape_aborts() {
    let x = X::connect();
    let esc = escape_keycode(&x);
    let kbd = x.real_keyboard();
    let mut w = Watcher::start();

    model_escape();
    w.expect_quiet("model Escape (key-sequence, XTEST)");

    x.device_key(kbd, esc);
    x.sync();
    w.expect_abort("physical Escape");
    w.expect_quiet("one abort per press");

    // A different physical key is not an abort.
    x.device_key(kbd, x.keycode_for(u32::from(b'a')));
    w.expect_quiet("physical 'a'");

    // Repeated: again model (quiet), then physical (abort).
    model_escape();
    w.expect_quiet("model Escape again");
    x.device_key(kbd, esc);
    w.expect_abort("second physical Escape");

    // syntheticSafe: suspend/resume are acked and do not change anything.
    w.send("suspend");
    assert_eq!(
        w.next(WAIT).as_deref(),
        Some(r#"{"event":"ack","cmd":"suspend"}"#)
    );
    w.send("resume");
    assert_eq!(
        w.next(WAIT).as_deref(),
        Some(r#"{"event":"ack","cmd":"resume"}"#)
    );
    x.device_key(kbd, esc);
    w.expect_abort("physical Escape after resume");

    assert_eq!(w.finish(), Some(0), "stdin EOF exits 0");
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn stdin_eof_exits_zero() {
    let w = Watcher::start();
    assert_eq!(w.finish(), Some(0));
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn raw_events_survive_another_clients_grabs() {
    let x = X::connect();
    let esc = escape_keycode(&x);
    let kbd = x.real_keyboard();
    let mut w = Watcher::start();

    // Another client: a passive grab on Escape on the root window (how an
    // app "owns" Escape globally) ...
    let grabber = X::connect();
    grabber
        .conn
        .grab_key(
            false,
            grabber.root,
            ModMask::ANY,
            esc,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        )
        .unwrap()
        .check()
        .expect("grab Escape on root");
    grabber.sync();

    x.device_key(kbd, esc);
    x.sync();
    w.expect_abort("physical Escape under a root key grab");
    // The grab really took the key (proves the grab was active).
    let deadline = Instant::now() + WAIT;
    let mut grabbed = false;
    while Instant::now() < deadline && !grabbed {
        while let Ok(Some(ev)) = grabber.conn.poll_for_event() {
            if matches!(ev, Event::KeyPress(ref k) if k.detail == esc) {
                grabbed = true;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(grabbed, "the grabbing client did not receive the Escape");

    // ... and a full active keyboard grab.
    let status = grabber
        .conn
        .grab_keyboard(false, grabber.root, 0u32, GrabMode::ASYNC, GrabMode::ASYNC)
        .unwrap()
        .reply()
        .unwrap()
        .status;
    assert_eq!(status, GrabStatus::SUCCESS);
    x.device_key(kbd, esc);
    x.sync();
    w.expect_abort("physical Escape under an active keyboard grab");
    model_escape();
    w.expect_quiet("model Escape under an active keyboard grab");

    grabber.conn.ungrab_keyboard(0u32).unwrap();
    grabber.sync();
    assert_eq!(w.finish(), Some(0));
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn new_master_xtest_device_is_ignored() {
    let x = X::connect();
    let esc = escape_keycode(&x);
    let mut w = Watcher::start();

    // A second master brings its own XTEST slaves; the watcher must pick
    // them up from the hierarchy event.
    let name = b"cuabort".to_vec();
    let len = (8 + name.len().div_ceil(4) * 4) / 4;
    x.conn
        .xinput_xi_change_hierarchy(&[HierarchyChange {
            len: len as u16,
            data: HierarchyChangeData::AddMaster(HierarchyChangeDataAddMaster {
                send_core: true,
                enable: true,
                name,
            }),
        }])
        .unwrap()
        .check()
        .expect("add master");
    x.sync();
    let xtest_kbd = x
        .device_id("cuabort XTEST keyboard")
        .expect("new master's XTEST keyboard");
    let master_kbd = x
        .device_id("cuabort keyboard")
        .expect("new master keyboard");

    x.device_key(xtest_kbd, esc);
    x.sync();
    w.expect_quiet("Escape from the new master's XTEST keyboard");

    // Still alive and reporting real keys.
    x.device_key(x.real_keyboard(), esc);
    w.expect_abort("physical Escape after the hierarchy change");

    x.conn
        .xinput_xi_change_hierarchy(&[HierarchyChange {
            len: 3,
            data: HierarchyChangeData::RemoveMaster(HierarchyChangeDataRemoveMaster {
                deviceid: master_kbd,
                return_mode: ChangeMode::FLOAT,
                return_pointer: 0,
                return_keyboard: 0,
            }),
        }])
        .unwrap()
        .check()
        .expect("remove master");
    x.sync();
    x.device_key(x.real_keyboard(), esc);
    w.expect_abort("physical Escape after removing the master");
    assert_eq!(w.finish(), Some(0));
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn doctor_reports_abort_watch_available() {
    let out = Command::new(BIN).arg("doctor").output().unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["abortWatch"],
        serde_json::json!({"available": true, "mechanism": "xi2", "reason": null})
    );
}

#[test]
#[ignore = "requires a throwaway X server; run under scripts/test-xvfb.sh"]
fn keymap_change_moves_escape() {
    let x = X::connect();
    let kbd = x.real_keyboard();
    let esc = escape_keycode(&x);
    let a = x.keycode_for(u32::from(b'a'));
    let setup = x.conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let before = x
        .conn
        .get_keyboard_mapping(min, max - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let per = before.keysyms_per_keycode;
    let row = |kc: u8| {
        let i = usize::from(kc - min) * usize::from(per);
        before.keysyms[i..i + usize::from(per)].to_vec()
    };
    let (esc_row, a_row) = (row(esc), row(a));
    let mut w = Watcher::start();

    // Swap: the 'a' key now produces Escape, the Escape key produces 'a'.
    x.conn
        .change_keyboard_mapping(1, a, per, &esc_row)
        .unwrap()
        .check()
        .unwrap();
    x.conn
        .change_keyboard_mapping(1, esc, per, &a_row)
        .unwrap()
        .check()
        .unwrap();
    x.sync();
    std::thread::sleep(Duration::from_millis(200));

    x.device_key(kbd, esc);
    w.expect_quiet("old Escape keycode after the swap");
    x.device_key(kbd, a);
    w.expect_abort("new Escape keycode after the swap");

    // Restore.
    x.conn
        .change_keyboard_mapping(1, a, per, &a_row)
        .unwrap()
        .check()
        .unwrap();
    x.conn
        .change_keyboard_mapping(1, esc, per, &esc_row)
        .unwrap()
        .check()
        .unwrap();
    x.sync();
    std::thread::sleep(Duration::from_millis(200));
    x.device_key(kbd, esc);
    w.expect_abort("Escape after restoring the keymap");
    assert_eq!(w.finish(), Some(0));
}

/// A private X server just for this test, so it can be killed.
struct OwnXvfb(Child, String);

impl OwnXvfb {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-screen",
                "0",
                "640x480x24",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let display = format!(":{}", line.trim());
        assert!(display.len() > 1, "Xvfb did not report a display");
        Self(child, display)
    }
}

impl Drop for OwnXvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires Xvfb; run under scripts/test-xvfb.sh"]
fn x_server_loss_is_a_failure() {
    let mut xvfb = OwnXvfb::start();
    let mut child = Command::new(BIN)
        .arg("abort-watch")
        .env("DISPLAY", &xvfb.1)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.contains(r#""event":"ready""#), "{line}");

    let _ = xvfb.0.kill();
    let _ = xvfb.0.wait();

    line.clear();
    out.read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["event"], "error", "{line}");
    assert!(
        v["message"].as_str().unwrap().contains("X connection lost"),
        "{line}"
    );
    let deadline = Instant::now() + WAIT;
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code();
        }
        assert!(Instant::now() < deadline, "watcher did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(code, Some(1));
}
