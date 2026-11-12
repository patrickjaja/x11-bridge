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

/// One key type: the modifiers it looks at and which level each combination
/// selects (combinations not listed select level 1, index 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyType {
    pub mods_mask: u8,
    /// `(mods, level index)` for the active map entries.
    pub entries: Vec<(u8, u8)>,
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
    fn num_groups(&self) -> u8 {
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
        Ok(Self::from_core_rows(min, &reply.keysyms, per, &modmap))
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

    /// Find a key and level modifiers that produce `keysym` in the active
    /// group, preferring no modifier, then Shift, then (with `allow_level3`)
    /// AltGr and AltGr+Shift; the lowest keycode wins within one choice.
    /// Locked modifiers (Caps Lock) are taken into account: this is what
    /// `type` needs to produce the character.
    pub fn find(&self, keysym: Keysym, allow_level3: bool) -> Option<Stroke> {
        self.find_with(keysym, allow_level3, self.locked_mods)
    }

    /// The key of a shortcut: levels 1-2 only and locked modifiers ignored,
    /// so `ctrl+a` stays Ctrl+A with Caps Lock on (a shortcut is matched by
    /// its key, not by the character).
    pub fn find_shortcut(&self, keysym: Keysym) -> Option<Stroke> {
        self.find_with(keysym, false, 0)
    }

    fn find_with(&self, keysym: Keysym, allow_level3: bool, locked: u8) -> Option<Stroke> {
        if keysym == 0 {
            return None;
        }
        let shift = self.shift.map(|m| m.mask);
        let level3 = if allow_level3 {
            self.level3.map(|m| m.mask)
        } else {
            None
        };
        let mut combos: Vec<(bool, bool, u8)> = vec![(false, false, 0)];
        if let Some(s) = shift {
            combos.push((true, false, s));
        }
        if let Some(l3) = level3 {
            combos.push((false, true, l3));
            if let Some(s) = shift {
                combos.push((true, true, s | l3));
            }
        }
        for (want_shift, want_level3, mods) in combos {
            for index in 0..self.keys.len() {
                let keycode = self.min_keycode + index as u8;
                if self.keysym_at(keycode, mods | locked, self.group) == keysym {
                    return Some(Stroke {
                        keycode,
                        shift: want_shift,
                        level3: want_level3,
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
            },
            // 1: FOUR_LEVEL
            KeyType {
                mods_mask: SHIFT_MASK | MOD5,
                entries: vec![(SHIFT_MASK, 1), (MOD5, 2), (SHIFT_MASK | MOD5, 3)],
            },
            // 2: ALPHABETIC (Lock acts like Shift)
            KeyType {
                mods_mask: SHIFT_MASK | LOCK,
                entries: vec![(SHIFT_MASK, 1), (LOCK, 1)],
            },
            // 3: TWO_LEVEL
            KeyType {
                mods_mask: SHIFT_MASK,
                entries: vec![(SHIFT_MASK, 1)],
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
