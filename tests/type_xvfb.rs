//! `type`, `key-sequence` and `hold-key` as Claude Desktop runs them: the
//! built binary, spawned per command, against a private Xvfb.
//!
//! `#[ignore]`d: each test starts its own Xvfb (`-displayfd`) and keeps a
//! holder connection open for its whole run (Xvfb resets its keymap when the
//! last client disconnects, which would hide a keycode left bound). Run under
//! `scripts/test-xvfb.sh`. Needs `Xvfb`, `setxkbmap` and, for the Caps Lock
//! test, `xterm` or `urxvt` (an independent Xlib text client:
//! XLookupString's Caps Lock rules, not this crate's).
//!
//! `X11_BRIDGE_TEST_BIN` runs the tests against another build (to show a test
//! fails on old code).

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, KeyButMask, Keycode,
    Keysym, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

fn bin() -> String {
    std::env::var("X11_BRIDGE_TEST_BIN")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_x11-bridge").into())
}

const LOCK: u8 = 0x02;
const MOD2: u8 = 0x10;

struct Session {
    xvfb: Child,
    display: String,
    conn: RustConnection,
    root: Window,
    children: Vec<Child>,
}

impl Session {
    fn start() -> Self {
        let mut xvfb = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-nolisten",
                "tcp",
                "-screen",
                "0",
                "1024x768x24",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb");
        let mut line = String::new();
        BufReader::new(xvfb.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let display = format!(":{}", line.trim());
        assert!(display.len() > 1, "Xvfb did not report a display");
        let (conn, screen) = RustConnection::connect(Some(&display)).expect("connect to Xvfb");
        let root = conn.setup().roots[screen].root;
        conn.xkb_use_extension(1, 0).unwrap().reply().unwrap();
        Self {
            xvfb,
            display,
            conn,
            root,
            children: Vec::new(),
        }
    }

    /// Load a layout and lock `group` and the modifiers `locked`
    /// (Caps Lock, Num Lock).
    fn layout(&self, layout: &str, group: u8, locked: u8) {
        let ok = Command::new("setxkbmap")
            .args(["-display", &self.display, "-layout", layout])
            .status()
            .expect("run setxkbmap (x11-xkb-utils)")
            .success();
        assert!(ok, "setxkbmap -layout {layout}");
        let group = match group {
            0 => xkb::Group::M1,
            1 => xkb::Group::M2,
            2 => xkb::Group::M3,
            _ => xkb::Group::M4,
        };
        self.conn
            .xkb_latch_lock_state(
                xkb::ID::USE_CORE_KBD.into(),
                (LOCK | MOD2).into(),
                locked.into(),
                true,
                group,
                0u8.into(),
                false,
                0,
            )
            .unwrap();
        self.sync();
        std::thread::sleep(Duration::from_millis(100));
    }

    fn sync(&self) {
        self.conn.get_input_focus().unwrap().reply().unwrap();
    }

    fn bridge(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args).env("DISPLAY", &self.display);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.bridge(args).output().expect("run x11-bridge")
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.bridge(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn x11-bridge")
    }

    fn keymap(&self) -> Vec<Keysym> {
        let s = self.conn.setup();
        self.conn
            .get_keyboard_mapping(s.min_keycode, s.max_keycode - s.min_keycode + 1)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms
    }

    fn per(&self) -> usize {
        let s = self.conn.setup();
        usize::from(
            self.conn
                .get_keyboard_mapping(s.min_keycode, 1)
                .unwrap()
                .reply()
                .unwrap()
                .keysyms_per_keycode,
        )
    }

    /// Keycodes without any keysym.
    fn spares(&self) -> Vec<Keycode> {
        let per = self.per();
        let min = self.conn.setup().min_keycode;
        self.keymap()
            .chunks(per)
            .enumerate()
            .filter(|(_, row)| row.iter().all(|&k| k == 0))
            .map(|(i, _)| min + i as u8)
            .collect()
    }

    fn journal(&self) -> bool {
        let atom = self
            .conn
            .intern_atom(false, b"_X11_BRIDGE_KEYMAP_RESTORE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let r = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::ANY, 0, 16)
            .unwrap()
            .reply()
            .unwrap();
        r.value_len > 0
    }

    /// A mapped window at the top left with the input focus, selecting key
    /// events.
    fn focused_window(&self) -> Window {
        let screen = &self.conn.setup().roots[0];
        let win = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                screen.root_depth,
                win,
                self.root,
                0,
                0,
                300,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS | EventMask::KEY_RELEASE),
            )
            .unwrap();
        self.conn.map_window(win).unwrap();
        self.sync();
        std::thread::sleep(Duration::from_millis(50));
        self.conn
            .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
            .unwrap();
        self.sync();
        win
    }

    fn events(&self, for_how_long: Duration) -> Vec<Event> {
        let end = Instant::now() + for_how_long;
        let mut out = Vec::new();
        while Instant::now() < end {
            while let Ok(Some(e)) = self.conn.poll_for_event() {
                out.push(e);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }

    /// Start a terminal that writes every byte it receives to a file, with
    /// the pointer (and so the focus) on it. Returns the file.
    fn terminal_sink(&mut self) -> PathBuf {
        let file = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "type-xvfb-term-{}.txt",
            self.display.trim_start_matches(':')
        ));
        let _ = std::fs::remove_file(&file);
        let script = format!("stty -icanon -echo min 1; exec cat >> '{}'", file.display());
        let term = ["xterm", "urxvt"]
            .into_iter()
            .find(|t| {
                Command::new("sh")
                    .args(["-c", &format!("command -v {t}")])
                    .stdout(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            })
            .expect("xterm or urxvt is needed for the Caps Lock test");
        let mut cmd = Command::new(term);
        if term == "xterm" {
            cmd.arg("-u8");
        }
        let child = cmd
            .args(["-geometry", "80x24+0+0", "-e", "sh", "-c", &script])
            .env("DISPLAY", &self.display)
            .env("LC_ALL", "C.UTF-8")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn terminal");
        self.children.push(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !file.exists() {
            assert!(Instant::now() < deadline, "terminal did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(500));
        self.conn
            .xtest_fake_input(6, 0, x11rb::CURRENT_TIME, self.root, 200, 150, 0)
            .unwrap();
        self.sync();
        std::thread::sleep(Duration::from_millis(200));
        file
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = self.xvfb.kill();
        let _ = self.xvfb.wait();
    }
}

fn read_settled(file: &PathBuf, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last = String::new();
    let mut since = Instant::now();
    loop {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(file) {
            let _ = f.read_to_string(&mut s);
        }
        if s == want || Instant::now() > deadline {
            return s;
        }
        if s != last {
            last = s;
            since = Instant::now();
        } else if since.elapsed() > Duration::from_millis(1500) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the keymap differs from `from` (a bridge bound something).
fn wait_bound(s: &Session, from: &[Keysym]) {
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(5);
    while s.keymap() == from {
        assert!(Instant::now() < deadline, "the bridge bound nothing");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
}

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out).unwrap_or(Value::Null)
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("x11-bridge did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn kill(child: &Child, signal: &str) {
    Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
}

const CAPS_TEXT: &str = "éñaü1ÉÑAÜ ạẠ ßẞ жЖ αΑ µ @";

/// Caps Lock (and Num Lock) on: what an Xlib terminal shows is exactly the
/// text. Bound case pairs used to get a key type that ignores Lock, so the
/// terminal upper-cased them (`éñ` came out as `ÉÑ`), and Unicode pairs
/// (`ạ`) could not be typed small at all.
#[test]
#[ignore = "requires Xvfb, setxkbmap and xterm/urxvt; run under scripts/test-xvfb.sh"]
fn caps_lock_text_arrives_exactly_in_an_xlib_terminal() {
    let mut s = Session::start();
    let file = s.terminal_sink();
    for (layout, group, locked) in [
        ("us", 0, LOCK),
        ("us", 0, LOCK | MOD2),
        ("de", 0, LOCK),
        ("us,de", 1, LOCK),
        ("us", 0, 0),
    ] {
        s.layout(layout, group, locked);
        std::fs::write(&file, "").unwrap();
        let out = s.run(&["type", "--text", CAPS_TEXT, "--delay-ms", "12"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got = read_settled(&file, CAPS_TEXT);
        assert_eq!(got, CAPS_TEXT, "{layout} group {group} locked {locked:#x}");
    }
}

/// SIGTERM (the app's execFileSync timeout) mid-text: exit 1, the partial
/// result on stdout, every keycode restored, no journal left.
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn sigterm_reports_progress_and_restores() {
    let s = Session::start();
    s.layout("de", 0, 0);
    let spares = s.spares();
    let pristine = s.keymap();
    let text = "中文字ÄéX¢ạ".repeat(200);
    let mut child = s.spawn(&["type", "--text", &text, "--delay-ms", "5"]);
    wait_bound(&s, &pristine);
    assert!(
        s.spares().len() < spares.len(),
        "nothing was bound mid-text"
    );
    kill(&child, "-TERM");
    let code = wait_exit(&mut child, Duration::from_secs(5));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let v = json(&stdout);
    assert_eq!(code, Some(1));
    assert_eq!(v["interrupted"], Value::Bool(true), "stdout: {v}");
    assert_eq!(v["reason"], "signal");
    let n = v["charCount"].as_u64().unwrap();
    assert!(n > 0 && n < 1600, "charCount {n}");
    assert_eq!(s.spares(), spares);
    assert!(!s.journal());
}

/// SIGKILL mid-text leaves a binding; the next keyboard command restores it.
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn sigkill_leftovers_are_restored_by_the_next_command() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let pristine = s.keymap();
    let text = "éñü中".repeat(300);
    for _ in 0..3 {
        let mut child = s.spawn(&["type", "--text", &text, "--delay-ms", "5"]);
        wait_bound(&s, &pristine);
        kill(&child, "-KILL");
        let _ = child.wait();
        assert_ne!(s.keymap(), pristine, "SIGKILL should leave a binding");
        assert!(s.journal());
        let out = s.run(&["key-sequence", "--keys", "shift"]);
        assert!(out.status.success());
        assert_eq!(s.keymap(), pristine, "not restored");
        assert!(!s.journal());
    }
}

/// Two bridges at once: the second waits for the keymap lock; killed while
/// waiting it leaves nothing, and the first restores everything.
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn two_bridges_serialise_and_leave_nothing_bound() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let pristine = s.keymap();
    let long = "éñü中文".repeat(100);
    let mut a = s.spawn(&["type", "--text", &long, "--delay-ms", "5"]);
    wait_bound(&s, &pristine);
    // B starts while A has keycodes bound and is killed a moment later. The
    // old bridge bound its own keycodes at once and overwrote A's journal;
    // A then deleted B's journal and B's bindings stayed for good.
    let with_a = s.keymap();
    let mut b = s.spawn(&["type", "--text", &"ạßÅÆ".repeat(100), "--delay-ms", "5"]);
    std::thread::sleep(Duration::from_millis(400));
    // B waits for the lock: A's bindings are untouched (the old bridge
    // rebound A's keycodes under it, so A typed B's characters).
    assert_eq!(s.keymap(), with_a, "the second bridge changed the keymap");
    kill(&b, "-KILL");
    let _ = b.wait();
    assert_eq!(wait_exit(&mut a, Duration::from_secs(20)), Some(0));
    assert_eq!(s.keymap(), pristine, "a keycode stayed bound");
    assert!(!s.journal());
    // A bridge that starts while another types waits for the lock and then
    // types; both finish cleanly.
    let mut a = s.spawn(&["type", "--text", &long, "--delay-ms", "5"]);
    wait_bound(&s, &pristine);
    let mut c = s.spawn(&["type", "--text", "ął", "--delay-ms", "5"]);
    assert_eq!(wait_exit(&mut a, Duration::from_secs(20)), Some(0));
    assert_eq!(wait_exit(&mut c, Duration::from_secs(10)), Some(0));
    assert_eq!(s.keymap(), pristine, "a keycode stayed bound");
    assert!(!s.journal());
}

/// Ctrl+C with a Russian group active presses the key that is C in the Latin
/// group, without touching the keymap (it used to bind `c` to a spare
/// keycode: a keymap change and a key Chrome reports with an empty code).
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn ctrl_c_on_a_russian_layout_presses_the_latin_key() {
    let s = Session::start();
    s.layout("us,ru", 1, 0);
    let before = s.keymap();
    let _win = s.focused_window();
    let out = s.run(&["key-sequence", "--keys", "ctrl+c"]);
    assert!(out.status.success());
    let events = s.events(Duration::from_millis(400));
    let c = events.iter().find_map(|e| match e {
        Event::KeyPress(k) if k.state.contains(KeyButMask::CONTROL) => Some(k.detail),
        _ => None,
    });
    assert_eq!(c, Some(54), "Ctrl+C must press keycode 54 (the C key)");
    assert_eq!(s.keymap(), before, "the keymap changed");
}

/// A keymap change by someone else mid-text: typing stops with
/// `keymapChanged`, and the foreign row is not overwritten by the restore.
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn a_foreign_keymap_change_is_kept() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let spares = s.spares();
    let pristine = s.keymap();
    let text = "éñüạ中文".repeat(200);
    let mut child = s.spawn(&["type", "--text", &text, "--delay-ms", "5"]);
    wait_bound(&s, &pristine);
    // Someone binds F13 to the second spare keycode (one we use).
    let per = s.per();
    let mut row = vec![0; per];
    row[0] = 0xffca; // F13
    s.conn
        .change_keyboard_mapping(1, spares[1], per as u8, &row)
        .unwrap();
    s.sync();
    let code = wait_exit(&mut child, Duration::from_secs(10));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let v = json(&stdout);
    assert_eq!(code, Some(1), "stdout: {v}");
    assert_eq!(v["reason"], "keymapChanged");
    let now = s
        .conn
        .get_keyboard_mapping(spares[1], 1)
        .unwrap()
        .reply()
        .unwrap()
        .keysyms;
    assert_eq!(now[0], 0xffca, "the foreign binding was overwritten");
    assert!(!s.journal());
}

/// SIGTERM during `hold-key`: the keys are released at once and the bound
/// keycode restored (the default action used to kill the bridge with both
/// still in place).
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn sigterm_during_hold_key_releases_the_keys() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let pristine = s.keymap();
    let _win = s.focused_window();
    let mut child = s.spawn(&[
        "hold-key",
        "--key",
        "shift",
        "--key",
        "é",
        "--duration-ms",
        "5000",
    ]);
    wait_bound(&s, &pristine);
    std::thread::sleep(Duration::from_millis(100));
    kill(&child, "-TERM");
    let events = s.events(Duration::from_millis(800));
    let released = events
        .iter()
        .filter(|e| matches!(e, Event::KeyRelease(_)))
        .count();
    assert_eq!(released, 2, "both keys released right after SIGTERM");
    assert_eq!(wait_exit(&mut child, Duration::from_secs(2)), Some(1));
    assert_eq!(s.keymap(), pristine);
}

/// A burst at `--delay-ms 0`: the wait for a slow client's backlog is capped,
/// so 4000 characters finish well inside the app's 15 s timeout (it used to
/// wait 4 ms per key: about 16 s).
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn a_burst_finishes_inside_the_app_timeout() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let text = "aé中".repeat(1334);
    let t0 = Instant::now();
    let out = s.run(&["type", "--text", &text, "--delay-ms", "0"]);
    let took = t0.elapsed();
    assert!(out.status.success());
    assert_eq!(json(&out.stdout)["charCount"], 4002);
    assert!(took < Duration::from_secs(6), "took {took:?}");
}

/// `(` and `)` come from the main block (Shift+9, Shift+0), not from the
/// keypad's parenthesis keys (keycodes 187/188, which Chrome reports as
/// `NumpadParenLeft`).
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn parentheses_use_the_main_block() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let _win = s.focused_window();
    let out = s.run(&["type", "--text", "()", "--delay-ms", "5"]);
    assert!(out.status.success());
    let presses: Vec<Keycode> = s
        .events(Duration::from_millis(300))
        .iter()
        .filter_map(|e| match e {
            Event::KeyPress(k) if k.state.contains(KeyButMask::SHIFT) => Some(k.detail),
            _ => None,
        })
        .collect();
    assert_eq!(presses, vec![18, 19]);
}

/// A signal that arrives after the last character (during the pause before
/// the restore) is no failure: exit 0, full `charCount`, keymap restored.
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn a_signal_after_the_last_character_is_success() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let pristine = s.keymap();
    let mut child = s.spawn(&["type", "--text", "éñ", "--delay-ms", "0"]);
    // Both keys go out ~50 ms after the start; the restore waits 200 ms more.
    std::thread::sleep(Duration::from_millis(170));
    kill(&child, "-TERM");
    let code = wait_exit(&mut child, Duration::from_secs(3));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let v = json(&stdout);
    assert_eq!(code, Some(0), "stdout: {v}");
    assert_eq!(v["charCount"], 2);
    assert_eq!(s.keymap(), pristine);
}

/// After a SIGKILL, a layout loaded before the next command may put a real
/// key on a journaled keycode. Recovery restores only rows that are exactly
/// what the bridge wrote, so that key survives (one-level rows such as
/// `Henkan_Mode NoSymbol Henkan_Mode` used to pass as bridge rows).
#[test]
#[ignore = "requires Xvfb and setxkbmap; run under scripts/test-xvfb.sh"]
fn recovery_leaves_a_real_key_alone() {
    let s = Session::start();
    s.layout("us", 0, 0);
    let spares = s.spares();
    let pristine = s.keymap();
    let mut child = s.spawn(&["type", "--text", &"éñü".repeat(300), "--delay-ms", "5"]);
    wait_bound(&s, &pristine);
    kill(&child, "-KILL");
    let _ = child.wait();
    assert!(s.journal());
    // The "new layout": a one-level key on the first spare keycode.
    let per = s.per();
    let mut row = vec![0; per];
    row[0] = 0xff23; // Henkan_Mode
    if per > 2 {
        row[2] = 0xff23;
    }
    s.conn
        .change_keyboard_mapping(1, spares[0], per as u8, &row)
        .unwrap();
    s.sync();
    let out = s.run(&["key-sequence", "--keys", "shift"]);
    assert!(out.status.success());
    let now = s
        .conn
        .get_keyboard_mapping(spares[0], 1)
        .unwrap()
        .reply()
        .unwrap()
        .keysyms;
    assert_eq!(now[0], 0xff23, "recovery wiped a real key");
    // The other keycodes the dead bridge bound are restored.
    assert_eq!(s.spares().len(), spares.len() - 1);
    assert!(!s.journal());
}
