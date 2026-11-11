//! `app-under-point` against a reparenting window manager (xfwm4).
//!
//! `#[ignore]`d: each test starts its own private Xvfb (`-displayfd`) and an
//! xfwm4 on it, maps windows there and asks the built binary what is under
//! given points. Run them only under `scripts/test-xvfb.sh`, never against a
//! real session. xfwm4 must be installed.
//!
//! The rows: a point on a decorated window's title bar, on its border and in
//! its client area resolve to the client's app; the invisible shadow margin of
//! a client-side-decorated window (`_GTK_FRAME_EXTENTS`) resolves to what is
//! below it; the empty desktop is no app (`null`); a dock resolves to the dock
//! client (docks are only reported when no app window is at the point).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use x11rb::connection::Connection;
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClipOrdering, ConnectionExt as _, CreateWindowAux, PropMode, Rectangle, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const BUILT_BIN: &str = env!("CARGO_BIN_EXE_x11-bridge");

/// The binary under test: this build, or `X11_BRIDGE_TEST_BIN` (to run the
/// suite against another build, e.g. a release binary or an older version).
fn bin() -> String {
    std::env::var("X11_BRIDGE_TEST_BIN").unwrap_or_else(|_| BUILT_BIN.to_owned())
}
const WAIT: Duration = Duration::from_secs(5);

/// A private X server and window manager just for one test; both are killed
/// on drop.
struct Session {
    xvfb: Child,
    wm: Option<Child>,
    bus: Option<Child>,
    config: Option<std::path::PathBuf>,
    display: String,
    conn: RustConnection,
    root: Window,
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
                "1280x800x24",
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
        Self {
            xvfb,
            wm: None,
            bus: None,
            config: None,
            display,
            conn,
            root,
        }
    }

    /// Start xfwm4 on this display and wait until it manages the screen.
    ///
    /// xfwm4 refuses to run without xfconf, so it gets a private session bus
    /// (its own `dbus-daemon`, which activates a private xfconfd) and an empty
    /// `XDG_CONFIG_HOME`: the desktop session's bus and settings are never
    /// touched, and the WM runs on its defaults (decorated windows). The
    /// config dir lives under cargo's target tmpdir (xfconfd may still write
    /// to it after the bus is gone) and is emptied before each start.
    fn start_xfwm4(&mut self) {
        let config = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "wm-hit-xfconf-{}",
            self.display.trim_start_matches(':')
        ));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .env("XDG_CONFIG_HOME", &config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dbus-daemon");
        let mut address = String::new();
        let mut bus_out = BufReader::new(bus.stdout.take().unwrap());
        bus_out.read_line(&mut address).unwrap();
        // Keep draining: services the bus activates (xfconfd) inherit this
        // pipe, and a closed one kills them with SIGPIPE on their first write.
        std::thread::spawn(move || std::io::copy(&mut bus_out, &mut std::io::sink()));
        let address = address.trim().to_owned();
        assert!(!address.is_empty(), "dbus-daemon did not report an address");
        self.bus = Some(bus);
        self.config = Some(config.clone());

        let wm_log = config.with_extension("xfwm4.log");
        let wm = Command::new("xfwm4")
            .args(["--compositor=off", "--sm-client-disable"])
            .env("DISPLAY", &self.display)
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("XDG_CONFIG_HOME", &config)
            .env("NO_AT_BRIDGE", "1")
            .env("GTK_A11Y", "none")
            .env("GIO_USE_VFS", "local")
            .env("GSETTINGS_BACKEND", "memory")
            .env_remove("SESSION_MANAGER")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&wm_log).unwrap())
            .spawn()
            .expect("spawn xfwm4 (is it installed?)");
        self.wm = Some(wm);
        let check = self.atom("_NET_SUPPORTING_WM_CHECK");
        let deadline = Instant::now() + WAIT;
        loop {
            let reply = self
                .conn
                .get_property(false, self.root, check, AtomEnum::WINDOW, 0, 1)
                .unwrap()
                .reply()
                .unwrap();
            if reply.value32().and_then(|mut v| v.next()).is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "xfwm4 did not start on {} (exit: {:?}): {}",
                self.display,
                self.wm.as_mut().and_then(|wm| wm.try_wait().ok().flatten()),
                std::fs::read_to_string(&wm_log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Let it finish its startup scan.
        std::thread::sleep(Duration::from_millis(300));
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn
            .intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    }

    fn set32(&self, window: Window, prop: &str, kind: impl Into<u32>, values: &[u32]) {
        let prop = self.atom(prop);
        self.conn
            .change_property32(PropMode::REPLACE, window, prop, kind.into(), values)
            .unwrap();
    }

    /// Create and map a top-level window with a WM_CLASS, a title and a
    /// user-specified position (so the WM places it there).
    fn window(&self, class: &str, title: &str, rect: (i16, i16, u16, u16)) -> Window {
        self.window_with(class, title, rect, |_| {})
    }

    fn window_with(
        &self,
        class: &str,
        title: &str,
        (x, y, w, h): (i16, i16, u16, u16),
        extra: impl FnOnce(Window),
    ) -> Window {
        let id = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                self.root,
                x,
                y,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().background_pixel(0x0080_8080),
            )
            .unwrap();
        let wm_class = format!("{}\0{class}\0", class.to_lowercase());
        self.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                wm_class.as_bytes(),
            )
            .unwrap();
        let utf8 = self.atom("UTF8_STRING");
        let net_wm_name = self.atom("_NET_WM_NAME");
        self.conn
            .change_property8(PropMode::REPLACE, id, net_wm_name, utf8, title.as_bytes())
            .unwrap();
        let hints = WmSizeHints {
            position: Some((
                WmSizeHintsSpecification::UserSpecified,
                i32::from(x),
                i32::from(y),
            )),
            size: Some((
                WmSizeHintsSpecification::UserSpecified,
                i32::from(w),
                i32::from(h),
            )),
            ..WmSizeHints::default()
        };
        hints
            .set_normal_hints(&self.conn, id)
            .unwrap()
            .check()
            .unwrap();
        extra(id);
        self.conn.map_window(id).unwrap();
        self.conn.flush().unwrap();
        self.wait_managed(id);
        id
    }

    /// Wait until the WM has set WM_STATE on `client` (it manages it).
    fn wait_managed(&self, client: Window) {
        let wm_state = self.atom("WM_STATE");
        let deadline = Instant::now() + WAIT;
        loop {
            let reply = self
                .conn
                .get_property(false, client, wm_state, AtomEnum::ANY, 0, 2)
                .unwrap()
                .reply()
                .unwrap();
            if !reply.value.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "window {client} was not managed");
            std::thread::sleep(Duration::from_millis(30));
        }
        // Placement and decoration settle right after.
        std::thread::sleep(Duration::from_millis(200));
    }

    /// Root-relative (x, y, width, height) of a window, border excluded.
    fn root_rect(&self, window: Window) -> (i32, i32, i32, i32) {
        let geometry = self.conn.get_geometry(window).unwrap().reply().unwrap();
        let origin = self
            .conn
            .translate_coordinates(window, self.root, 0, 0)
            .unwrap()
            .reply()
            .unwrap();
        (
            i32::from(origin.dst_x),
            i32::from(origin.dst_y),
            i32::from(geometry.width),
            i32::from(geometry.height),
        )
    }

    /// The root child holding `client` (the WM frame, or the client itself).
    fn frame_of(&self, client: Window) -> Window {
        let mut window = client;
        loop {
            let tree = self.conn.query_tree(window).unwrap().reply().unwrap();
            if tree.parent == self.root || tree.parent == x11rb::NONE {
                return window;
            }
            window = tree.parent;
        }
    }

    /// Run `app-under-point` on this display: (exit code, stdout as JSON or
    /// `Value::String("<exit N: stderr>")` when it failed).
    fn under(&self, x: i32, y: i32) -> Value {
        let out = Command::new(bin())
            .args([
                "app-under-point",
                "--x",
                &x.to_string(),
                "--y",
                &y.to_string(),
            ])
            .env("DISPLAY", &self.display)
            .output()
            .expect("run x11-bridge");
        if !out.status.success() {
            return Value::String(format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        serde_json::from_slice(&out.stdout).expect("app-under-point printed JSON")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The WM first, then the X server, then the bus (its xfconfd exits
        // with the bus).
        for child in [self.wm.as_mut(), Some(&mut self.xvfb), self.bus.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(config) = self.config.take() {
            let _ = std::fs::remove_dir_all(config);
        }
    }
}

fn app(bundle_id: &str, display_name: &str) -> Value {
    json!({ "bundleId": bundle_id, "displayName": display_name })
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn decorated_window_frame_resolves_to_its_client() {
    let mut s = Session::start();
    s.start_xfwm4();
    let client = s.window("FrameTest", "Frame test", (200, 150, 400, 300));

    let frame = s.frame_of(client);
    assert_ne!(frame, client, "xfwm4 did not reparent the window");
    let (fx, fy, fw, fh) = s.root_rect(frame);
    let (cx, cy, cw, ch) = s.root_rect(client);
    let title_h = cy - fy;
    let left = cx - fx;
    let bottom = (fy + fh) - (cy + ch);
    assert!(
        title_h > 4,
        "no title bar: frame {fx},{fy} {fw}x{fh}, client {cx},{cy}"
    );
    assert!(
        left > 0 || bottom > 0,
        "no border: frame {fx},{fy} {fw}x{fh}, client {cx},{cy} {cw}x{ch}"
    );

    let expected = app("FrameTest", "Frame test");
    // Title bar, middle (not on a button).
    assert_eq!(
        s.under(cx + cw / 2, fy + title_h / 2),
        expected,
        "title bar"
    );
    // Title bar, near the right end (the window buttons).
    assert_eq!(
        s.under(fx + fw - 8, fy + title_h / 2),
        expected,
        "title bar buttons"
    );
    // A border.
    let border = if left > 0 {
        (fx + left / 2, cy + ch / 2)
    } else {
        (cx + cw / 2, cy + ch + bottom / 2)
    };
    assert_eq!(s.under(border.0, border.1), expected, "border {border:?}");
    // Client area.
    assert_eq!(s.under(cx + cw / 2, cy + ch / 2), expected, "client area");
    // Empty desktop.
    assert_eq!(s.under(1100, 700), Value::Null, "empty desktop");
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn claude_title_bar_resolves_to_claude() {
    let mut s = Session::start();
    s.start_xfwm4();
    let client = s.window("com.anthropic.Claude", "Claude", (100, 100, 500, 400));
    let frame = s.frame_of(client);
    let (_, fy, _, _) = s.root_rect(frame);
    let (cx, cy, cw, _) = s.root_rect(client);
    assert!(cy > fy, "no title bar");
    assert_eq!(
        s.under(cx + cw / 2, fy + (cy - fy) / 2),
        app("com.anthropic.Claude", "Claude")
    );
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn csd_shadow_margin_is_not_the_window() {
    let mut s = Session::start();
    s.start_xfwm4();
    // Something below the CSD window's shadow, to prove the point falls
    // through to it.
    let below = s.window("BelowTest", "Below", (500, 100, 300, 300));
    let below_frame = s.frame_of(below);
    let (bx, by, bw, bh) = s.root_rect(below_frame);

    let motif = s.atom("_MOTIF_WM_HINTS");
    let csd = s.window_with("CsdTest", "CSD test", (100, 450, 300, 200), |id| {
        // Undecorated (the client draws its own), with a 20 px shadow margin.
        s.set32(id, "_MOTIF_WM_HINTS", motif, &[2, 0, 0, 0, 0]);
        s.set32(
            id,
            "_GTK_FRAME_EXTENTS",
            AtomEnum::CARDINAL,
            &[20, 20, 20, 20],
        );
    });
    let (cx, cy, cw, ch) = s.root_rect(csd);

    let expected = app("CsdTest", "CSD test");
    // Inside the visible window.
    assert_eq!(s.under(cx + cw / 2, cy + ch / 2), expected, "visible area");
    assert_eq!(s.under(cx + 21, cy + 21), expected, "visible corner");
    // The shadow margin over the empty desktop.
    assert_eq!(s.under(cx + 5, cy + ch / 2), Value::Null, "left shadow");
    assert_eq!(
        s.under(cx + cw / 2, cy + ch - 5),
        Value::Null,
        "bottom shadow"
    );

    // Move the CSD window so its top-right shadow overlaps `below`.
    let rx = bx - cw + 10; // the right 10 px of the CSD rect cover `below`
    let ry = by + bh / 2;
    s.conn
        .configure_window(
            csd,
            &x11rb::protocol::xproto::ConfigureWindowAux::new()
                .x(rx)
                .y(ry),
        )
        .unwrap();
    s.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let (cx, cy, cw, ch) = s.root_rect(csd);
    assert!(
        cx + cw > bx && cx + cw - 20 <= bx,
        "CSD shadow does not overlap: csd {cx},{cy} {cw}x{ch}, below {bx},{by} {bw}x{bh}"
    );
    // In the right shadow margin, over `below`.
    assert_eq!(
        s.under(cx + cw - 3, cy + ch / 2),
        app("BelowTest", "Below"),
        "shadow over another window"
    );
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn dock_resolves_to_the_dock_client() {
    let mut s = Session::start();
    s.start_xfwm4();
    let type_atom = s.atom("_NET_WM_WINDOW_TYPE");
    let dock_type = s.atom("_NET_WM_WINDOW_TYPE_DOCK");
    let dock = s.window_with("DockTest", "Dock", (0, 760, 1280, 40), |id| {
        s.conn
            .change_property32(
                PropMode::REPLACE,
                id,
                type_atom,
                AtomEnum::ATOM,
                &[dock_type],
            )
            .unwrap();
    });
    let (dx, dy, dw, dh) = s.root_rect(dock);
    assert_eq!(
        s.under(dx + dw / 2, dy + dh / 2),
        app("DockTest", "Dock"),
        "dock"
    );
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn window_without_app_id_answers_its_client_id() {
    let mut s = Session::start();
    s.start_xfwm4();
    // A managed window with no WM_CLASS and no desktop file: the answer is
    // the CLIENT's id (what `windows` lists), never a frame or decoration
    // window's id, and never null (null is the empty desktop).
    let id = s.conn.generate_id().unwrap();
    s.conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            s.root,
            300,
            300,
            200,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap();
    s.conn.map_window(id).unwrap();
    s.conn.flush().unwrap();
    s.wait_managed(id);
    let frame = s.frame_of(id);
    let (_, fy, _, _) = s.root_rect(frame);
    let (cx, cy, cw, ch) = s.root_rect(id);
    let own = app(&id.to_string(), &id.to_string());
    assert_ne!(s.frame_of(id), id, "xfwm4 did not reparent the window");
    assert_eq!(s.under(cx + cw / 2, cy + ch / 2), own, "client area");
    assert_eq!(s.under(cx + cw / 2, fy + (cy - fy) / 2), own, "title bar");
    // The id is the one `windows` lists for it.
    let out = Command::new(bin())
        .arg("windows")
        .env("DISPLAY", &s.display)
        .output()
        .unwrap();
    let windows: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        windows
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"].as_str() == Some(id.to_string().as_str())),
        "{windows}"
    );
}

/// A full-screen override-redirect window like Claude's glow: WM_CLASS
/// com.anthropic.Claude, mapped above everything, with the given input shape
/// (`Some(&[])` = empty; real Electron 45 with `setIgnoreMouseEvents(true)`
/// on X11 sets a 1x1 input rectangle at the origin).
fn glow(s: &Session, input: Option<&[Rectangle]>) -> Window {
    let id = s.conn.generate_id().unwrap();
    s.conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            s.root,
            0,
            0,
            1280,
            800,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .unwrap();
    s.conn
        .change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"com.anthropic.Claude\0com.anthropic.Claude\0",
        )
        .unwrap();
    if let Some(rects) = input {
        s.conn
            .shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                ClipOrdering::UNSORTED,
                id,
                0,
                0,
                rects,
            )
            .unwrap();
    }
    s.conn.map_window(id).unwrap();
    s.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    id
}

/// A dock along the bottom and an app window; returns (dock point, app
/// point, empty desktop point).
fn dock_and_app(s: &Session) -> ((i32, i32), (i32, i32), (i32, i32)) {
    let type_atom = s.atom("_NET_WM_WINDOW_TYPE");
    let dock_type = s.atom("_NET_WM_WINDOW_TYPE_DOCK");
    let dock = s.window_with("DockTest", "Dock", (0, 760, 1280, 40), |id| {
        s.conn
            .change_property32(
                PropMode::REPLACE,
                id,
                type_atom,
                AtomEnum::ATOM,
                &[dock_type],
            )
            .unwrap();
    });
    let client = s.window("AppTest", "App", (200, 150, 400, 300));
    let (dx, dy, dw, dh) = s.root_rect(dock);
    let (cx, cy, cw, ch) = s.root_rect(client);
    (
        (dx + dw / 2, dy + dh / 2),
        (cx + cw / 2, cy + ch / 2),
        (1000, 500),
    )
}

fn glow_is_looked_through(input: &[Rectangle]) {
    let mut s = Session::start();
    s.start_xfwm4();
    let (dock, client, desktop) = dock_and_app(&s);
    let glow = glow(&s, Some(input));
    let order = s.conn.query_tree(s.root).unwrap().reply().unwrap().children;
    assert_eq!(order.last(), Some(&glow), "the glow is not on top");
    assert_eq!(s.under(dock.0, dock.1), app("DockTest", "Dock"), "dock");
    assert_eq!(s.under(client.0, client.1), app("AppTest", "App"), "app");
    assert_eq!(s.under(desktop.0, desktop.1), Value::Null, "desktop");
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn glow_with_empty_input_shape_is_looked_through() {
    glow_is_looked_through(&[]);
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn glow_with_1x1_input_shape_is_looked_through() {
    glow_is_looked_through(&[Rectangle {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    }]);
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn override_redirect_window_taking_input_still_answers_off_app_windows() {
    // Control: the same window WITHOUT an input shape takes clicks everywhere.
    // Over an app window the app still answers (popups never won over a
    // managed window); over the dock and the desktop the overlay does.
    let mut s = Session::start();
    s.start_xfwm4();
    let (dock, client, desktop) = dock_and_app(&s);
    glow(&s, None);
    let claude = app("com.anthropic.Claude", "com.anthropic.Claude");
    assert_eq!(s.under(client.0, client.1), app("AppTest", "App"), "app");
    assert_eq!(s.under(dock.0, dock.1), claude, "dock");
    assert_eq!(s.under(desktop.0, desktop.1), claude, "desktop");
}

#[test]
#[ignore = "requires Xvfb; run under scripts/test-xvfb.sh"]
fn doctor_reports_input_shape() {
    let s = Session::start();
    let out = Command::new(bin())
        .arg("doctor")
        .env("DISPLAY", &s.display)
        .output()
        .unwrap();
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doctor["capabilities"]["input_shape"], true, "{doctor}");
}

/// Bare X (no WM): a fake reparenting "WM" frame with the client four levels
/// down (deeper than the WM_STATE search), listed in `_NET_CLIENT_LIST`, over
/// another window. Plus a WM-internal root child (nothing listed under it).
#[test]
#[ignore = "requires Xvfb; run under scripts/test-xvfb.sh"]
fn deep_client_found_through_the_client_list() {
    let s = Session::start();
    let make = |parent: Window, rect: (i16, i16, u16, u16)| {
        let id = s.conn.generate_id().unwrap();
        s.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                parent,
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        s.conn.map_window(id).unwrap();
        id
    };
    let class = |id: Window, class: &[u8]| {
        s.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                class,
            )
            .unwrap();
    };
    let wm_state = s.atom("WM_STATE");
    // Below: a plain classed window covering the area.
    let below = make(s.root, (0, 0, 600, 400));
    class(below, b"below\0BelowTest\0");
    s.set32(below, "WM_STATE", wm_state, &[1, 0]);
    // The deep frame: root child -> l1 -> l2 -> l3 -> client (level 4).
    let frame = make(s.root, (100, 100, 300, 200));
    let l1 = make(frame, (0, 0, 300, 200));
    let l2 = make(l1, (0, 0, 300, 200));
    let l3 = make(l2, (0, 0, 300, 200));
    let client = make(l3, (0, 20, 300, 180));
    class(client, b"deep\0DeepTest\0");
    s.set32(client, "WM_STATE", wm_state, &[1, 0]);
    // A WM-internal root child: no WM_STATE, no WM_CLASS, nothing listed.
    let internal = make(s.root, (450, 250, 100, 100));
    s.set32(
        s.root,
        "_NET_CLIENT_LIST",
        AtomEnum::WINDOW,
        &[below, client],
    );
    s.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let _ = internal;

    let deep = app("DeepTest", "DeepTest");
    assert_eq!(s.under(200, 105), deep, "frame strip above the client");
    assert_eq!(s.under(200, 200), deep, "client area");
    assert_eq!(
        s.under(500, 300),
        app("BelowTest", "BelowTest"),
        "WM-internal window"
    );
    assert_eq!(s.under(50, 50), app("BelowTest", "BelowTest"), "below only");
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn topmost_window_title_bar_over_another_client_wins() {
    let mut s = Session::start();
    s.start_xfwm4();
    let below = s.window("BelowTest", "Below", (100, 100, 400, 300));
    let above = s.window("AboveTest", "Above", (150, 200, 400, 300));
    let order = s.conn.query_tree(s.root).unwrap().reply().unwrap().children;
    let pos = |w: Window| order.iter().position(|&c| c == s.frame_of(w)).unwrap();
    assert!(pos(above) > pos(below), "the later window is not on top");

    let (bx, by, bw, bh) = s.root_rect(below);
    let (ax, ay, _, _) = s.root_rect(above);
    let (_, afy, _, _) = s.root_rect(s.frame_of(above));
    // `above`'s title bar lies over `below`'s client area.
    let title = (ax + 100, afy + (ay - afy) / 2);
    assert!(
        title.0 < bx + bw && title.1 > by && title.1 < by + bh,
        "the title bar does not overlap the lower client"
    );
    assert_eq!(
        s.under(title.0, title.1),
        app("AboveTest", "Above"),
        "title bar over a client"
    );
    // The overlap of both client areas, and `below`'s uncovered part.
    assert_eq!(
        s.under(ax + 50, ay + 50),
        app("AboveTest", "Above"),
        "overlap"
    );
    assert_eq!(
        s.under(bx + 20, by + 20),
        app("BelowTest", "Below"),
        "lower window"
    );
}

#[test]
#[ignore = "requires Xvfb and xfwm4; run under scripts/test-xvfb.sh"]
fn override_redirect_popup_answers_only_off_app_windows() {
    let mut s = Session::start();
    s.start_xfwm4();
    let client = s.window("MenuOwner", "Owner", (100, 100, 300, 200));
    let (cx, cy, cw, _) = s.root_rect(client);
    // A popup menu straddling the window's right edge onto the desktop.
    let popup = s.conn.generate_id().unwrap();
    s.conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            popup,
            s.root,
            i16::try_from(cx + cw - 50).unwrap(),
            i16::try_from(cy + 20).unwrap(),
            150,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .unwrap();
    s.conn
        .change_property8(
            PropMode::REPLACE,
            popup,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"popupowner\0PopupOwner\0",
        )
        .unwrap();
    s.conn.map_window(popup).unwrap();
    s.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    // Over the app window: the app window (unchanged: popups never won over
    // a managed window).
    assert_eq!(s.under(cx + cw - 20, cy + 50), app("MenuOwner", "Owner"));
    // Over the desktop: the popup's own app.
    assert_eq!(
        s.under(cx + cw + 50, cy + 50),
        app("PopupOwner", "PopupOwner")
    );
}

#[test]
#[ignore = "requires Xvfb; run under scripts/test-xvfb.sh"]
fn without_a_window_manager_the_classed_window_answers() {
    // Bare X: nobody sets WM_STATE, so the WM_CLASS heuristic applies.
    let s = Session::start();
    let id = s.conn.generate_id().unwrap();
    s.conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            s.root,
            50,
            50,
            200,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new(),
        )
        .unwrap();
    s.conn
        .change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"barex\0BareX\0",
        )
        .unwrap();
    s.conn.map_window(id).unwrap();
    s.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(s.under(100, 100), app("BareX", "BareX"));
    assert_eq!(s.under(600, 600), Value::Null);
}
