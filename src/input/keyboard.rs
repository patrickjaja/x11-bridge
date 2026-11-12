//! Keyboard synthesis over XTEST.
//!
//! Every command resolves keysyms against the session layout
//! ([`crate::input::layout`]: active XKB group, Shift, Caps Lock and, for
//! `type`, AltGr levels) and binds what the layout lacks to spare keycodes
//! ([`crate::input::remap`]). `key-sequence` presses each modifier keycode,
//! taps the base key, then releases the modifiers in reverse order (mirroring
//! the xdotool `keydown/keyup` chording the JS does today). `type` walks the
//! text char-by-char with `delay_ms` between characters. `hold-key` presses
//! keys, sleeps `duration_ms`, releases.
//!
//! SIGTERM / SIGINT / SIGHUP (installed by `main` with
//! [`catch_stop_signals`]) stop each command at the next safe point: `type`
//! between two characters, `hold-key` during its hold, `key-sequence` between
//! repetitions. Keys are released and keycodes restored before the result
//! (`interrupted: true`) is returned.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::{keysym_for_name, parse_spec};
use crate::input::layout::{Layout, Stroke};
use crate::input::remap::{Remapper, Unit, plan_pages, unit_for};

const CURRENT_TIME: u32 = 0;
const CORE_DEVICE: u8 = 0;
/// Small settle delay between key-sequence repeats (matches JS `sleep 0.008`).
const KEY_REPEAT_MS: u64 = 8;

/// Signal that asked the running command to stop (0 = none).
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::SeqCst);
}

/// Let SIGTERM / SIGINT / SIGHUP stop a keyboard command at its next safe
/// point, so keys are released and bound keycodes restored before the
/// process exits (the default action would kill it with them in place).
/// Called by `main` only: the in-process tests keep their own handlers.
pub fn catch_stop_signals() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores into an atomic.
        unsafe {
            libc::signal(signal, handler);
        }
    }
}

fn stop_signal() -> Option<i32> {
    match INTERRUPTED.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
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
    /// Set when a stop signal cut the command short (keys released).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
}

/// Result of a `type` action. Mirrors kwin-portal-bridge's `TypeActionResult`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeActionResult {
    pub action: String,
    pub text: String,
    /// Characters (Unicode scalar values) typed, from the start of `text`.
    pub char_count: usize,
    /// Set when typing stopped before the end of `text`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
    /// Why typing stopped: `"signal"` or `"keymapChanged"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
fn tap(conn: &Conn, layout: &Layout, stroke: Stroke) -> Result<()> {
    let mods: Vec<Keycode> = [
        layout.shift.filter(|_| stroke.shift).map(|m| m.keycode),
        layout.level3.filter(|_| stroke.level3).map(|m| m.keycode),
    ]
    .into_iter()
    .flatten()
    .collect();
    for &m in &mods {
        fake_key(conn, m, true)?;
    }
    fake_key(conn, stroke.keycode, true)?;
    fake_key(conn, stroke.keycode, false)?;
    for &m in mods.iter().rev() {
        fake_key(conn, m, false)?;
    }
    Ok(())
}

/// Resolve the keys of a combo or hold to physical keys (+ Shift): the layout
/// first (active group, then the others; no AltGr; Caps Lock ignored), then
/// bound to spare keycodes. Returns the layout to press with.
fn combo_strokes(
    conn: &Conn,
    remapper: &mut Remapper,
    layout: Layout,
    keysyms: &[Keysym],
) -> Result<(Layout, Vec<Stroke>)> {
    let mut units: Vec<Unit> = Vec::new();
    for &k in keysyms {
        if layout.find_shortcut(k).is_none() {
            let u = unit_for(k);
            if !units.contains(&u) {
                units.push(u);
            }
        }
    }
    let layout = if units.is_empty() {
        layout
    } else {
        let slots = remapper.prepare()?;
        if crate::input::remap::slots_needed(&units) > slots {
            bail!("not enough spare keycodes for this key combination");
        }
        remapper.load(&units)?;
        Layout::fetch(conn)?
    };
    let strokes = keysyms
        .iter()
        .map(|&k| {
            layout
                .find_shortcut(k)
                .ok_or_else(|| anyhow!("no key produces keysym {k:#x}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((layout, strokes))
}

/// Send an executor-style key sequence such as `ctrl+shift+tab`, `repeat` times.
///
/// The `keys` field of the result is the raw split tokens (trimmed, non-empty),
/// matching how kwin-portal-bridge echoes the spec back.
pub fn key_sequence(conn: &Conn, keys: &str, repeat: Option<u32>) -> Result<KeyboardActionResult> {
    let spec = parse_spec(keys)?;
    let repeat = repeat.unwrap_or(1).max(1);

    let mut remapper = Remapper::new(conn)?;
    let layout = Layout::fetch(conn)?;

    // Modifiers first, base last.
    let mut keysyms = Vec::with_capacity(spec.modifiers.len() + 1);
    for name in &spec.modifiers {
        let keysym = keysym_for_name(name)?;
        if layout.find_shortcut(keysym).is_none() {
            bail!("no keycode for modifier `{name}`");
        }
        keysyms.push(keysym);
    }
    keysyms.push(keysym_for_name(&spec.base)?);
    let (layout, strokes) = combo_strokes(conn, &mut remapper, layout, &keysyms)?;
    let (&base, modifiers) = strokes.split_last().expect("base key");

    let mut interrupted = false;
    for i in 0..repeat {
        if i > 0 {
            conn.conn.flush().ok();
            thread::sleep(Duration::from_millis(KEY_REPEAT_MS));
            if stop_signal().is_some() {
                interrupted = true;
                break;
            }
        }
        for m in modifiers {
            fake_key(conn, m.keycode, true)?;
        }
        tap(conn, &layout, base)?;
        conn.conn.flush().ok();
        for m in modifiers.iter().rev() {
            fake_key(conn, m.keycode, false)?;
        }
    }
    conn.sync()?;
    // The pause before the restore counts from the last release.
    remapper.note_key(true);
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
        interrupted: interrupted.then_some(true),
    })
}

/// Type `text` as individual key events, `delay_ms` apart. Handles '\n' ->
/// Return, '\t' -> Tab. Characters on the layout use their key and level
/// (Shift, AltGr, with Caps Lock accounted for); the rest are bound to spare
/// keycodes page by page (see [`crate::input::remap`]).
///
/// A stop signal or a keymap change by someone else ends typing between two
/// characters: the result then has `interrupted: true`, the `reason` and the
/// number of characters typed. A signal that arrives after the last
/// character is not an interruption.
pub fn type_text(conn: &Conn, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
    let mut remapper = Remapper::new(conn)?;
    let mut layout = Layout::fetch(conn)?;

    // Resolve every character before the first key (each distinct keysym
    // once).
    let mut plan: Vec<(Keysym, Option<Stroke>)> = Vec::new();
    let mut resolved: HashMap<Keysym, Option<Stroke>> = HashMap::new();
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
        let stroke = *resolved
            .entry(keysym)
            .or_insert_with(|| layout.find(keysym, true));
        plan.push((keysym, stroke));
    }
    let needs: Vec<Option<Unit>> = plan
        .iter()
        .map(|&(keysym, stroke)| stroke.is_none().then(|| unit_for(keysym)))
        .collect();
    let slots = if needs.iter().any(Option::is_some) {
        remapper.prepare()?
    } else {
        0
    };
    let mut pages = plan_pages(&needs, slots).into_iter().peekable();
    // Strokes of the bound characters of the current page.
    let mut bound: HashMap<Keysym, Stroke> = HashMap::new();

    let total = plan.len();
    let mut char_count = 0usize;
    let mut reason: Option<&str> = None;
    for (i, &(keysym, stroke)) in plan.iter().enumerate() {
        if stop_signal().is_some() {
            reason = Some("signal");
            break;
        }
        if remapper.keymap_suspect() {
            let now = Layout::fetch(conn)?;
            if now.keys != layout.keys || now.types != layout.types || now.group != layout.group {
                reason = Some("keymapChanged");
                break;
            }
        }
        if i > 0 && delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
        if pages.peek().is_some_and(|(start, _)| *start == i) {
            let (_, page) = pages.next().expect("peeked");
            remapper.load(&page)?;
            layout = Layout::fetch(conn)?;
            if remapper.fix_case_types(&layout)? {
                layout = Layout::fetch(conn)?;
            }
            bound.clear();
            for unit in &page {
                for k in unit.keysyms() {
                    let found = layout.find(k, false).ok_or_else(|| {
                        anyhow!("the X server does not type keysym {k:#x} after binding it")
                    })?;
                    bound.insert(k, found);
                }
            }
        }
        match stroke {
            Some(stroke) => {
                tap(conn, &layout, stroke)?;
                remapper.note_key(false);
            }
            None => {
                let stroke = bound
                    .iter()
                    .find(|(k, _)| crate::input::layout::same_symbol(**k, keysym))
                    .map(|(_, s)| *s)
                    .ok_or_else(|| anyhow!("keysym {keysym:#x} is not bound"))?;
                tap(conn, &layout, stroke)?;
                remapper.note_key(true);
            }
        }
        conn.conn.flush().ok();
        char_count += 1;
    }

    conn.sync()?;
    remapper.finish()?;

    let interrupted = char_count < total;
    Ok(TypeActionResult {
        action: "type".to_owned(),
        text: text.to_owned(),
        char_count,
        interrupted: interrupted.then_some(true),
        reason: if interrupted {
            reason.map(str::to_owned)
        } else {
            None
        },
    })
}

/// Hold `keys` down for `duration_ms`, then release. Each entry is a full CU
/// key token (modifier name or base key). Modifiers are pressed first (in
/// given order); all keys are released in reverse order. A stop signal ends
/// the hold early (keys released, `interrupted: true`).
pub fn hold_key(conn: &Conn, keys: &[String], duration_ms: u64) -> Result<KeyboardActionResult> {
    if keys.is_empty() {
        bail!("hold key list is empty");
    }

    let mut remapper = Remapper::new(conn)?;
    let layout = Layout::fetch(conn)?;

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
    let keysyms = ordered
        .iter()
        .map(|k| keysym_for_name(k))
        .collect::<Result<Vec<_>>>()?;
    let (layout, held) = combo_strokes(conn, &mut remapper, layout, &keysyms)?;

    // Press: shift (if any key needs it), then each key in order.
    let shift = layout.shift.map(|m| m.keycode);
    let any_shift = shift.is_some() && held.iter().any(|s| s.shift);
    if any_shift {
        fake_key(conn, shift.unwrap(), true)?;
    }
    for s in &held {
        fake_key(conn, s.keycode, true)?;
    }
    conn.conn
        .flush()
        .context("failed to flush hold-key press")?;

    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    let mut interrupted = false;
    while Instant::now() < deadline {
        if stop_signal().is_some() {
            interrupted = true;
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        thread::sleep(left.min(Duration::from_millis(10)));
    }

    // Release in reverse order, then shift.
    for s in held.iter().rev() {
        fake_key(conn, s.keycode, false)?;
    }
    if any_shift {
        fake_key(conn, shift.unwrap(), false)?;
    }
    conn.sync()?;
    remapper.note_key(true);
    remapper.finish()?;

    Ok(KeyboardActionResult {
        action: "hold-key".to_owned(),
        keys: keys.to_vec(),
        repeat: None,
        duration_ms: Some(duration_ms),
        interrupted: interrupted.then_some(true),
    })
}
