//! CU key-spec parsing: turn strings like `ctrl+shift+tab` into modifiers plus
//! a base key, and map CU key-names to X11 keysyms.
//!
//! The name table below is extracted from `_mapKey` in
//! `js/cu_linux_executor.js` (the current xdotool-era mapping) — it is the set
//! of CU key-spec tokens the parser MUST accept. This module resolves each name
//! (or a literal char) to an X11 keysym value, then to a live keycode via the
//! server's keyboard mapping. Which key and level types a keysym is decided
//! by [`crate::input::layout`]; glyphs the layout lacks are bound to spare
//! keycodes by [`crate::input::remap`].
//!
//! Key-spec grammar (matches the JS `combo.split("+")`):
//!   spec        := token ("+" token)*
//!   token       := modifier | keyname
//!   modifier    := "ctrl" | "control" | "alt" | "shift"
//!                | "super" | "meta" | "cmd" | "command"   (cmd/meta/command -> super)
//!   keyname     := one of the CU_KEY_NAMES table entries, OR a single literal
//!                  character (e.g. "a", "1", "/") passed through verbatim.
//! Matching is case-insensitive; the base key is the last non-modifier token.
//!
//! CU key-name table (CU spec token -> X11 keysym name), from `_mapKey`:
//!   "ctrl" / "control"                 -> Control_L      (modifier)
//!   "alt"                              -> Alt_L          (modifier)
//!   "shift"                            -> Shift_L        (modifier)
//!   "super"/"meta"/"cmd"/"command"     -> Super_L        (modifier)
//!   "enter" / "return"                 -> Return
//!   "backspace"                        -> BackSpace
//!   "delete"                           -> Delete
//!   "escape" / "esc"                   -> Escape
//!   "tab"                              -> Tab
//!   "space" / " "                      -> space
//!   "up"                               -> Up
//!   "down"                             -> Down
//!   "left"                             -> Left
//!   "right"                            -> Right
//!   "home"                             -> Home
//!   "end"                              -> End
//!   "pageup" / "page_up"               -> Prior
//!   "pagedown" / "page_down"           -> Next
//!   "capslock"                         -> Caps_Lock
//! Anything else (single letters/digits/punctuation) is passed through as-is
//! and resolved as a keysym by name/char.
//!
//! NOTE on `page_up`/`page_down`: the JS `_mapKey` only strips to lowercase and
//! matches `"pageup"`/`"pagedown"` (no underscore). CU emits both `page_down`
//! and `pagedown` historically, so the parser accepts the underscore forms too
//! (normalized by removing `_`).

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, Keycode, Keysym};
use xkeysym::Keysym as XKeysym;

use crate::conn::Conn;

/// A parsed key spec: modifier keysym-names plus the base key name.
#[derive(Debug, Clone)]
pub struct KeySpec {
    pub modifiers: Vec<String>,
    pub base: String,
}

/// Modifier tokens (lowercased) recognized at the front of a key spec.
/// `cmd`/`command`/`meta` normalize to `super`.
pub const MODIFIER_TOKENS: &[&str] = &[
    "ctrl", "control", "alt", "shift", "super", "meta", "cmd", "command",
];

/// CU key-name -> X11 keysym-name table, extracted from `_mapKey`.
/// Underscore forms (`page_up`, `page_down`) are included as aliases; the
/// parser also accepts a caller stripping `_` before lookup.
pub const CU_KEY_NAMES: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("return", "Return"),
    ("backspace", "BackSpace"),
    ("delete", "Delete"),
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("tab", "Tab"),
    ("space", "space"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Prior"),
    ("page_up", "Prior"),
    ("pagedown", "Next"),
    ("page_down", "Next"),
    ("capslock", "Caps_Lock"),
];

/// X11 keysym-name -> keysym value table.
///
/// Covers the CU table's target names plus the modifier keysyms and the common
/// extra names a raw X keysym-name passthrough might carry (function keys,
/// keypad, navigation). Names are matched case-sensitively first (X keysym
/// names are case-sensitive, e.g. `Return` vs `return`), with an ASCII-lower
/// fallback for the CU table's lowercase tokens.
const KEYSYM_NAMES: &[(&str, u32)] = &[
    // Modifiers (resolved as base keys too, e.g. hold-key --key ctrl).
    ("Control_L", xkeysym::key::Control_L),
    ("Control_R", xkeysym::key::Control_R),
    ("Shift_L", xkeysym::key::Shift_L),
    ("Shift_R", xkeysym::key::Shift_R),
    ("Alt_L", xkeysym::key::Alt_L),
    ("Alt_R", xkeysym::key::Alt_R),
    ("Super_L", xkeysym::key::Super_L),
    ("Super_R", xkeysym::key::Super_R),
    ("Meta_L", xkeysym::key::Meta_L),
    ("Meta_R", xkeysym::key::Meta_R),
    ("Caps_Lock", xkeysym::key::Caps_Lock),
    ("Num_Lock", xkeysym::key::Num_Lock),
    // CU named keys.
    ("Return", xkeysym::key::Return),
    ("BackSpace", xkeysym::key::BackSpace),
    ("Delete", xkeysym::key::Delete),
    ("Escape", xkeysym::key::Escape),
    ("Tab", xkeysym::key::Tab),
    ("space", xkeysym::key::space),
    ("Up", xkeysym::key::Up),
    ("Down", xkeysym::key::Down),
    ("Left", xkeysym::key::Left),
    ("Right", xkeysym::key::Right),
    ("Home", xkeysym::key::Home),
    ("End", xkeysym::key::End),
    ("Prior", xkeysym::key::Prior),
    ("Next", xkeysym::key::Next),
    ("Insert", xkeysym::key::Insert),
    ("Menu", xkeysym::key::Menu),
    ("Print", xkeysym::key::Print),
    ("Pause", xkeysym::key::Pause),
    // Function keys F1..F24 (common raw passthrough names).
    ("F1", xkeysym::key::F1),
    ("F2", xkeysym::key::F2),
    ("F3", xkeysym::key::F3),
    ("F4", xkeysym::key::F4),
    ("F5", xkeysym::key::F5),
    ("F6", xkeysym::key::F6),
    ("F7", xkeysym::key::F7),
    ("F8", xkeysym::key::F8),
    ("F9", xkeysym::key::F9),
    ("F10", xkeysym::key::F10),
    ("F11", xkeysym::key::F11),
    ("F12", xkeysym::key::F12),
    ("F13", xkeysym::key::F13),
    ("F14", xkeysym::key::F14),
    ("F15", xkeysym::key::F15),
    ("F16", xkeysym::key::F16),
    ("F17", xkeysym::key::F17),
    ("F18", xkeysym::key::F18),
    ("F19", xkeysym::key::F19),
    ("F20", xkeysym::key::F20),
    ("F21", xkeysym::key::F21),
    ("F22", xkeysym::key::F22),
    ("F23", xkeysym::key::F23),
    ("F24", xkeysym::key::F24),
];

/// Normalize a token for CU-table / modifier lookup: trim, lowercase, and drop
/// underscores (so `page_down` == `pagedown`, matching the DESIGN grammar).
fn normalize_token(token: &str) -> String {
    token
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|&c| c != '_')
        .collect()
}

/// True if `token` names a modifier. `cmd`/`command`/`meta`/`super` all count.
pub fn is_modifier(token: &str) -> bool {
    MODIFIER_TOKENS.contains(&normalize_token(token).as_str())
}

/// Canonical X11 keysym-name for a modifier token (`ctrl` -> `Control_L`, ...).
fn modifier_keysym_name(token: &str) -> Option<&'static str> {
    match normalize_token(token).as_str() {
        "ctrl" | "control" => Some("Control_L"),
        "alt" => Some("Alt_L"),
        "shift" => Some("Shift_L"),
        "super" | "meta" | "cmd" | "command" => Some("Super_L"),
        _ => None,
    }
}

/// Parse a CU key spec (`ctrl+shift+tab`) into modifiers + base key.
///
/// Tokens are split on `+`, trimmed, and empty tokens dropped (so a literal
/// `+` base key is not directly expressible — that matches the JS
/// `combo.split("+")` behavior). All-but-last modifier tokens become modifiers;
/// the last non-modifier token is the base. A spec that is *all* modifiers
/// (e.g. `ctrl+shift`) uses its last token as the base key with the rest as
/// modifiers, so chords like `hold-key --key ctrl` still resolve.
pub fn parse_spec(spec: &str) -> Result<KeySpec> {
    let tokens: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.is_empty() {
        bail!("key spec is empty");
    }

    // The base is the last non-modifier token; everything before it that is a
    // modifier becomes a modifier. If every token is a modifier, the last one
    // is the base (e.g. a bare `ctrl`).
    let base_idx = tokens
        .iter()
        .rposition(|token| !is_modifier(token))
        .unwrap_or(tokens.len() - 1);

    let mut modifiers = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if idx == base_idx {
            continue;
        }
        if !is_modifier(token) {
            bail!("unexpected non-modifier token `{token}` before base key in `{spec}`");
        }
        modifiers.push((*token).to_owned());
    }

    Ok(KeySpec {
        modifiers,
        base: tokens[base_idx].to_owned(),
    })
}

/// Resolve a CU key-name (or literal char) to its X11 keysym-name.
///
/// This mirrors the JS `_mapKey`: known CU tokens map to their keysym name;
/// modifiers map to their `*_L` keysym name; single characters map to their
/// keysym via `Keysym::from_char`; anything else is treated as a raw X11
/// keysym name and passed through verbatim.
pub fn keysym_name(name: &str) -> Result<String> {
    if let Some(modifier) = modifier_keysym_name(name) {
        return Ok(modifier.to_owned());
    }

    let normalized = normalize_token(name);
    if let Some((_, keysym_name)) = CU_KEY_NAMES.iter().find(|(cu, _)| *cu == normalized) {
        return Ok((*keysym_name).to_owned());
    }

    // A single literal char (letter/digit/punctuation): pass through verbatim.
    // `keysym_for_name` resolves it via `Keysym::from_char`.
    let trimmed = name.trim();
    if trimmed.chars().count() == 1 {
        return Ok(trimmed.to_owned());
    }

    // Otherwise treat as a raw X11 keysym name (e.g. `F5`, `eacute`).
    Ok(trimmed.to_owned())
}

/// Standard X keysym names (keysymdef.h) for the printable ASCII punctuation
/// and symbols; each keysym equals its ASCII code. Models send xdotool-style
/// names such as `ctrl+comma`, so these resolve like the literal character.
const ASCII_KEYSYM_NAMES: &[(&str, char)] = &[
    ("space", ' '),
    ("exclam", '!'),
    ("quotedbl", '"'),
    ("numbersign", '#'),
    ("dollar", '$'),
    ("percent", '%'),
    ("ampersand", '&'),
    ("apostrophe", '\''),
    ("quoteright", '\''),
    ("parenleft", '('),
    ("parenright", ')'),
    ("asterisk", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("minus", '-'),
    ("period", '.'),
    ("slash", '/'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less", '<'),
    ("equal", '='),
    ("greater", '>'),
    ("question", '?'),
    ("at", '@'),
    ("bracketleft", '['),
    ("backslash", '\\'),
    ("bracketright", ']'),
    ("asciicircum", '^'),
    ("underscore", '_'),
    ("grave", '`'),
    ("quoteleft", '`'),
    ("braceleft", '{'),
    ("bar", '|'),
    ("braceright", '}'),
    ("asciitilde", '~'),
];

/// Look a keysym name up case-insensitively: the keysym-name table first, then
/// the ASCII punctuation names.
fn keysym_by_name_ignore_case(name: &str) -> Option<Keysym> {
    if let Some((_, value)) = KEYSYM_NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return Some(*value);
    }
    ASCII_KEYSYM_NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, ch)| ch as Keysym)
}

/// Resolve a key-spec token (CU name / modifier / literal char / raw keysym
/// name) to an X11 keysym *value*.
pub fn keysym_for_name(name: &str) -> Result<Keysym> {
    let resolved = keysym_name(name)?;

    // Exact keysym-name table (case-sensitive: X keysym names are).
    if let Some((_, value)) = KEYSYM_NAMES.iter().find(|(n, _)| *n == resolved) {
        return Ok(*value);
    }

    // A single char: resolve via xkeysym's Unicode -> keysym mapping.
    if resolved.chars().count() == 1 {
        let ch = resolved.chars().next().expect("count == 1");
        let ks = XKeysym::from_char(ch);
        if ks != XKeysym::NoSymbol {
            return Ok(ks.raw());
        }
        bail!("no X11 keysym for character `{ch}` (U+{:04X})", ch as u32);
    }

    if let Some(value) = keysym_by_name_ignore_case(&resolved) {
        return Ok(value);
    }

    bail!("unsupported key name `{name}` (resolved keysym name `{resolved}`)")
}

/// A keysym resolved to a live keycode, plus whether Shift must be held to
/// produce it (the keysym sits at column 1 of the keycode's group).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedKey {
    pub keycode: Keycode,
    /// Kept for callers that type at level 2; the click modifiers and tests
    /// only need the keycode.
    #[allow(dead_code)]
    pub needs_shift: bool,
}

/// The server keyboard mapping (keysyms per keycode over the min..=max range),
/// used to turn keysyms into keycodes and to find/restore spare keycodes for
/// off-layout glyphs.
pub struct KeyboardMap {
    min_keycode: u8,
    max_keycode: u8,
    keysyms_per_keycode: usize,
    keysyms: Vec<Keysym>,
}

impl KeyboardMap {
    /// Fetch the full keyboard mapping from the server.
    pub fn fetch(conn: &Conn) -> Result<Self> {
        let setup = conn.conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let count = max_keycode - min_keycode + 1;

        let reply = conn
            .conn
            .get_keyboard_mapping(min_keycode, count)
            .context("failed to request keyboard mapping")?
            .reply()
            .context("failed to read keyboard mapping reply")?;

        Ok(Self {
            min_keycode,
            max_keycode,
            keysyms_per_keycode: usize::from(reply.keysyms_per_keycode),
            keysyms: reply.keysyms,
        })
    }

    /// Keysyms bound to a given keycode (a slice of `keysyms_per_keycode`).
    fn syms_for(&self, keycode: u8) -> &[Keysym] {
        let idx = usize::from(keycode - self.min_keycode) * self.keysyms_per_keycode;
        &self.keysyms[idx..idx + self.keysyms_per_keycode]
    }

    /// Find the keycode already bound to `keysym`, tracking whether it needs
    /// Shift (keysym at column 1). Columns 0/1 are the base group; higher
    /// columns (mode-switched groups) are ignored to keep injection predictable.
    pub fn keycode_for(&self, keysym: Keysym) -> Option<ResolvedKey> {
        if keysym == 0 {
            return None;
        }
        for keycode in self.min_keycode..=self.max_keycode {
            let syms = self.syms_for(keycode);
            if syms.first().copied() == Some(keysym) {
                return Some(ResolvedKey {
                    keycode,
                    needs_shift: false,
                });
            }
            if self.keysyms_per_keycode >= 2 && syms.get(1).copied() == Some(keysym) {
                return Some(ResolvedKey {
                    keycode,
                    needs_shift: true,
                });
            }
        }
        None
    }

    /// Decode a keycode + shift level back to a `char` (test/debug helper for
    /// verifying injected key events). Returns the base-group keysym at column
    /// 0 (or 1 if `shifted`) mapped through `xkeysym`'s keysym -> char table.
    /// Only the Xvfb integration tests read events back this way, so it is dead
    /// code in a non-test build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn decode_keycode(&self, keycode: u8, shifted: bool) -> Option<char> {
        if keycode < self.min_keycode || keycode > self.max_keycode {
            return None;
        }
        let syms = self.syms_for(keycode);
        let column = if shifted { 1 } else { 0 };
        let raw = syms.get(column).copied().filter(|&s| s != 0).or_else(|| {
            // Fall back to column 0 if the shifted column is empty.
            syms.first().copied().filter(|&s| s != 0)
        })?;
        XKeysym::new(raw).key_char()
    }

    /// Keysyms per keycode in the core mapping.
    pub fn keysyms_per_keycode(&self) -> usize {
        self.keysyms_per_keycode
    }

    /// Keycodes free for binding off-layout characters, with their rows: every
    /// keycode without any keysym (no physical key of the layout produces
    /// it). Without one, the last keycode is borrowed (the xdotool fallback).
    pub fn spare_keycodes(&self) -> Vec<(u8, Vec<Keysym>)> {
        let spares: Vec<(u8, Vec<Keysym>)> = (self.min_keycode..=self.max_keycode)
            .filter(|&keycode| self.syms_for(keycode).iter().all(|&s| s == 0))
            .map(|keycode| (keycode, self.syms_for(keycode).to_vec()))
            .collect();
        if spares.is_empty() {
            vec![(self.max_keycode, self.syms_for(self.max_keycode).to_vec())]
        } else {
            spares
        }
    }

    /// The Shift_L keycode, for wrapping keysyms that sit at column 1.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn shift_keycode(&self) -> Result<Keycode> {
        self.keycode_for(xkeysym::key::Shift_L)
            .map(|resolved| resolved.keycode)
            .ok_or_else(|| anyhow!("no keycode bound to Shift_L on this server"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_names_for_punctuation_resolve_like_the_character() {
        for (name, ch) in ASCII_KEYSYM_NAMES {
            assert_eq!(
                keysym_for_name(name).unwrap(),
                XKeysym::from_char(*ch).raw(),
                "{name}"
            );
        }
        assert_eq!(keysym_for_name("comma").unwrap(), 0x2c);
        assert_eq!(keysym_for_name("Period").unwrap(), 0x2e);
        assert_eq!(keysym_for_name("BRACKETLEFT").unwrap(), 0x5b);
        assert_eq!(keysym_for_name("asciitilde").unwrap(), 0x7e);
        assert_eq!(keysym_for_name("space").unwrap(), 0x20);
        // Existing names stay case-insensitive and exact names keep priority.
        assert_eq!(keysym_for_name("f5").unwrap(), xkeysym::key::F5);
        assert_eq!(keysym_for_name("Return").unwrap(), xkeysym::key::Return);
        assert!(keysym_for_name("nosuchkeyname").is_err());
    }

    #[test]
    fn chord_with_keysym_name_base_parses() {
        let spec = parse_spec("ctrl+comma").unwrap();
        assert_eq!(spec.base, "comma");
        assert_eq!(keysym_for_name(&spec.base).unwrap(), 0x2c);
    }

    #[test]
    fn parses_modifier_chord() {
        let spec = parse_spec("ctrl+shift+tab").unwrap();
        assert_eq!(spec.modifiers, vec!["ctrl", "shift"]);
        assert_eq!(spec.base, "tab");
    }

    #[test]
    fn parses_cmd_as_super_base_last() {
        let spec = parse_spec("cmd+a").unwrap();
        assert_eq!(spec.modifiers, vec!["cmd"]);
        assert_eq!(spec.base, "a");
    }

    #[test]
    fn parses_single_key() {
        let spec = parse_spec("return").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "return");
    }

    #[test]
    fn parses_bare_modifier_as_base() {
        // A lone modifier (e.g. `hold-key --key ctrl`) uses it as the base.
        let spec = parse_spec("ctrl").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "ctrl");
    }

    #[test]
    fn empty_spec_errors() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("+").is_err());
    }

    #[test]
    fn keysym_names_match_cu_table() {
        assert_eq!(keysym_name("return").unwrap(), "Return");
        assert_eq!(keysym_name("Enter").unwrap(), "Return");
        assert_eq!(keysym_name("escape").unwrap(), "Escape");
        assert_eq!(keysym_name("esc").unwrap(), "Escape");
        assert_eq!(keysym_name("page_down").unwrap(), "Next");
        assert_eq!(keysym_name("pagedown").unwrap(), "Next");
        assert_eq!(keysym_name("page_up").unwrap(), "Prior");
        assert_eq!(keysym_name("space").unwrap(), "space");
        assert_eq!(keysym_name("capslock").unwrap(), "Caps_Lock");
    }

    #[test]
    fn keysym_names_map_modifiers() {
        assert_eq!(keysym_name("ctrl").unwrap(), "Control_L");
        assert_eq!(keysym_name("control").unwrap(), "Control_L");
        assert_eq!(keysym_name("cmd").unwrap(), "Super_L");
        assert_eq!(keysym_name("command").unwrap(), "Super_L");
        assert_eq!(keysym_name("meta").unwrap(), "Super_L");
        assert_eq!(keysym_name("super").unwrap(), "Super_L");
        assert_eq!(keysym_name("alt").unwrap(), "Alt_L");
        assert_eq!(keysym_name("shift").unwrap(), "Shift_L");
    }

    #[test]
    fn keysym_names_pass_through_literals() {
        assert_eq!(keysym_name("a").unwrap(), "a");
        assert_eq!(keysym_name("/").unwrap(), "/");
        assert_eq!(keysym_name("F5").unwrap(), "F5");
    }

    #[test]
    fn keysym_values_resolve() {
        assert_eq!(keysym_for_name("return").unwrap(), xkeysym::key::Return);
        assert_eq!(keysym_for_name("tab").unwrap(), xkeysym::key::Tab);
        assert_eq!(keysym_for_name("F5").unwrap(), xkeysym::key::F5);
        assert_eq!(keysym_for_name("ctrl").unwrap(), xkeysym::key::Control_L);
        // Literal ASCII char -> its keysym (ASCII == keysym for basic latin).
        assert_eq!(keysym_for_name("a").unwrap(), u32::from('a'));
        assert_eq!(keysym_for_name("A").unwrap(), u32::from('A'));
        // Exotic glyph resolves to a keysym via xkeysym (é -> eacute 0xe9).
        assert_eq!(keysym_for_name("é").unwrap(), 0x0e9);
    }

    #[test]
    fn is_modifier_recognizes_aliases() {
        for token in [
            "ctrl", "Control", "ALT", "shift", "super", "meta", "cmd", "command",
        ] {
            assert!(is_modifier(token), "{token} should be a modifier");
        }
        assert!(!is_modifier("a"));
        assert!(!is_modifier("tab"));
    }
}
