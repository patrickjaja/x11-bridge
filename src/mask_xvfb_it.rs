//! Mask-mode integration tests against a live X server.
//!
//! `#[ignore]`d like the input tests; run them under a throwaway Xvfb via
//! `scripts/test-xvfb.sh` (1280x800x24, no window manager). Each test builds its
//! own windows on its own connection (closing it destroys them), captures with
//! `capture_with_mask`, and checks raw RGB pixels before JPEG encoding.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::capture::{RgbImage, RootRect, capture_with_mask};
use crate::conn::Conn;
use crate::mask::{MASK_FILL, MaskReport, MaskRequest};

const RED: u32 = 0xFF0000;
const GREEN: u32 = 0x00FF00;
const BLUE: u32 = 0x0000FF;
const YELLOW: u32 = 0xFFFF00;
const GRAY: u32 = 0x808080;
const CYAN: u32 = 0x00FFFF;

fn rgb(color: u32) -> [u8; 3] {
    [(color >> 16) as u8, (color >> 8) as u8, color as u8]
}

fn atom(conn: &Conn, name: &str) -> u32 {
    conn.conn
        .intern_atom(false, name.as_bytes())
        .unwrap()
        .reply()
        .unwrap()
        .atom
}

struct Spec<'a> {
    parent: Option<Window>,
    rect: (i16, i16, u16, u16),
    color: u32,
    override_redirect: bool,
    class: Option<&'a str>,
    pid: Option<u32>,
    transient_for: Option<Window>,
    wm_state: bool,
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Self {
            parent: None,
            rect: (0, 0, 10, 10),
            color: 0,
            override_redirect: false,
            class: None,
            pid: None,
            transient_for: None,
            wm_state: false,
        }
    }
}

fn make(conn: &Conn, spec: Spec) -> Window {
    let screen = &conn.conn.setup().roots[conn.screen_num];
    let win = conn.conn.generate_id().unwrap();
    let (x, y, w, h) = spec.rect;
    conn.conn
        .create_window(
            screen.root_depth,
            win,
            spec.parent.unwrap_or(conn.root),
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .background_pixel(spec.color)
                .override_redirect(u32::from(spec.override_redirect)),
        )
        .unwrap();
    if let Some(class) = spec.class {
        let value = format!("{}\0{}\0", class.to_lowercase(), class);
        conn.conn
            .change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                value.as_bytes(),
            )
            .unwrap();
    }
    if let Some(pid) = spec.pid {
        let net_wm_pid = atom(conn, "_NET_WM_PID");
        conn.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                net_wm_pid,
                AtomEnum::CARDINAL,
                &[pid],
            )
            .unwrap();
    }
    if let Some(parent) = spec.transient_for {
        conn.conn
            .change_property32(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                &[parent],
            )
            .unwrap();
    }
    if spec.wm_state {
        let wm_state = atom(conn, "WM_STATE");
        conn.conn
            .change_property32(PropMode::REPLACE, win, wm_state, wm_state, &[1, 0])
            .unwrap();
    }
    conn.conn.map_window(win).unwrap();
    win
}

fn settle(conn: &Conn) {
    conn.conn.sync().unwrap();
    std::thread::sleep(Duration::from_millis(80));
}

fn capture(conn: &Conn, request: &MaskRequest) -> (RgbImage, MaskReport) {
    let screen = &conn.conn.setup().roots[conn.screen_num];
    let rect = RootRect {
        x: 0,
        y: 0,
        width: i32::from(screen.width_in_pixels),
        height: i32::from(screen.height_in_pixels),
    };
    let (img, report) = capture_with_mask(conn, rect, Some(request)).expect("masked capture");
    (img, report.expect("mask report"))
}

fn px(img: &RgbImage, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * img.width + x) * 3) as usize;
    [img.pixels[i], img.pixels[i + 1], img.pixels[i + 2]]
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn mask_keeps_only_allowlisted_windows() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let a = make(
        &conn,
        Spec {
            rect: (100, 100, 300, 200),
            color: RED,
            class: Some("AppA"),
            pid: Some(4001),
            ..Spec::default()
        },
    );
    let b = make(
        &conn,
        Spec {
            rect: (300, 150, 300, 200),
            color: BLUE,
            class: Some("AppB"),
            pid: Some(4002),
            ..Spec::default()
        },
    );
    let c = make(
        &conn,
        Spec {
            rect: (800, 100, 200, 200),
            color: GREEN,
            class: Some("AppC"),
            pid: Some(4003),
            ..Spec::default()
        },
    );
    settle(&conn);

    let (img, report) = capture(&conn, &MaskRequest::Keep(vec![a, c]));
    assert_eq!(px(&img, 150, 150), rgb(RED), "kept A visible");
    assert_eq!(px(&img, 350, 200), MASK_FILL, "B over A is masked");
    assert_eq!(px(&img, 550, 300), MASK_FILL, "B masked");
    assert_eq!(px(&img, 900, 200), rgb(GREEN), "kept C visible");
    assert_eq!(px(&img, 10, 10), MASK_FILL, "root background masked");
    assert_eq!(px(&img, 1279, 799), MASK_FILL);
    assert_eq!(report.mode, "keep");
    assert_eq!(report.kept, vec![a.to_string(), c.to_string()]);
    assert!(report.kept_missing.is_empty());
    let masked: Vec<&str> = report.masked.iter().map(|w| w.id.as_str()).collect();
    assert!(masked.contains(&b.to_string().as_str()), "{masked:?}");
    let entry = report
        .masked
        .iter()
        .find(|w| w.id == b.to_string())
        .unwrap();
    assert_eq!(entry.resource_class.as_deref(), Some("AppB"));
    assert_eq!(entry.pid, Some(4002));
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn mask_all_masks_every_pixel() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let a = make(
        &conn,
        Spec {
            rect: (0, 0, 640, 400),
            color: RED,
            class: Some("AppA"),
            ..Spec::default()
        },
    );
    settle(&conn);
    let (img, report) = capture(&conn, &MaskRequest::All);
    assert!(img.pixels.chunks(3).all(|p| p == MASK_FILL));
    assert_eq!(report.mode, "all");
    assert!(report.kept.is_empty());
    assert!(report.masked.iter().any(|w| w.id == a.to_string()));
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn override_redirect_popups_follow_their_owner() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let a = make(
        &conn,
        Spec {
            rect: (100, 100, 300, 300),
            color: RED,
            class: Some("AppA"),
            pid: Some(5001),
            ..Spec::default()
        },
    );
    let _b = make(
        &conn,
        Spec {
            rect: (700, 100, 300, 300),
            color: BLUE,
            class: Some("AppB"),
            pid: Some(5002),
            ..Spec::default()
        },
    );
    // Menu of A, transient for A, spills outside A (to 450).
    let _menu = make(
        &conn,
        Spec {
            rect: (350, 120, 100, 50),
            color: YELLOW,
            override_redirect: true,
            transient_for: Some(a),
            ..Spec::default()
        },
    );
    // Tooltip of A by pid only.
    let _tip = make(
        &conn,
        Spec {
            rect: (120, 450, 80, 30),
            color: CYAN,
            override_redirect: true,
            class: Some("AppA"),
            pid: Some(5001),
            ..Spec::default()
        },
    );
    // Dropdown of B overlapping A: masked.
    let _drop = make(
        &conn,
        Spec {
            rect: (150, 300, 100, 50),
            color: GREEN,
            override_redirect: true,
            class: Some("AppB"),
            pid: Some(5002),
            ..Spec::default()
        },
    );
    settle(&conn);

    let (img, report) = capture(&conn, &MaskRequest::Keep(vec![a]));
    assert_eq!(px(&img, 200, 200), rgb(RED));
    assert_eq!(px(&img, 440, 140), rgb(YELLOW), "A's menu kept outside A");
    assert_eq!(px(&img, 150, 460), rgb(CYAN), "A's tooltip kept");
    assert_eq!(px(&img, 200, 320), MASK_FILL, "B's dropdown over A masked");
    assert_eq!(px(&img, 800, 200), MASK_FILL, "B masked");
    assert_eq!(report.auto_kept.len(), 2, "{report:?}");
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn reparented_frame_is_kept_whole_and_occluded_by_masked_frame() {
    let conn = Conn::connect().expect("connect to Xvfb");
    // A WM frame (gray "title bar") holding the client.
    let frame = make(
        &conn,
        Spec {
            rect: (100, 100, 400, 300),
            color: GRAY,
            ..Spec::default()
        },
    );
    let client = make(
        &conn,
        Spec {
            parent: Some(frame),
            rect: (5, 30, 390, 265),
            color: RED,
            class: Some("Framed"),
            pid: Some(6001),
            wm_state: true,
            ..Spec::default()
        },
    );
    // Another app's frame on top, overlapping the right part.
    let other_frame = make(
        &conn,
        Spec {
            rect: (400, 200, 300, 300),
            color: GRAY,
            ..Spec::default()
        },
    );
    let other_client = make(
        &conn,
        Spec {
            parent: Some(other_frame),
            rect: (5, 30, 290, 265),
            color: BLUE,
            class: Some("Other"),
            wm_state: true,
            ..Spec::default()
        },
    );
    settle(&conn);

    let (img, report) = capture(&conn, &MaskRequest::Keep(vec![client]));
    assert_eq!(px(&img, 110, 110), rgb(GRAY), "own decoration kept");
    assert_eq!(px(&img, 200, 200), rgb(RED), "client kept");
    assert_eq!(px(&img, 450, 350), MASK_FILL, "other frame on top masked");
    assert_eq!(px(&img, 410, 205), MASK_FILL, "other decoration masked");
    assert_eq!(report.kept, vec![client.to_string()]);
    let other = report
        .masked
        .iter()
        .find(|w| w.frame == other_frame.to_string())
        .expect("other frame reported");
    assert_eq!(other.id, other_client.to_string(), "reported by client id");
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn missing_and_unmapped_ids_are_reported_and_masked() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let hidden = make(
        &conn,
        Spec {
            rect: (0, 0, 200, 200),
            color: RED,
            class: Some("Hidden"),
            ..Spec::default()
        },
    );
    conn.conn.unmap_window(hidden).unwrap();
    settle(&conn);
    let bogus: Window = 0x07ff_fff0;
    let (img, report) = capture(&conn, &MaskRequest::Keep(vec![bogus, hidden]));
    assert!(img.pixels.chunks(3).all(|p| p == MASK_FILL));
    assert_eq!(
        report.kept_missing,
        vec![bogus.to_string(), hidden.to_string()]
    );
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn zoom_region_masks_relative_to_the_region() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let a = make(
        &conn,
        Spec {
            rect: (600, 400, 100, 100),
            color: RED,
            class: Some("AppA"),
            ..Spec::default()
        },
    );
    settle(&conn);
    // Region 550,350 100x100: A covers its bottom-right quarter.
    let rect = RootRect {
        x: 550,
        y: 350,
        width: 100,
        height: 100,
    };
    let (img, report) = capture_with_mask(&conn, rect, Some(&MaskRequest::Keep(vec![a]))).unwrap();
    assert_eq!((img.width, img.height), (100, 100));
    assert_eq!(px(&img, 75, 75), rgb(RED));
    assert_eq!(px(&img, 25, 25), MASK_FILL);
    assert_eq!(report.unwrap().kept, vec![a.to_string()]);

    // End to end through `zoom`: JSON carries the mask report.
    let monitor = conn.resolve_monitor(None).unwrap();
    let out = crate::capture::zoom(
        &conn,
        Some(&monitor.name),
        (550 - monitor.x, 350 - monitor.y, 100, 100),
        Some(&MaskRequest::Keep(vec![a])),
    )
    .unwrap();
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["mask"]["kept"][0], a.to_string());
    assert_eq!(json["mask"]["fill"], "#1E1E1E");
    // Unmasked zoom keeps the old shape (no `mask` key).
    let plain = crate::capture::zoom(&conn, None, (0, 0, 10, 10), None).unwrap();
    assert!(serde_json::to_value(&plain).unwrap().get("mask").is_none());
}
