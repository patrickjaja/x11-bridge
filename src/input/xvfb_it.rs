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

/// Load an XKB layout into the test server with `setxkbmap` (and lock
/// `group`). `holder` keeps a connection open: Xvfb resets its keymap when the
/// last client disconnects.
fn load_layout(holder: &Conn, layout: &str, group: u8) {
    use x11rb::protocol::xkb::{ConnectionExt as _, Group, ID};
    let status = std::process::Command::new("setxkbmap")
        .args(["-layout", layout])
        .status()
        .expect("run setxkbmap (x11-xkb-utils)");
    assert!(status.success(), "setxkbmap -layout {layout} failed");
    holder
        .conn
        .xkb_use_extension(1, 0)
        .unwrap()
        .reply()
        .unwrap();
    let group = match group {
        0 => Group::M1,
        1 => Group::M2,
        2 => Group::M3,
        _ => Group::M4,
    };
    holder
        .conn
        .xkb_latch_lock_state(
            ID::USE_CORE_KBD.into(),
            0u8.into(),
            0u8.into(),
            true,
            group,
            0u8.into(),
            false,
            0,
        )
        .unwrap();
    holder.sync().unwrap();
    std::thread::sleep(Duration::from_millis(100));
}

/// A text sink that translates keys like an Xlib/XKB client: with the keymap
/// it holds, fetched again only when it processes the first key event after a
/// mapping notification - and it is busy for `lag` when the first
/// notification of a change arrives, like a client that is drawing. Returns
/// the text it read once `expect_chars` characters arrived or `timeout` ran
/// out.
fn lazy_client_text(text: &str, delay_ms: u64, lag: Duration, timeout: Duration) -> String {
    use crate::input::layout::Layout;
    use std::sync::mpsc;
    use x11rb::protocol::xkb::{self, ConnectionExt as _};

    let (ready_tx, ready_rx) = mpsc::channel();
    let expect_chars = text.chars().count();
    let client = std::thread::spawn(move || {
        let conn = Conn::connect().expect("client connect");
        conn.conn.xkb_use_extension(1, 0).unwrap().reply().unwrap();
        // An XKB client hears about keymap changes through XKB events.
        let events = xkb::EventType::MAP_NOTIFY | xkb::EventType::NEW_KEYBOARD_NOTIFY;
        conn.conn
            .xkb_select_events(
                xkb::ID::USE_CORE_KBD.into(),
                0u16.into(),
                events,
                0xffu16.into(),
                0xffu16.into(),
                &xkb::SelectEventsAux::new(),
            )
            .unwrap();
        let win = make_focused_window(&conn, EventMask::KEY_PRESS | EventMask::KEY_RELEASE);
        let mut layout = Layout::fetch(&conn).unwrap();
        let mut stale = false;
        let mut out = String::new();
        ready_tx.send(()).unwrap();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && out.chars().count() < expect_chars {
            let Ok(Some(event)) = conn.conn.poll_for_event() else {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            };
            match event {
                Event::MappingNotify(_)
                | Event::XkbMapNotify(_)
                | Event::XkbNewKeyboardNotify(_) => {
                    if !stale {
                        std::thread::sleep(lag);
                    }
                    stale = true;
                }
                Event::KeyPress(kp) => {
                    if stale {
                        layout = Layout::fetch(&conn).unwrap();
                        stale = false;
                    }
                    let state = u16::from(kp.state);
                    let keysym = layout.keysym_at(
                        kp.detail,
                        (state & 0xff) as u8,
                        ((state >> 13) & 3) as u8,
                    );
                    if keysym == xkeysym::key::Return {
                        out.push('\n');
                    } else if let Some(ch) = xkeysym::Keysym::new(keysym).key_char() {
                        out.push(ch);
                    }
                }
                _ => {}
            }
        }
        conn.conn.destroy_window(win).ok();
        conn.sync().ok();
        out
    });
    ready_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let typer = Conn::connect().expect("typer connect");
    keyboard::type_text(&typer, text, delay_ms).expect("type text");
    client.join().unwrap()
}

/// German layout: AltGr characters, dead-key characters, characters on no key
/// and Return reach a client that reads the keymap lazily and is busy when a
/// mapping changes - at the app's default delay and at none. The old
/// per-character remap and restore lost every off-layout character here.
#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn type_text_german_layout_reaches_a_lazy_client() {
    let holder = Conn::connect().expect("connect to Xvfb");
    load_layout(&holder, "de", 0);
    let text = "aZ yz @€{}[]\\|~ ^´` éñ😀中ß Ä?\n+-_/#'\";:<>!=*%&()$";
    for delay in [12, 0] {
        let got = lazy_client_text(
            text,
            delay,
            Duration::from_millis(40),
            Duration::from_secs(15),
        );
        assert_eq!(got, text, "de layout, delay {delay} ms");
    }
    // No keycode is left bound and the journal is gone.
    let map = KeyboardMap::fetch(&holder).unwrap();
    assert!(map.spare_keycodes().len() > 1, "spare keycodes left bound");
    load_layout(&holder, "us", 0);
}

/// Two layouts with the second active: the key is chosen in the active group
/// (the core map describes group 1 only, so `y` used to come out as `z`).
#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn type_text_second_group_types_the_active_layout() {
    let holder = Conn::connect().expect("connect to Xvfb");
    load_layout(&holder, "us,de", 1);
    let text = "yzYZ @{}\\ äöü ^`'";
    let got = lazy_client_text(text, 12, Duration::from_millis(40), Duration::from_secs(10));
    load_layout(&holder, "us", 0);
    assert_eq!(got, text, "us,de with de active");
}

/// More distinct off-layout characters than spare keycodes: the text is typed
/// in pages and arrives intact.
#[test]
#[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
fn type_text_pages_many_distinct_characters() {
    let holder = Conn::connect().expect("connect to Xvfb");
    load_layout(&holder, "de", 0);
    let spares = KeyboardMap::fetch(&holder).unwrap().spare_keycodes().len();
    let text: String = ('\u{0391}'..='\u{03a9}')
        .chain('\u{0430}'..='\u{044f}')
        .filter(|c| !c.is_control())
        .take(spares * 2 + 5)
        .collect();
    let got = lazy_client_text(&text, 0, Duration::from_millis(40), Duration::from_secs(15));
    load_layout(&holder, "us", 0);
    assert_eq!(got, text);
}
