//! Keyboard synthesis over XTEST.
//!
//! Every command resolves keysyms against the session layout
//! ([`crate::input::layout`]: active XKB group, Shift and, for `type`, AltGr
//! levels) and binds what the layout lacks to spare keycodes
//! ([`crate::input::remap`]). `key-sequence` presses each modifier keycode,
//! taps the base key, then releases the modifiers in reverse order (mirroring
//! the xdotool `keydown/keyup` chording the JS does today). `type` walks the
//! text char-by-char with `delay_ms` between characters. `hold-key` presses
//! keys, sleeps `duration_ms`, releases.

use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::{KeyboardMap, keysym_for_name, parse_spec};
use crate::input::layout::{Layout, Stroke};
use crate::input::remap::{Remapper, plan_pages, recover_stale};

const CURRENT_TIME: u32 = 0;
const CORE_DEVICE: u8 = 0;
/// Small settle delay between key-sequence repeats (matches JS `sleep 0.008`).
const KEY_REPEAT_MS: u64 = 8;

/// Signal that interrupted `type` (0 = none).
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::SeqCst);
}

/// Let SIGTERM / SIGINT / SIGHUP stop `type` between two characters, so the
/// remapped keycodes are restored before the process exits (the default
/// action would kill it with them bound).
fn catch_stop_signals() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores into an atomic.
        unsafe {
            libc::signal(signal, handler);
        }
    }
}

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

/// Tap one key: press the level modifiers, the key, release in reverse.
fn tap(conn: &Conn, layout: &Layout, keycode: Keycode, shift: bool, level3: bool) -> Result<()> {
    let mods: Vec<Keycode> = [
        layout.shift.filter(|_| shift).map(|m| m.keycode),
        layout.level3.filter(|_| level3).map(|m| m.keycode),
    ]
    .into_iter()
    .flatten()
    .collect();
    for &m in &mods {
        fake_key(conn, m, true)?;
    }
    fake_key(conn, keycode, true)?;
    fake_key(conn, keycode, false)?;
    for &m in mods.iter().rev() {
        fake_key(conn, m, false)?;
    }
    Ok(())
}

/// A key of a combo or hold: on the layout (no AltGr, so a shortcut never
/// gains a modifier; Caps Lock ignored), else bound to a spare keycode.
enum ComboKey {
    Layout(Stroke),
    Bound(Keysym),
}

fn combo_key(layout: &Layout, keysym: Keysym) -> ComboKey {
    match layout.find_shortcut(keysym) {
        Some(stroke) => ComboKey::Layout(stroke),
        None => ComboKey::Bound(keysym),
    }
}

/// Bind the keysyms of `keys` the layout lacks, then return each key as a
/// keycode plus whether Shift is needed.
fn combo_keycodes(remapper: &mut Remapper, keys: &[ComboKey]) -> Result<Vec<(Keycode, bool)>> {
    let mut bound: Vec<Keysym> = Vec::new();
    for key in keys {
        if let ComboKey::Bound(sym) = key
            && !bound.contains(sym)
        {
            bound.push(*sym);
        }
    }
    if bound.len() > remapper.capacity() {
        bail!("not enough spare keycodes for this key combination");
    }
    if !bound.is_empty() {
        remapper.load(&bound)?;
        remapper.mark_used();
    }
    keys.iter()
        .map(|key| match key {
            ComboKey::Layout(stroke) => Ok((stroke.keycode, stroke.shift)),
            ComboKey::Bound(sym) => remapper
                .keycode_of(*sym)
                .map(|keycode| (keycode, false))
                .ok_or_else(|| anyhow!("spare keycode binding failed")),
        })
        .collect()
}

/// Send an executor-style key sequence such as `ctrl+shift+tab`, `repeat` times.
///
/// The `keys` field of the result is the raw split tokens (trimmed, non-empty),
/// matching how kwin-portal-bridge echoes the spec back.
pub fn key_sequence(conn: &Conn, keys: &str, repeat: Option<u32>) -> Result<KeyboardActionResult> {
    let spec = parse_spec(keys)?;
    let repeat = repeat.unwrap_or(1).max(1);

    recover_stale(conn)?;
    let layout = Layout::fetch(conn)?;
    let map = KeyboardMap::fetch(conn)?;
    let mut remapper = Remapper::new(conn, &map);

    // Resolve modifiers and the base key up front (modifiers first, base last).
    let mut combo = Vec::with_capacity(spec.modifiers.len() + 1);
    for name in &spec.modifiers {
        let keysym = keysym_for_name(name)?;
        match layout.find_shortcut(keysym) {
            Some(stroke) => combo.push(ComboKey::Layout(stroke)),
            None => bail!("no keycode for modifier `{name}`"),
        }
    }
    combo.push(combo_key(&layout, keysym_for_name(&spec.base)?));
    let keycodes = combo_keycodes(&mut remapper, &combo)?;
    let (&(base, base_shift), modifiers) = keycodes.split_last().expect("base key");

    for i in 0..repeat {
        if i > 0 {
            conn.conn.flush().ok();
            thread::sleep(Duration::from_millis(KEY_REPEAT_MS));
        }
        for &(keycode, _) in modifiers {
            fake_key(conn, keycode, true)?;
        }
        tap(conn, &layout, base, base_shift, false)?;
        conn.conn.flush().ok();
        for &(keycode, _) in modifiers.iter().rev() {
            fake_key(conn, keycode, false)?;
        }
    }
    conn.sync()?;
    remapper.finish()?;

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
/// Return, '\t' -> Tab. Characters on the layout use their key and level
/// (Shift, AltGr); the rest are bound to spare keycodes page by page (see
/// [`crate::input::remap`]).
pub fn type_text(conn: &Conn, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
    catch_stop_signals();
    recover_stale(conn)?;
    let layout = Layout::fetch(conn)?;
    let map = KeyboardMap::fetch(conn)?;
    let mut remapper = Remapper::new(conn, &map);

    // Resolve every character before the first key.
    let mut plan: Vec<(Keysym, Option<Stroke>)> = Vec::new();
    for ch in text.chars() {
        let keysym = match ch {
            '\n' | '\r' => keysym_for_name("return")?,
            '\t' => keysym_for_name("tab")?,
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
        plan.push((keysym, layout.find(keysym, true)));
    }
    let needs: Vec<Option<Keysym>> = plan
        .iter()
        .map(|&(keysym, stroke)| stroke.is_none().then_some(keysym))
        .collect();
    let mut pages = plan_pages(&needs, remapper.capacity())
        .into_iter()
        .peekable();

    let mut char_count = 0usize;
    for (i, &(keysym, stroke)) in plan.iter().enumerate() {
        if INTERRUPTED.load(Ordering::SeqCst) != 0 {
            break;
        }
        if i > 0 && delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
        if pages.peek().is_some_and(|(start, _)| *start == i) {
            let (_, page) = pages.next().expect("peeked");
            remapper.load(&page)?;
        }
        match stroke {
            Some(stroke) => tap(conn, &layout, stroke.keycode, stroke.shift, stroke.level3)?,
            None => {
                let keycode = remapper
                    .keycode_of(keysym)
                    .ok_or_else(|| anyhow!("keysym {keysym:#x} is not bound"))?;
                tap(conn, &layout, keycode, false, false)?;
                remapper.mark_used();
            }
        }
        conn.conn.flush().ok();
        char_count += 1;
    }

    conn.sync()?;
    remapper.finish()?;

    let signal = INTERRUPTED.load(Ordering::SeqCst);
    if signal != 0 {
        bail!("typing stopped by signal {signal} after {char_count} characters");
    }

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

    recover_stale(conn)?;
    let layout = Layout::fetch(conn)?;
    let map = KeyboardMap::fetch(conn)?;
    let mut remapper = Remapper::new(conn, &map);

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

    // Resolve every key to a keycode (+ shift level); off-layout glyphs stay
    // bound for the whole duration.
    let mut combo = Vec::with_capacity(ordered.len());
    for key in &ordered {
        combo.push(combo_key(&layout, keysym_for_name(key)?));
    }
    let held = combo_keycodes(&mut remapper, &combo)?;

    // Press: shift (if any key needs it), then each key in order.
    let shift = layout.shift.map(|m| m.keycode);
    let any_shift = shift.is_some() && held.iter().any(|&(_, s)| s);
    if any_shift {
        fake_key(conn, shift.unwrap(), true)?;
    }
    for &(keycode, _) in &held {
        fake_key(conn, keycode, true)?;
    }
    conn.conn
        .flush()
        .context("failed to flush hold-key press")?;

    thread::sleep(Duration::from_millis(duration_ms));

    // Release in reverse order, then shift.
    for &(keycode, _) in held.iter().rev() {
        fake_key(conn, keycode, false)?;
    }
    if any_shift {
        fake_key(conn, shift.unwrap(), false)?;
    }
    conn.sync()?;
    remapper.finish()?;

    Ok(KeyboardActionResult {
        action: "hold-key".to_owned(),
        keys: keys.to_vec(),
        repeat: None,
        duration_ms: Some(duration_ms),
    })
}
