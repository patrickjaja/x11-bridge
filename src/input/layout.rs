//! The session's keyboard layout as XKB sees it: which key, in the active
//! group, at which shift level, produces a keysym.
//!
//! The core keyboard mapping (`GetKeyboardMapping`) only describes group 1 at
//! levels 1-2 reliably, so `type` used to miss every AltGr character of the
//! layout (on German `@ { [ ] } \ | ~ €`) and, with a second layout active
//! (`us,de` switched to `de`), pressed the group-1 key for a group-2
//! character (`y` came out as `z`). This module reads the XKB map and state
//! instead: the effective group of each key, its key type's level for a
//! modifier set (Lock and NumLock included), and the Shift and
//! ISO_Level3_Shift keys that select levels 2-4.
//!
//! Without the XKB extension it falls back to the core mapping (group 1,
//! levels 1-2), which is what the bridge did before.

use anyhow::{Context, Result};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xkb::{self, ConnectionExt as _, MapPart};
use x11rb::protocol::xproto::{ConnectionExt as _, Keycode, Keysym};

use crate::conn::Conn;

/// Real-modifier bit for Shift.
pub const SHIFT_MASK: u8 = 0x01;
/// Real-modifier bit for Lock (Caps Lock).
pub const LOCK_MASK: u8 = 0x02;

/// The character a keysym stands for.
pub fn keysym_char(keysym: Keysym) -> Option<char> {
    if keysym == 0 {
        return None;
    }
    xkeysym::Keysym::new(keysym).key_char()
}

fn keysym_of(ch: char) -> Keysym {
    xkeysym::Keysym::from_char(ch).raw()
}

/// The capital of a keysym's character (itself if it has none, or if the
/// capital is not a single character, like `ß` -> `SS`).
pub fn keysym_upper(keysym: Keysym) -> Keysym {
    case_map(keysym, |c| c.to_uppercase().collect())
}

/// The small letter of a keysym's character (itself if it has none).
pub fn keysym_lower(keysym: Keysym) -> Keysym {
    case_map(keysym, |c| c.to_lowercase().collect())
}

fn case_map(keysym: Keysym, map: impl Fn(char) -> Vec<char>) -> Keysym {
    let Some(ch) = keysym_char(keysym) else {
        return keysym;
    };
    // `ß` has the single capital `ẞ` in X (Xlib upper-cases it so under Caps
    // Lock), not the two letters `SS` of Unicode's full mapping.
    if ch == 'ß' {
        let up = map(ch);
        if up.len() > 1 {
            return keysym_of('ẞ');
        }
    }
    match map(ch).as_slice() {
        [single] if *single != ch => {
            let mapped = keysym_of(*single);
            if mapped == 0 { keysym } else { mapped }
        }
        _ => keysym,
    }
}

/// Two keysyms that stand for the same character (`U00E9` and `eacute`).
pub fn same_symbol(a: Keysym, b: Keysym) -> bool {
    a == b || (a != 0 && b != 0 && keysym_char(a).is_some() && keysym_char(a) == keysym_char(b))
}

/// One key type: the modifiers it looks at and which level each combination
/// selects (combinations not listed select level 1, index 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyType {
    pub mods_mask: u8,
    /// `(mods, level index)` for the active map entries.
    pub entries: Vec<(u8, u8)>,
    /// `(entry mods, preserved mods)`: modifiers an entry leaves unconsumed
    /// (FOUR_LEVEL_SEMIALPHABETIC keeps Lock at AltGr levels, so clients
    /// upper-case `µ` under Caps Lock).
    pub preserve: Vec<(u8, u8)>,
}

impl KeyType {
    /// The level index `mods` selects on a key of this type.
    pub fn level(&self, mods: u8) -> u8 {
        let masked = mods & self.mods_mask;
        self.entries
            .iter()
            .find(|&&(m, _)| m == masked)
            .map_or(0, |&(_, level)| level)
    }

    /// The modifiers a key of this type consumes when `mods` are down.
    pub fn consumed(&self, mods: u8) -> u8 {
        let masked = mods & self.mods_mask;
        let kept = self
            .preserve
            .iter()
            .find(|&&(m, _)| m == masked)
            .map_or(0, |&(_, p)| p);
        self.mods_mask & !kept
    }
}

/// The keysyms of one key: per group, its key type and its levels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// XKB group info byte: low nibble = number of groups, bits 6-7 = what an
    /// out-of-range group does (wrap / clamp / redirect to bits 4-5).
    pub group_info: u8,
    /// Key type index per group.
    pub types: Vec<u8>,
    /// Levels per group.
    pub width: u8,
    /// `groups * width` keysyms, group-major.
    pub syms: Vec<Keysym>,
}

impl Key {
    pub fn num_groups(&self) -> u8 {
        self.group_info & 0x0f
    }

    /// The group this key uses while `group` is the effective group.
    pub fn effective_group(&self, group: u8) -> Option<u8> {
        let n = self.num_groups();
        if n == 0 {
            return None;
        }
        if group < n {
            return Some(group);
        }
        Some(match self.group_info & 0xc0 {
            0x40 => n - 1,
            0x80 => {
                let redirect = (self.group_info >> 4) & 0x03;
                if redirect < n { redirect } else { 0 }
            }
            _ => group % n,
        })
    }
}

/// A modifier key the bridge can press: its keycode and the real modifier it
/// sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModKey {
    pub keycode: Keycode,
    pub mask: u8,
}

/// How to produce a keysym from the layout: the key and the level modifiers
/// to hold while tapping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub keycode: Keycode,
    pub shift: bool,
    pub level3: bool,
}

/// The layout snapshot `type`, `key-sequence` and `hold-key` resolve against.
#[derive(Debug, Clone)]
pub struct Layout {
    pub min_keycode: Keycode,
    /// Indexed by `keycode - min_keycode`.
    pub keys: Vec<Key>,
    pub types: Vec<KeyType>,
    /// Effective keyboard group (0-based).
    pub group: u8,
    /// Locked modifiers (Caps Lock, Num Lock) that are on while we type.
    pub locked_mods: u8,
    pub shift: Option<ModKey>,
    pub level3: Option<ModKey>,
}

impl Layout {
    /// Read the layout from the server: XKB when available, else the core map.
    pub fn fetch(conn: &Conn) -> Result<Self> {
        match Self::fetch_xkb(conn) {
            Ok(Some(layout)) => Ok(layout),
            _ => Self::fetch_core(conn),
        }
    }

    fn fetch_xkb(conn: &Conn) -> Result<Option<Self>> {
        let c = &conn.conn;
        if c.extension_information(xkb::X11_EXTENSION_NAME)?.is_none() {
            return Ok(None);
        }
        let use_ext = c.xkb_use_extension(1, 0)?.reply()?;
        if !use_ext.supported {
            return Ok(None);
        }
        let device: xkb::DeviceSpec = xkb::ID::USE_CORE_KBD.into();
        let state = c.xkb_get_state(device)?.reply()?;
        let setup = c.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let n_keys = max - min + 1;
        let parts = MapPart::KEY_TYPES | MapPart::KEY_SYMS | MapPart::MODIFIER_MAP;
        let reply = c
            .xkb_get_map(
                device,
                parts,
                0u16.into(),
                0,
                0,
                min,
                n_keys,
                0,
                0,
                0,
                0,
                0u16.into(),
                0,
                0,
                min,
                n_keys,
                0,
                0,
            )?
            .reply()
            .context("failed to read the XKB keyboard map")?;

        let types = reply
            .map
            .types_rtrn
            .unwrap_or_default()
            .iter()
            .map(|t| KeyType {
                mods_mask: u16::from(t.mods_mask) as u8,
                entries: t
                    .map
                    .iter()
                    .filter(|e| e.active)
                    .map(|e| (u16::from(e.mods_mask) as u8, e.level))
                    .collect(),
                preserve: if t.has_preserve {
                    t.map
                        .iter()
                        .zip(&t.preserve)
                        .filter(|(e, _)| e.active)
                        .map(|(e, p)| (u16::from(e.mods_mask) as u8, u16::from(p.mask) as u8))
                        .collect()
                } else {
                    Vec::new()
                },
            })
            .collect();
        let mut keys: Vec<Key> = reply
            .map
            .syms_rtrn
            .unwrap_or_default()
            .iter()
            .map(|k| Key {
                group_info: k.group_info,
                types: k.kt_index.to_vec(),
                width: k.width,
                syms: k.syms.clone(),
            })
            .collect();
        keys.resize(
            usize::from(n_keys),
            Key {
                group_info: 0,
                types: vec![0; 4],
                width: 0,
                syms: Vec::new(),
            },
        );
        let mut modmap = vec![0u8; usize::from(n_keys)];
        for entry in reply.map.modmap_rtrn.unwrap_or_default() {
            if let Some(slot) = entry
                .keycode
                .checked_sub(min)
                .and_then(|i| modmap.get_mut(usize::from(i)))
            {
                *slot = u16::from(entry.mods) as u8;
            }
        }
        let mut layout = Layout {
            min_keycode: min,
            keys,
            types,
            group: u8::from(state.group),
            locked_mods: u16::from(state.locked_mods) as u8,
            shift: None,
            level3: None,
        };
        layout.find_mod_keys(&modmap);
        Ok(Some(layout))
    }

    /// Core-protocol fallback: group 1, levels 1-2, Shift selects level 2.
    fn fetch_core(conn: &Conn) -> Result<Self> {
        let c = &conn.conn;
        let setup = c.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let n_keys = max - min + 1;
        let reply = c
            .get_keyboard_mapping(min, n_keys)?
            .reply()
            .context("failed to read keyboard mapping reply")?;
        let per = usize::from(reply.keysyms_per_keycode);
        let modifiers = c.get_modifier_mapping()?.reply()?;
        let mut modmap = vec![0u8; usize::from(n_keys)];
        let per_mod = usize::from(modifiers.keycodes_per_modifier());
        for (index, &keycode) in modifiers.keycodes.iter().enumerate() {
            if per_mod == 0 || keycode < min {
                continue;
            }
            if let Some(slot) = modmap.get_mut(usize::from(keycode - min)) {
                *slot |= 1 << (index / per_mod);
            }
        }
        let mut layout = Self::from_core_rows(min, &reply.keysyms, per, &modmap);
        // Without XKB the core pointer state carries Caps Lock.
        let pointer = c.query_pointer(conn.root)?.reply()?;
        layout.locked_mods = (u16::from(pointer.mask) as u8) & LOCK_MASK;
        Ok(layout)
    }

    /// Build a group-1, two-level layout from core keyboard-mapping rows.
    pub fn from_core_rows(min: Keycode, rows: &[Keysym], per: usize, modmap: &[u8]) -> Self {
        let keys = rows
            .chunks(per.max(1))
            .map(|row| {
                let syms: Vec<Keysym> = row.iter().take(2).copied().collect();
                let empty = syms.iter().all(|&s| s == 0);
                Key {
                    group_info: if empty { 0 } else { 1 },
                    types: vec![0; 4],
                    width: syms.len() as u8,
                    syms,
                }
            })
            .collect();
        let mut layout = Layout {
            min_keycode: min,
            keys,
            types: vec![KeyType {
                mods_mask: SHIFT_MASK,
                entries: vec![(SHIFT_MASK, 1)],
                preserve: vec![],
            }],
            group: 0,
            locked_mods: 0,
            shift: None,
            level3: None,
        };
        layout.find_mod_keys(modmap);
        layout
    }

    /// Pick the Shift and ISO_Level3_Shift keys: a key whose level-1 keysym is
    /// the modifier and which the modifier map binds to a real modifier.
    fn find_mod_keys(&mut self, modmap: &[u8]) {
        let pick = |layout: &Layout, keysyms: &[u32]| -> Option<ModKey> {
            for &want in keysyms {
                for (index, key) in layout.keys.iter().enumerate() {
                    let mask = modmap.get(index).copied().unwrap_or(0);
                    if mask == 0 {
                        continue;
                    }
                    let Some(group) = key.effective_group(layout.group) else {
                        continue;
                    };
                    if key.syms.get(usize::from(group) * usize::from(key.width)) == Some(&want) {
                        return Some(ModKey {
                            keycode: layout.min_keycode + index as u8,
                            mask,
                        });
                    }
                }
            }
            None
        };
        self.shift = pick(self, &[xkeysym::key::Shift_L, xkeysym::key::Shift_R]);
        self.level3 = pick(self, &[xkeysym::key::ISO_Level3_Shift]);
    }

    /// The keysym `keycode` produces with `mods` held in `group` (what an
    /// XKB client reads from a key event).
    pub fn keysym_at(&self, keycode: Keycode, mods: u8, group: u8) -> Keysym {
        let Some(key) = keycode
            .checked_sub(self.min_keycode)
            .and_then(|i| self.keys.get(usize::from(i)))
        else {
            return 0;
        };
        let Some(g) = key.effective_group(group) else {
            return 0;
        };
        let level = self
            .types
            .get(usize::from(
                key.types.get(usize::from(g)).copied().unwrap_or(0),
            ))
            .map_or(0, |t| t.level(mods));
        if level >= key.width {
            return 0;
        }
        key.syms
            .get(usize::from(g) * usize::from(key.width) + usize::from(level))
            .copied()
            .unwrap_or(0)
    }

    /// What a client reads from `keycode` with `mods` in `group`: the level's
    /// keysym, upper-cased when Lock is on and the key type does not use Lock
    /// (the XKB "Lock not consumed" rule that xkbcommon and Xlib apply).
    pub fn produced(&self, keycode: Keycode, mods: u8, group: u8) -> Keysym {
        let keysym = self.keysym_at(keycode, mods, group);
        if mods & LOCK_MASK == 0 || keysym == 0 {
            return keysym;
        }
        let consumes_lock = keycode
            .checked_sub(self.min_keycode)
            .and_then(|i| self.keys.get(usize::from(i)))
            .and_then(|key| {
                let g = key.effective_group(group)?;
                let index = key.types.get(usize::from(g)).copied().unwrap_or(0);
                self.types.get(usize::from(index))
            })
            .is_some_and(|t| t.consumed(mods) & LOCK_MASK != 0);
        if consumes_lock {
            keysym
        } else {
            keysym_upper(keysym)
        }
    }

    /// The key type of `keycode` in the active group.
    pub fn key_type(&self, keycode: Keycode) -> Option<&KeyType> {
        let key = self
            .keys
            .get(usize::from(keycode.checked_sub(self.min_keycode)?))?;
        let g = key.effective_group(self.group)?;
        self.types.get(usize::from(
            key.types.get(usize::from(g)).copied().unwrap_or(0),
        ))
    }

    /// Index of a two-level type whose level 2 is chosen by Shift or by Lock
    /// (ALPHABETIC in every standard keymap).
    pub fn alphabetic_type(&self) -> Option<u8> {
        self.types
            .iter()
            .position(|t| {
                t.mods_mask == SHIFT_MASK | LOCK_MASK
                    && t.entries.contains(&(SHIFT_MASK, 1))
                    && t.entries.contains(&(LOCK_MASK, 1))
                    && !t.entries.contains(&(SHIFT_MASK | LOCK_MASK, 1))
            })
            .map(|i| i as u8)
    }

    /// Find a key and level modifiers that produce `keysym` in the active
    /// group with the locked modifiers (Caps Lock) as they are: what `type`
    /// needs to produce the character. Keys are tried in keycode order, each
    /// at levels 1 and 2 (Shift), so the main block wins over the keypad;
    /// AltGr levels (with `allow_level3`) only when no key has it at levels
    /// 1-2.
    pub fn find(&self, keysym: Keysym, allow_level3: bool) -> Option<Stroke> {
        if keysym == 0 {
            return None;
        }
        let shift = self.shift.map(|m| m.mask);
        let level3 = self.level3.map(|m| m.mask).filter(|_| allow_level3);
        let mut passes: Vec<Vec<(bool, bool, u8)>> = vec![vec![(false, false, 0)]];
        if let Some(s) = shift {
            passes[0].push((true, false, s));
        }
        if let Some(l3) = level3 {
            let mut altgr = vec![(false, true, l3)];
            if let Some(s) = shift {
                altgr.push((true, true, s | l3));
            }
            passes.push(altgr);
        }
        for combos in passes {
            for index in 0..self.keys.len() {
                let keycode = self.min_keycode + index as u8;
                for &(want_shift, want_level3, mods) in &combos {
                    let got = self.produced(keycode, mods | self.locked_mods, self.group);
                    if same_symbol(got, keysym) {
                        return Some(Stroke {
                            keycode,
                            shift: want_shift,
                            level3: want_level3,
                        });
                    }
                }
            }
        }
        None
    }

    /// The physical key of a shortcut: the key that carries `keysym` at level
    /// 1 or 2 in the active group, else in another group (Ctrl+C on an active
    /// Russian layout presses the key that is C in the Latin layout, as a
    /// user does). Locked modifiers are ignored, so `ctrl+a` stays Ctrl+A with
    /// Caps Lock on; AltGr is never used.
    pub fn find_shortcut(&self, keysym: Keysym) -> Option<Stroke> {
        if keysym == 0 {
            return None;
        }
        let mut groups = vec![self.group];
        groups.extend((0..4u8).filter(|&g| g != self.group));
        let shift = self.shift.map(|m| m.mask);
        for group in groups {
            for index in 0..self.keys.len() {
                let keycode = self.min_keycode + index as u8;
                let key = &self.keys[index];
                if group != self.group && group >= key.num_groups() {
                    continue;
                }
                if same_symbol(self.keysym_at(keycode, 0, group), keysym) {
                    return Some(Stroke {
                        keycode,
                        shift: false,
                        level3: false,
                    });
                }
                if let Some(s) = shift
                    && same_symbol(self.keysym_at(keycode, s, group), keysym)
                {
                    return Some(Stroke {
                        keycode,
                        shift: true,
                        level3: false,
                    });
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkeysym::key;

    const LOCK: u8 = 0x02;
    const MOD5: u8 = 0x80;

    /// A miniature `us,de`-like keymap: keycode 10 is `q Q @` (de has `@` at
    /// AltGr+q), 11 is `y Y` in group 1 and `z Z` in group 2, 12 is Shift_L,
    /// 13 is ISO_Level3_Shift, 14 is an alphabetic `a A`.
    fn layout(group: u8, locked: u8) -> Layout {
        let types = vec![
            // 0: ONE_LEVEL
            KeyType {
                mods_mask: 0,
                entries: vec![],
                preserve: vec![],
            },
            // 1: FOUR_LEVEL
            KeyType {
                mods_mask: SHIFT_MASK | MOD5,
                entries: vec![(SHIFT_MASK, 1), (MOD5, 2), (SHIFT_MASK | MOD5, 3)],
                preserve: vec![],
            },
            // 2: ALPHABETIC (Lock acts like Shift)
            KeyType {
                mods_mask: SHIFT_MASK | LOCK,
                entries: vec![(SHIFT_MASK, 1), (LOCK, 1)],
                preserve: vec![],
            },
            // 3: TWO_LEVEL
            KeyType {
                mods_mask: SHIFT_MASK,
                entries: vec![(SHIFT_MASK, 1)],
                preserve: vec![],
            },
        ];
        let keys = vec![
            Key {
                group_info: 1,
                types: vec![1, 0, 0, 0],
                width: 4,
                syms: vec![key::q, key::Q, key::at, key::Greek_OMEGA],
            },
            Key {
                group_info: 2,
                types: vec![3, 3, 0, 0],
                width: 2,
                syms: vec![key::y, key::Y, key::z, key::Z],
            },
            Key {
                group_info: 1,
                types: vec![0, 0, 0, 0],
                width: 1,
                syms: vec![key::Shift_L],
            },
            Key {
                group_info: 1,
                types: vec![0, 0, 0, 0],
                width: 1,
                syms: vec![key::ISO_Level3_Shift],
            },
            Key {
                group_info: 1,
                types: vec![2, 0, 0, 0],
                width: 2,
                syms: vec![key::a, key::A],
            },
        ];
        let mut l = Layout {
            min_keycode: 10,
            keys,
            types,
            group,
            locked_mods: locked,
            shift: None,
            level3: None,
        };
        l.find_mod_keys(&[0, 0, SHIFT_MASK, MOD5, 0]);
        l
    }

    #[test]
    fn modifier_keys_come_from_the_modifier_map() {
        let l = layout(0, 0);
        assert_eq!(
            l.shift,
            Some(ModKey {
                keycode: 12,
                mask: SHIFT_MASK
            })
        );
        assert_eq!(
            l.level3,
            Some(ModKey {
                keycode: 13,
                mask: MOD5
            })
        );
    }

    #[test]
    fn altgr_levels_are_used_for_type_only() {
        let l = layout(0, 0);
        let at = Stroke {
            keycode: 10,
            shift: false,
            level3: true,
        };
        assert_eq!(l.find(key::at, true), Some(at));
        assert_eq!(l.find(key::at, false), None);
        assert_eq!(
            l.find(key::Greek_OMEGA, true),
            Some(Stroke {
                keycode: 10,
                shift: true,
                level3: true
            })
        );
        assert_eq!(
            l.find(key::Q, true),
            Some(Stroke {
                keycode: 10,
                shift: true,
                level3: false
            })
        );
    }

    #[test]
    fn the_active_group_decides_the_key() {
        let us = layout(0, 0);
        assert_eq!(us.find(key::y, true).map(|s| s.keycode), Some(11));
        assert_eq!(us.find(key::z, true), None);
        let de = layout(1, 0);
        assert_eq!(
            de.find(key::z, true),
            Some(Stroke {
                keycode: 11,
                shift: false,
                level3: false
            })
        );
        assert_eq!(de.find(key::y, true), None);
        // Keys with one group wrap: `q` is still reachable in group 2.
        assert_eq!(de.find(key::q, true).map(|s| s.keycode), Some(10));
    }

    #[test]
    fn caps_lock_flips_the_shift_choice_on_alphabetic_keys() {
        let caps = layout(0, LOCK);
        assert_eq!(
            caps.find(key::a, true),
            Some(Stroke {
                keycode: 14,
                shift: true,
                level3: false
            })
        );
        // A shortcut keeps the plain key whatever Caps Lock says.
        assert_eq!(
            caps.find_shortcut(key::a),
            Some(Stroke {
                keycode: 14,
                shift: false,
                level3: false
            })
        );
        assert_eq!(
            caps.find(key::A, true),
            Some(Stroke {
                keycode: 14,
                shift: false,
                level3: false
            })
        );
    }

    #[test]
    fn caps_lock_upper_cases_keys_whose_type_ignores_lock() {
        // Keycode 15: a TWO_LEVEL key carrying `é É` (what the server makes of
        // a Unicode pair). Caps Lock is not consumed, so clients upper-case:
        // `é` cannot be typed, `É` comes with or without Shift.
        let mut l = layout(0, LOCK);
        l.keys.push(Key {
            group_info: 1,
            types: vec![3, 0, 0, 0],
            width: 2,
            syms: vec![key::eacute, key::Eacute],
        });
        assert_eq!(l.produced(15, LOCK, 0), key::Eacute);
        assert_eq!(l.produced(15, LOCK | SHIFT_MASK, 0), key::Eacute);
        assert_eq!(l.find(key::eacute, true), None);
        assert_eq!(l.find(key::Eacute, true).map(|s| s.keycode), Some(15));
        // The same key as ALPHABETIC (type 2) consumes Lock.
        l.keys[5].types[0] = 2;
        assert_eq!(
            l.find(key::eacute, true),
            Some(Stroke {
                keycode: 15,
                shift: true,
                level3: false
            })
        );
        assert_eq!(l.alphabetic_type(), Some(2));
    }

    #[test]
    fn a_preserved_lock_is_not_consumed() {
        // FOUR_LEVEL_SEMIALPHABETIC-like: Lock picks level 2 at levels 1-2,
        // but at AltGr levels it is preserved, so clients upper-case `µ`.
        let t = KeyType {
            mods_mask: SHIFT_MASK | LOCK | MOD5,
            entries: vec![(SHIFT_MASK, 1), (LOCK, 1), (MOD5, 2), (MOD5 | LOCK, 2)],
            preserve: vec![(MOD5 | LOCK, LOCK)],
        };
        assert_eq!(t.consumed(LOCK) & LOCK, LOCK);
        assert_eq!(t.consumed(LOCK | MOD5) & LOCK, 0);
        let mut l = layout(0, LOCK);
        l.types.push(t);
        l.keys.push(Key {
            group_info: 1,
            types: vec![4, 0, 0, 0],
            width: 4,
            syms: vec![key::m, key::M, key::mu, 0],
        });
        assert_eq!(l.produced(15, LOCK | MOD5, 0), key::Greek_MU);
        assert_eq!(l.find(key::mu, true), None);
    }

    #[test]
    fn keys_are_tried_in_keycode_order_before_altgr() {
        // `Q` (Shift on keycode 10) is found before an unshifted `Q` on a
        // higher keycode, like `(` on the main block before the keypad's.
        let mut l = layout(0, 0);
        l.keys.push(Key {
            group_info: 1,
            types: vec![0, 0, 0, 0],
            width: 1,
            syms: vec![key::Q],
        });
        assert_eq!(
            l.find(key::Q, true),
            Some(Stroke {
                keycode: 10,
                shift: true,
                level3: false
            })
        );
    }

    #[test]
    fn shortcut_falls_back_to_another_group() {
        // Group 2 active (`z Z` on keycode 11): `y` is no key of it, but the
        // key that is `y` in group 1 is pressed (Ctrl+Y on a Russian layout
        // presses the Latin key, as a user does).
        let l = layout(1, 0);
        assert_eq!(l.find(key::y, true), None);
        assert_eq!(l.find_shortcut(key::y).map(|s| s.keycode), Some(11));
        // A character of the active group still wins.
        assert_eq!(l.find_shortcut(key::z).map(|s| s.keycode), Some(11));
    }

    #[test]
    fn unicode_and_legacy_keysyms_are_the_same_symbol() {
        assert!(same_symbol(key::eacute, 0x10000e9));
        assert!(!same_symbol(key::eacute, key::Eacute));
        assert_eq!(keysym_upper(key::ssharp), 0x1001e9e);
        assert_eq!(keysym_upper(key::_1), key::_1);
    }

    #[test]
    fn group_wrap_clamp_and_redirect() {
        let mut k = Key {
            group_info: 2,
            types: vec![0; 4],
            width: 1,
            syms: vec![1, 2],
        };
        assert_eq!(k.effective_group(3), Some(1));
        k.group_info = 2 | 0x40;
        assert_eq!(k.effective_group(3), Some(1));
        k.group_info = 2 | 0x80;
        assert_eq!(k.effective_group(3), Some(0));
        k.group_info = 0;
        assert_eq!(k.effective_group(0), None);
    }

    #[test]
    fn core_fallback_is_group_one_two_levels() {
        // rows of 4: `a A a A`, `Shift_L`, empty
        let rows = [
            key::a,
            key::A,
            key::a,
            key::A,
            key::Shift_L,
            0,
            key::Shift_L,
            0,
            0,
            0,
            0,
            0,
        ];
        let l = Layout::from_core_rows(8, &rows, 4, &[0, SHIFT_MASK, 0]);
        assert_eq!(l.shift.map(|m| m.keycode), Some(9));
        assert_eq!(
            l.find(key::A, true),
            Some(Stroke {
                keycode: 8,
                shift: true,
                level3: false
            })
        );
        assert_eq!(l.find(key::b, true), None);
    }
}
