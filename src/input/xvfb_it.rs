//! Integration tests that drive the real XTEST path against a live X server.
//!
//! These are `#[ignore]`d by default because they need a display; run them
//! under a throwaway Xvfb via `scripts/test-xvfb.sh` (NEVER against the user's
//! real session — XTEST injection would move their pointer and type into their
//! focused window). Each test opens a `RustConnection`, sets up a window that
//! selects the relevant events, injects through the input modules, and asserts
//! the server observed exactly what we synthesized.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass,
};

use crate::conn::Conn;
use crate::input::keymap::KeyboardMap;
use crate::input::{keyboard, pointer};

/// Pump the event queue until `deadline`, collecting every event seen.
fn drain_events(conn: &Conn, deadline: Instant) -> Vec<Event> {
    let mut events = Vec::new();
    while Instant::now() < deadline {
        while let Ok(Some(event)) = conn.conn.poll_for_event() {
            events.push(event);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    events
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn pointer_move_lands_where_commanded() {
    let conn = Conn::connect().expect("connect to Xvfb");

    pointer::move_pointer(&conn, 321, 214).expect("move pointer");
    conn.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));

    let reply = conn.conn.query_pointer(conn.root).unwrap().reply().unwrap();
    assert_eq!(
        (reply.root_x, reply.root_y),
        (321, 214),
        "pointer did not land at commanded coordinates"
    );

    // A second move to a different point confirms it is not a fluke.
    pointer::move_pointer(&conn, 10, 480).expect("move pointer again");
    conn.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let reply = conn.conn.query_pointer(conn.root).unwrap().reply().unwrap();
    assert_eq!((reply.root_x, reply.root_y), (10, 480));
}

/// Create a mapped, input-focused window that selects the given events.
fn make_focused_window(conn: &Conn, mask: EventMask) -> x11rb::protocol::xproto::Window {
    let setup = conn.conn.setup();
    let screen = &setup.roots[conn.screen_num];
    let win = conn.conn.generate_id().unwrap();
    conn.conn
        .create_window(
            screen.root_depth,
            win,
            conn.root,
            0,
            0,
            400,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().event_mask(mask),
        )
        .unwrap();
    conn.conn.map_window(win).unwrap();
    conn.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(60));
    conn.conn
        .set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .unwrap();
    conn.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    win
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn click_reaches_a_window() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let win = make_focused_window(
        &conn,
        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
    );

    // Grab the button so the press is reported even without a WM.
    conn.conn.flush().unwrap();
    // Click at the window's interior (window is at root 0,0 sized 400x300).
    pointer::click(&conn, 50, 40, pointer::Button::Left, 1, &[]).expect("click");

    let events = drain_events(&conn, Instant::now() + Duration::from_millis(300));
    let presses = events
        .iter()
        .filter(|e| matches!(e, Event::ButtonPress(_)))
        .count();
    let releases = events
        .iter()
        .filter(|e| matches!(e, Event::ButtonRelease(_)))
        .count();
    assert!(
        presses >= 1 && releases >= 1,
        "expected >=1 button press and release, got press={presses} release={releases}"
    );

    conn.conn.destroy_window(win).ok();
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn type_text_roundtrips_ascii_and_accent() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let win = make_focused_window(&conn, EventMask::KEY_PRESS | EventMask::KEY_RELEASE);

    // Mixed ASCII + uppercase (needs shift) + accented char (off-layout remap).
    // 'é' is not on the (US) Xvfb layout, so the executor binds it to a scratch
    // keycode transiently and restores the layout afterward. We verify:
    //   - the ASCII prefix "Ab1" decodes off the stable layout (uppercase 'A'
    //     confirms the shift-level path), and
    //   - a 4th KeyPress arrived on a keycode distinct from the ASCII keycodes,
    //     confirming the dynamic-remap path fired for the off-layout glyph.
    // (Decoding the scratch keycode's transient 'é' binding directly is
    // inherently racy against the per-char restore, so we assert the observable
    // invariant instead: a separate keycode carried the accent.)
    let text = "Ab1é";
    keyboard::type_text(&conn, text, 20).expect("type text");
    let events = drain_events(&conn, Instant::now() + Duration::from_millis(400));

    let map = KeyboardMap::fetch(&conn).unwrap();
    // The uppercase 'A' also presses Shift_L (its own KeyPress); exclude the
    // shift keycode so we count only the character keys.
    let shift_keycode = map.shift_keycode().ok();
    let press_keycodes: Vec<(u8, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::KeyPress(kp) if Some(kp.detail) != shift_keycode => Some((
                kp.detail,
                kp.state
                    .contains(x11rb::protocol::xproto::KeyButMask::SHIFT),
            )),
            _ => None,
        })
        .collect();

    assert_eq!(
        press_keycodes.len(),
        4,
        "expected 4 character KeyPress events (A,b,1,é), got {}",
        press_keycodes.len()
    );

    // First three decode straight off the stable layout.
    let ascii: String = press_keycodes[..3]
        .iter()
        .filter_map(|&(kc, shifted)| map.decode_keycode(kc, shifted))
        .collect();
    assert_eq!(
        ascii, "Ab1",
        "ASCII prefix did not round-trip (got {ascii:?})"
    );

    // The accent used a distinct scratch keycode (not any ASCII keycode above).
    let ascii_keycodes: Vec<u8> = press_keycodes[..3].iter().map(|&(kc, _)| kc).collect();
    let accent_keycode = press_keycodes[3].0;
    assert!(
        !ascii_keycodes.contains(&accent_keycode),
        "accent reused an ASCII keycode ({accent_keycode}); dynamic remap did not fire"
    );

    conn.conn.destroy_window(win).ok();
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn key_sequence_delivers_base_key() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let win = make_focused_window(&conn, EventMask::KEY_PRESS | EventMask::KEY_RELEASE);

    // ctrl+a: the base 'a' KeyPress must arrive while Control is held.
    keyboard::key_sequence(&conn, "ctrl+a", None).expect("key sequence");
    let events = drain_events(&conn, Instant::now() + Duration::from_millis(300));

    let map = KeyboardMap::fetch(&conn).unwrap();
    let saw_a_with_ctrl = events.iter().any(|e| {
        if let Event::KeyPress(kp) = e {
            let ctrl = kp
                .state
                .contains(x11rb::protocol::xproto::KeyButMask::CONTROL);
            map.decode_keycode(kp.detail, false) == Some('a') && ctrl
        } else {
            false
        }
    });
    assert!(saw_a_with_ctrl, "did not observe ctrl+a base key press");

    conn.conn.destroy_window(win).ok();
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn scroll_sign_convention_maps_to_wheel_buttons() {
    // Verifies the documented convention: positive dy -> button 5 (down),
    // negative dy -> button 4 (up); positive dx -> button 7 (right),
    // negative dx -> button 6 (left). Wheel events are ButtonPress on the
    // corresponding button.
    let conn = Conn::connect().expect("connect to Xvfb");
    let win = make_focused_window(&conn, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE);

    let buttons_from = |dx: f64, dy: f64| -> Vec<u8> {
        pointer::scroll(&conn, 50, 40, dx, dy).expect("scroll");
        drain_events(&conn, Instant::now() + Duration::from_millis(200))
            .iter()
            .filter_map(|e| match e {
                Event::ButtonPress(bp) => Some(bp.detail),
                _ => None,
            })
            .collect()
    };

    assert!(
        buttons_from(0.0, 3.0).contains(&5),
        "positive dy should scroll down (button 5)"
    );
    assert!(
        buttons_from(0.0, -3.0).contains(&4),
        "negative dy should scroll up (button 4)"
    );
    assert!(
        buttons_from(3.0, 0.0).contains(&7),
        "positive dx should scroll right (button 7)"
    );
    assert!(
        buttons_from(-3.0, 0.0).contains(&6),
        "negative dx should scroll left (button 6)"
    );

    conn.conn.destroy_window(win).ok();
}

#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn drag_presses_moves_and_releases() {
    let conn = Conn::connect().expect("connect to Xvfb");
    let win = make_focused_window(
        &conn,
        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON1_MOTION,
    );

    pointer::drag(&conn, 30, 30, 200, 150).expect("drag");
    let events = drain_events(&conn, Instant::now() + Duration::from_millis(500));

    let press = events
        .iter()
        .filter(|e| matches!(e, Event::ButtonPress(_)))
        .count();
    let release = events
        .iter()
        .filter(|e| matches!(e, Event::ButtonRelease(_)))
        .count();
    let motion = events
        .iter()
        .filter(|e| matches!(e, Event::MotionNotify(_)))
        .count();

    assert!(press >= 1, "drag should press button 1");
    assert!(release >= 1, "drag should release button 1");
    assert!(
        motion >= 2,
        "drag should emit interpolated motion (got {motion} motion events)"
    );

    // Cursor should end at the drag target.
    let ptr = conn.conn.query_pointer(conn.root).unwrap().reply().unwrap();
    assert_eq!(
        (ptr.root_x, ptr.root_y),
        (200, 150),
        "cursor did not end at the drag target"
    );

    conn.conn.destroy_window(win).ok();
}
