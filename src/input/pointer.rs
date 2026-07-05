//! Pointer synthesis over XTEST.
//!
//! Warps the pointer with `xtest_fake_input` (MotionNotify, absolute root
//! coords) and presses buttons with ButtonPress/ButtonRelease. Scroll maps to
//! wheel buttons 4/5 (vertical) and 6/7 (horizontal). Drag = down at from,
//! interpolated motion to to, up at to. Left-down / left-up hold button 1
//! across separate invocations — XTEST button state lives in the X server, not
//! our process, so it survives between short-lived runs (no daemon).
//!
//! ## Scroll sign convention
//!
//! Positive `dy` scrolls **down**, positive `dx` scrolls **right**. This is:
//!
//! - what `cu_linux_executor.js` does on the xdotool path (`vertical>0 ->
//!   button 5` = down; `horizontal>0 -> button 7` = right), and
//! - what `kwin-portal-bridge` does via the RemoteDesktop portal
//!   (`notify_pointer_axis(dx, dy)`, whose libinput/wl_pointer axis convention
//!   is positive-down / positive-right).
//!
//! So a positive `dy` means the same direction here as on KDE. X11 wheel
//! buttons: 4=up, 5=down, 6=left, 7=right.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::{KeyboardMap, keysym_for_name};
use crate::output::AppRef;

/// XTEST `fake_input` uses `CurrentTime` (0) for the `time` field.
const CURRENT_TIME: u32 = 0;
/// Default device id (0 = core pointer/keyboard).
const CORE_DEVICE: u8 = 0;
/// Delay between repeated clicks (mirrors xdotool `--delay 50`).
const CLICK_REPEAT_MS: u64 = 50;
/// Number of interpolation steps for a drag (mirrors kwin bridge's smooth drag).
const DRAG_STEPS: u32 = 20;
/// Delay between drag interpolation steps.
const DRAG_STEP_MS: u64 = 8;

/// A resolved pointer button.
#[derive(Debug, Clone, Copy)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// Parse the CLI `--button` string (`left`/`right`/`middle`).
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            other => bail!("unsupported mouse button: {other}"),
        }
    }

    /// X11 core button number: left=1, middle=2, right=3.
    fn number(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
        }
    }
}

/// The result of a pointer action, mirroring kwin-portal-bridge's
/// `PointerActionResult` (camelCase `action`, `x`, `y`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerActionResult {
    pub action: String,
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<AppRef>,
}

/// The result of a drag, mirroring kwin-portal-bridge's `DragActionResult`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DragActionResult {
    pub action: String,
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
}

/// The result of a left-button-held state change.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonStateResult {
    pub action: String,
    pub button: String,
    pub is_held: bool,
}

/// Warp the pointer to an absolute root-window point (like `xdotool mousemove
/// --sync`). Coordinates are clamped to i16 (the XTEST wire type).
fn fake_motion(conn: &Conn, x: i32, y: i32) -> Result<()> {
    let root_x = i16::try_from(x.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0);
    let root_y = i16::try_from(y.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0);
    conn.conn
        .xtest_fake_input(
            MOTION_NOTIFY_EVENT,
            0, // detail=0 => absolute motion
            CURRENT_TIME,
            conn.root,
            root_x,
            root_y,
            CORE_DEVICE,
        )
        .context("failed to synthesize pointer motion")?;
    Ok(())
}

/// Press or release a pointer button at the current cursor position.
fn fake_button(conn: &Conn, button: u8, press: bool) -> Result<()> {
    let event = if press {
        BUTTON_PRESS_EVENT
    } else {
        BUTTON_RELEASE_EVENT
    };
    conn.conn
        .xtest_fake_input(event, button, CURRENT_TIME, conn.root, 0, 0, CORE_DEVICE)
        .context("failed to synthesize button event")?;
    Ok(())
}

/// Press or release a key by keycode.
fn fake_key(conn: &Conn, keycode: u8, press: bool) -> Result<()> {
    use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
    let event = if press {
        KEY_PRESS_EVENT
    } else {
        KEY_RELEASE_EVENT
    };
    conn.conn
        .xtest_fake_input(event, keycode, CURRENT_TIME, conn.root, 0, 0, CORE_DEVICE)
        .context("failed to synthesize key event")?;
    Ok(())
}

/// Resolve modifier names (e.g. `ctrl`, `shift`) to keycodes for chording a
/// click. Reuses the keyboard mapping so click modifiers share the keysym path
/// with the keyboard module.
fn modifier_keycodes(conn: &Conn, modifiers: &[String]) -> Result<Vec<u8>> {
    if modifiers.is_empty() {
        return Ok(Vec::new());
    }
    let map = KeyboardMap::fetch(conn)?;
    let mut keycodes = Vec::with_capacity(modifiers.len());
    for name in modifiers {
        let keysym = keysym_for_name(name)?;
        let resolved = map
            .keycode_for(keysym)
            .ok_or_else(|| anyhow::anyhow!("no keycode for modifier `{name}`"))?;
        keycodes.push(resolved.keycode);
    }
    Ok(keycodes)
}

pub fn move_pointer(conn: &Conn, x: i32, y: i32) -> Result<PointerActionResult> {
    fake_motion(conn, x, y)?;
    conn.sync()?;
    Ok(PointerActionResult {
        action: "move".to_owned(),
        x,
        y,
        raised: None,
    })
}

pub fn click(
    conn: &Conn,
    x: i32,
    y: i32,
    button: Button,
    count: u32,
    modifiers: &[String],
) -> Result<PointerActionResult> {
    let btn = button.number();
    let repeat = count.max(1);
    let modifier_keys = modifier_keycodes(conn, modifiers)?;

    fake_motion(conn, x, y)?;

    for &keycode in &modifier_keys {
        fake_key(conn, keycode, true)?;
    }

    for i in 0..repeat {
        if i > 0 {
            conn.conn.flush().ok();
            thread::sleep(Duration::from_millis(CLICK_REPEAT_MS));
        }
        fake_button(conn, btn, true)?;
        fake_button(conn, btn, false)?;
    }

    // Release modifiers in reverse order.
    for &keycode in modifier_keys.iter().rev() {
        fake_key(conn, keycode, false)?;
    }

    conn.sync()?;
    Ok(PointerActionResult {
        action: "click".to_owned(),
        x,
        y,
        raised: None,
    })
}

pub fn scroll(conn: &Conn, x: i32, y: i32, dx: f64, dy: f64) -> Result<PointerActionResult> {
    fake_motion(conn, x, y)?;

    // Vertical: positive dy -> down (button 5), negative -> up (button 4).
    let vertical_ticks = dy.round().abs() as u32;
    if vertical_ticks > 0 {
        let button = if dy > 0.0 { 5 } else { 4 };
        wheel(conn, button, vertical_ticks)?;
    }

    // Horizontal: positive dx -> right (button 7), negative -> left (button 6).
    let horizontal_ticks = dx.round().abs() as u32;
    if horizontal_ticks > 0 {
        let button = if dx > 0.0 { 7 } else { 6 };
        wheel(conn, button, horizontal_ticks)?;
    }

    conn.sync()?;
    Ok(PointerActionResult {
        action: "scroll".to_owned(),
        x,
        y,
        raised: None,
    })
}

/// Emit `ticks` wheel clicks on the given button (press+release each).
fn wheel(conn: &Conn, button: u8, ticks: u32) -> Result<()> {
    for i in 0..ticks {
        if i > 0 {
            conn.conn.flush().ok();
            thread::sleep(Duration::from_millis(30)); // mirrors xdotool --delay 30
        }
        fake_button(conn, button, true)?;
        fake_button(conn, button, false)?;
    }
    Ok(())
}

pub fn drag(
    conn: &Conn,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<DragActionResult> {
    fake_motion(conn, from_x, from_y)?;
    conn.conn.flush().ok();
    fake_button(conn, 1, true)?;
    conn.conn.flush().ok();
    thread::sleep(Duration::from_millis(DRAG_STEP_MS));

    // Interpolate movement in DRAG_STEPS increments so drag-to-select and
    // drag-and-drop targets that watch motion see a smooth path.
    for step in 1..=DRAG_STEPS {
        let t = f64::from(step) / f64::from(DRAG_STEPS);
        let x = from_x + ((to_x - from_x) as f64 * t).round() as i32;
        let y = from_y + ((to_y - from_y) as f64 * t).round() as i32;
        fake_motion(conn, x, y)?;
        conn.conn.flush().ok();
        thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    }

    // Ensure we land exactly on target before releasing.
    fake_motion(conn, to_x, to_y)?;
    conn.conn.flush().ok();
    thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    fake_button(conn, 1, false)?;
    conn.sync()?;

    Ok(DragActionResult {
        action: "drag".to_owned(),
        from_x,
        from_y,
        to_x,
        to_y,
    })
}

pub fn left_mouse_down(conn: &Conn) -> Result<ButtonStateResult> {
    // Press button 1 at the current cursor position. Stateless/idempotent: the
    // X server tracks the held state across invocations (no daemon). We do not
    // track prior state across processes — pressing an already-held button is
    // tolerated (matches how kpb treats these).
    fake_button(conn, 1, true)?;
    conn.sync()?;
    Ok(ButtonStateResult {
        action: "left-mouse-down".to_owned(),
        button: "left".to_owned(),
        is_held: true,
    })
}

pub fn left_mouse_up(conn: &Conn) -> Result<ButtonStateResult> {
    fake_button(conn, 1, false)?;
    conn.sync()?;
    Ok(ButtonStateResult {
        action: "left-mouse-up".to_owned(),
        button: "left".to_owned(),
        is_held: false,
    })
}
