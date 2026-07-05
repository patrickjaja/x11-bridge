//! Keyboard synthesis over XTEST.
//!
//! Parses a key spec via [`crate::input::keymap`], presses each modifier
//! keycode, taps the base key, then releases the modifiers in reverse order
//! (mirroring the xdotool `keydown/keyup` chording the JS does today). `type`
//! walks the text char-by-char, mapping each to a keysym (temporarily rebinding
//! a spare keycode for glyphs not on the current layout) with `delay_ms`
//! between events. `hold-key` presses keys, sleeps `duration_ms`, releases.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::{KeyboardMap, keysym_for_name, parse_spec};

const CURRENT_TIME: u32 = 0;
const CORE_DEVICE: u8 = 0;
/// Small settle delay between key-sequence repeats (matches JS `sleep 0.008`).
const KEY_REPEAT_MS: u64 = 8;

/// Result of a key-sequence or hold action. Mirrors kwin-portal-bridge's
/// `KeyboardActionResult`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardActionResult {
    pub action: String,
    pub keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Result of a `type` action. Mirrors kwin-portal-bridge's `TypeActionResult`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeActionResult {
    pub action: String,
    pub text: String,
    pub char_count: usize,
}

fn fake_key(conn: &Conn, keycode: Keycode, press: bool) -> Result<()> {
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

/// Send an executor-style key sequence such as `ctrl+shift+tab`, `repeat` times.
///
/// The `keys` field of the result is the raw split tokens (trimmed, non-empty),
/// matching how kwin-portal-bridge echoes the spec back.
pub fn key_sequence(conn: &Conn, keys: &str, repeat: Option<u32>) -> Result<KeyboardActionResult> {
    let spec = parse_spec(keys)?;
    let repeat = repeat.unwrap_or(1).max(1);

    let map = KeyboardMap::fetch(conn)?;

    // Resolve modifiers to keycodes up front.
    let mut modifier_keycodes = Vec::with_capacity(spec.modifiers.len());
    for name in &spec.modifiers {
        let keysym = keysym_for_name(name)?;
        let resolved = map
            .keycode_for(keysym)
            .ok_or_else(|| anyhow::anyhow!("no keycode for modifier `{name}`"))?;
        modifier_keycodes.push(resolved.keycode);
    }

    let base_keysym = keysym_for_name(&spec.base)?;
    let shift_keycode = map.shift_keycode().ok();

    for i in 0..repeat {
        if i > 0 {
            conn.conn.flush().ok();
            thread::sleep(Duration::from_millis(KEY_REPEAT_MS));
        }

        // Press modifiers in order.
        for &keycode in &modifier_keycodes {
            fake_key(conn, keycode, true)?;
        }

        // Resolve the base key (dynamic remap for off-layout glyphs).
        let (resolved, _guard) = map.resolve_or_remap(base_keysym)?;
        let needs_shift = resolved.needs_shift && shift_keycode.is_some();
        if needs_shift {
            fake_key(conn, shift_keycode.unwrap(), true)?;
        }
        fake_key(conn, resolved.keycode, true)?;
        fake_key(conn, resolved.keycode, false)?;
        if needs_shift {
            fake_key(conn, shift_keycode.unwrap(), false)?;
        }
        // _guard restores any remapped keycode here, after release.
        conn.conn.flush().ok();

        // Release modifiers in reverse order.
        for &keycode in modifier_keycodes.iter().rev() {
            fake_key(conn, keycode, false)?;
        }
    }

    conn.sync()?;

    // Echo the raw split tokens, like the kwin bridge does.
    let echoed: Vec<String> = keys
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    Ok(KeyboardActionResult {
        action: "key".to_owned(),
        keys: echoed,
        repeat: Some(repeat),
        duration_ms: None,
    })
}

/// Type `text` as individual key events, `delay_ms` apart. Handles '\n' ->
/// Return, '\t' -> Tab, and off-layout glyphs (accents, symbols, emoji) via a
/// temporary keycode remap.
pub fn type_text(conn: &Conn, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
    let map = KeyboardMap::fetch(conn)?;
    let shift_keycode = map.shift_keycode().ok();

    let mut char_count = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i > 0 && delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }

        // Resolve special chars to their keysym name; everything else via char.
        let keysym = match ch {
            '\n' => keysym_for_name("return")?,
            '\t' => keysym_for_name("tab")?,
            '\r' => keysym_for_name("return")?,
            other => {
                let ks = xkeysym::Keysym::from_char(other);
                if ks == xkeysym::Keysym::NoSymbol {
                    bail!(
                        "no X11 keysym for character `{other}` (U+{:04X})",
                        other as u32
                    );
                }
                ks.raw()
            }
        };

        let (resolved, _guard) = map.resolve_or_remap(keysym)?;
        let needs_shift = resolved.needs_shift && shift_keycode.is_some();
        if needs_shift {
            fake_key(conn, shift_keycode.unwrap(), true)?;
        }
        fake_key(conn, resolved.keycode, true)?;
        fake_key(conn, resolved.keycode, false)?;
        if needs_shift {
            fake_key(conn, shift_keycode.unwrap(), false)?;
        }
        conn.conn.flush().ok();
        // _guard restores a remapped keycode here (after the char is delivered).
        char_count += 1;
    }

    conn.sync()?;

    Ok(TypeActionResult {
        action: "type".to_owned(),
        text: text.to_owned(),
        char_count,
    })
}

/// Hold `keys` down for `duration_ms`, then release. Each entry is a full CU
/// key token (modifier name or base key). Modifiers are pressed first (in
/// given order); all keys are released in reverse order.
pub fn hold_key(conn: &Conn, keys: &[String], duration_ms: u64) -> Result<KeyboardActionResult> {
    if keys.is_empty() {
        bail!("hold key list is empty");
    }

    let map = KeyboardMap::fetch(conn)?;
    let shift_keycode = map.shift_keycode().ok();

    // Sort so modifiers are pressed before non-modifiers, preserving relative
    // order within each group (a stable partition).
    let mut ordered: Vec<&String> = Vec::with_capacity(keys.len());
    for key in keys {
        if crate::input::keymap::is_modifier(key) {
            ordered.push(key);
        }
    }
    for key in keys {
        if !crate::input::keymap::is_modifier(key) {
            ordered.push(key);
        }
    }

    // Resolve every key to a keycode (+ shift level), holding remap guards for
    // the whole duration so off-layout glyphs stay bound while held.
    struct Held {
        keycode: Keycode,
        shift: bool,
    }
    let mut held: Vec<Held> = Vec::with_capacity(ordered.len());
    let mut guards = Vec::new();
    for key in &ordered {
        let keysym = keysym_for_name(key)?;
        let (resolved, guard) = map.resolve_or_remap(keysym)?;
        if let Some(guard) = guard {
            guards.push(guard);
        }
        held.push(Held {
            keycode: resolved.keycode,
            shift: resolved.needs_shift && shift_keycode.is_some(),
        });
    }

    // Press: shift (if any key needs it), then each key in order.
    let any_shift = held.iter().any(|h| h.shift);
    if any_shift {
        fake_key(conn, shift_keycode.unwrap(), true)?;
    }
    for h in &held {
        fake_key(conn, h.keycode, true)?;
    }
    conn.conn
        .flush()
        .context("failed to flush hold-key press")?;

    thread::sleep(Duration::from_millis(duration_ms));

    // Release in reverse order, then shift.
    for h in held.iter().rev() {
        fake_key(conn, h.keycode, false)?;
    }
    if any_shift {
        fake_key(conn, shift_keycode.unwrap(), false)?;
    }
    // Drop the remap guards first (each queues its keymap-restore), then a
    // single round-trip guarantees the release events AND the restores are
    // processed by the server before this one-shot process exits.
    drop(guards);
    conn.sync()?;

    Ok(KeyboardActionResult {
        action: "hold-key".to_owned(),
        keys: keys.to_vec(),
        repeat: None,
        duration_ms: Some(duration_ms),
    })
}
