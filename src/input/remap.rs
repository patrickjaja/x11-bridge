//! Characters the layout cannot produce: bound to spare keycodes for a while.
//!
//! X clients translate a key event with the keymap they hold when they process
//! it, and they fetch a changed keymap only when they process the
//! `MappingNotify` for it. A keycode remapped for one character and restored
//! right after the key (what this bridge and xdotool did) is therefore read
//! with the restored map by any client that gets to the event after the
//! restore: measured under Xvfb, GTK, urxvt and Chrome lost every such
//! character, and with no delay between characters they typed the previous
//! one instead.
//!
//! So a text's off-layout characters are bound in pages: every spare keycode
//! (one with no keysym at all, which no physical key produces) takes one
//! keysym, the page's characters are typed, and the keycodes change again
//! only after [`SETTLE_MS`] - for the next page, or for the restore at the
//! end. The pause is paid once per page, not per character.
//!
//! The original rows are journaled in a root-window property before the first
//! change, so a bridge killed mid-text (SIGKILL, or a timeout) leaves a record
//! the next keyboard command uses to restore them ([`recover_stale`]).

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Keycode, Keysym, PropMode};
use x11rb::wrapper::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::KeyboardMap;

/// Pause before a remapped keycode changes again, so every client has
/// translated the keys typed with it. Chrome under load needed more than
/// 20 ms in the Xvfb experiments; GTK and urxvt needed 1 ms.
pub const SETTLE_MS: u64 = 200;

/// Root-window property holding the original rows of remapped keycodes while
/// a command has them bound: `[1, pid, keysyms_per_keycode, (keycode,
/// row...)...]`, CARDINAL/32.
const JOURNAL_ATOM: &[u8] = b"_X11_BRIDGE_KEYMAP_RESTORE";
const JOURNAL_VERSION: u32 = 1;

/// Split a text into pages: `chars[i]` is the keysym a character needs bound
/// to a spare keycode, or `None` when the layout types it. Returns each page's
/// first character index and the distinct keysyms it binds (at most
/// `capacity`). Characters before the first page need no binding.
pub fn plan_pages(chars: &[Option<Keysym>], capacity: usize) -> Vec<(usize, Vec<Keysym>)> {
    let capacity = capacity.max(1);
    let mut pages: Vec<(usize, Vec<Keysym>)> = Vec::new();
    for (index, sym) in chars.iter().enumerate() {
        let Some(sym) = *sym else { continue };
        match pages.last_mut() {
            Some((_, syms)) if syms.contains(&sym) => {}
            Some((_, syms)) if syms.len() < capacity => syms.push(sym),
            _ => pages.push((index, vec![sym])),
        }
    }
    pages
}

struct Slot {
    keycode: Keycode,
    original: Vec<Keysym>,
    bound: Option<Keysym>,
}

/// The spare keycodes of one command and what they are bound to.
pub struct Remapper<'a> {
    conn: &'a Conn,
    per: u8,
    slots: Vec<Slot>,
    /// A key was typed on a bound keycode since its last change.
    used: bool,
    journal: Option<u32>,
    settle: Duration,
}

impl<'a> Remapper<'a> {
    pub fn new(conn: &'a Conn, map: &KeyboardMap) -> Self {
        let slots = map
            .spare_keycodes()
            .into_iter()
            .map(|(keycode, original)| Slot {
                keycode,
                original,
                bound: None,
            })
            .collect();
        Self {
            conn,
            per: map.keysyms_per_keycode() as u8,
            slots,
            used: false,
            journal: None,
            settle: Duration::from_millis(SETTLE_MS),
        }
    }

    /// How many keysyms one page can bind.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The keycode `keysym` is bound to right now.
    pub fn keycode_of(&self, keysym: Keysym) -> Option<Keycode> {
        self.slots
            .iter()
            .find(|s| s.bound == Some(keysym))
            .map(|s| s.keycode)
    }

    /// Record that a key was typed on a bound keycode.
    pub fn mark_used(&mut self) {
        self.used = true;
    }

    /// Bind every keysym of `page` (at most [`capacity`](Self::capacity)),
    /// keeping keycodes that already carry one of them.
    pub fn load(&mut self, page: &[Keysym]) -> Result<()> {
        let mut changes: Vec<(usize, Keysym)> = Vec::new();
        let mut taken: Vec<bool> = self
            .slots
            .iter()
            .map(|s| s.bound.is_some_and(|b| page.contains(&b)))
            .collect();
        for &sym in page {
            if self.keycode_of(sym).is_some() || changes.iter().any(|&(_, s)| s == sym) {
                continue;
            }
            let Some(free) = taken.iter().position(|&t| !t) else {
                anyhow::bail!("more characters in one page than spare keycodes");
            };
            taken[free] = true;
            changes.push((free, sym));
        }
        if changes.is_empty() {
            return Ok(());
        }
        if self.used {
            thread::sleep(self.settle);
        }
        self.write_journal()?;
        for &(index, sym) in &changes {
            let row = vec![sym; usize::from(self.per.max(1))];
            self.conn
                .conn
                .change_keyboard_mapping(1, self.slots[index].keycode, self.per.max(1), &row)
                .context("failed to bind a spare keycode")?;
            self.slots[index].bound = Some(sym);
        }
        self.conn.sync()?;
        self.used = false;
        Ok(())
    }

    /// Restore every bound keycode (after the settle pause when keys were
    /// typed on them) and drop the journal.
    pub fn finish(&mut self) -> Result<()> {
        if self.slots.iter().all(|s| s.bound.is_none()) {
            return Ok(());
        }
        if self.used {
            thread::sleep(self.settle);
            self.used = false;
        }
        for slot in &mut self.slots {
            if slot.bound.take().is_some() {
                self.conn
                    .conn
                    .change_keyboard_mapping(1, slot.keycode, self.per.max(1), &slot.original)
                    .context("failed to restore a remapped keycode")?;
            }
        }
        if let Some(atom) = self.journal.take() {
            self.conn.conn.delete_property(self.conn.root, atom)?;
        }
        self.conn.sync()
    }

    fn write_journal(&mut self) -> Result<()> {
        if self.journal.is_some() {
            return Ok(());
        }
        let atom = intern(self.conn)?;
        let mut data = vec![JOURNAL_VERSION, std::process::id(), u32::from(self.per)];
        for slot in &self.slots {
            data.push(u32::from(slot.keycode));
            data.extend(slot.original.iter().copied());
        }
        self.conn.conn.change_property32(
            PropMode::REPLACE,
            self.conn.root,
            atom,
            AtomEnum::CARDINAL,
            &data,
        )?;
        self.journal = Some(atom);
        Ok(())
    }
}

impl Drop for Remapper<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn intern(conn: &Conn) -> Result<u32> {
    Ok(conn.conn.intern_atom(false, JOURNAL_ATOM)?.reply()?.atom)
}

/// Is `pid` a running x11-bridge (another command still typing)?
fn bridge_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim() == "x11-bridge")
        .unwrap_or(false)
}

/// A parsed journal: `(pid, keysyms per keycode, [(keycode, original row)])`.
type Journal = (u32, usize, Vec<(Keycode, Vec<Keysym>)>);

/// Parse a journal.
pub fn parse_journal(data: &[u32]) -> Option<Journal> {
    let (&version, rest) = data.split_first()?;
    if version != JOURNAL_VERSION || rest.len() < 2 {
        return None;
    }
    let (pid, per) = (rest[0], rest[1] as usize);
    if per == 0 {
        return None;
    }
    let entries = rest[2..].chunks(per + 1);
    let mut out = Vec::new();
    for entry in entries {
        if entry.len() != per + 1 || entry[0] > 255 {
            return None;
        }
        out.push((entry[0] as Keycode, entry[1..].to_vec()));
    }
    Some((pid, per, out))
}

/// A row this bridge wrote: one keysym, repeated (the server may read back
/// some columns empty after XKB re-derives the key from the core row).
pub fn is_bridge_row(row: &[Keysym]) -> bool {
    row.first()
        .is_some_and(|&first| first != 0 && row.iter().all(|&s| s == first || s == 0))
}

/// Undo the remaps of a bridge that died mid-text: restore every journaled
/// keycode that still carries a bridge row (a layout loaded since is left
/// alone), then drop the journal. A journal of a bridge that is still running
/// is left for that bridge.
pub fn recover_stale(conn: &Conn) -> Result<()> {
    let atom = intern(conn)?;
    let reply = conn
        .conn
        .get_property(false, conn.root, atom, AtomEnum::CARDINAL, 0, 4096)?
        .reply()?;
    let Some(data) = reply.value32() else {
        return Ok(());
    };
    let data: Vec<u32> = data.collect();
    if data.is_empty() {
        return Ok(());
    }
    if let Some((pid, per, entries)) = parse_journal(&data) {
        if pid != std::process::id() && bridge_alive(pid) {
            return Ok(());
        }
        for (keycode, original) in entries {
            let current = conn
                .conn
                .get_keyboard_mapping(keycode, 1)?
                .reply()
                .context("failed to read a journaled keycode")?;
            if usize::from(current.keysyms_per_keycode) == per
                && is_bridge_row(&current.keysyms)
                && original.len() == per
            {
                conn.conn
                    .change_keyboard_mapping(1, keycode, per as u8, &original)?;
            }
        }
    }
    conn.conn.delete_property(conn.root, atom)?;
    conn.sync()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_hold_at_most_capacity_distinct_keysyms() {
        let chars = [
            None,
            Some(1),
            Some(2),
            Some(1),
            None,
            Some(3),
            Some(2),
            Some(4),
        ];
        assert_eq!(
            plan_pages(&chars, 3),
            vec![(1, vec![1, 2, 3]), (7, vec![4])]
        );
        assert_eq!(
            plan_pages(&chars, 1),
            vec![
                (1, vec![1]),
                (2, vec![2]),
                (3, vec![1]),
                (5, vec![3]),
                (6, vec![2]),
                (7, vec![4])
            ]
        );
    }

    #[test]
    fn layout_only_text_has_no_pages() {
        assert!(plan_pages(&[None, None], 4).is_empty());
        assert!(plan_pages(&[], 4).is_empty());
    }

    #[test]
    fn zero_capacity_still_pages_one_by_one() {
        assert_eq!(plan_pages(&[Some(7), Some(7)], 0), vec![(0, vec![7])]);
    }

    #[test]
    fn journal_round_trip_and_rejects() {
        let data = [1, 42, 2, 8, 0, 0, 97, 5, 6];
        assert_eq!(
            parse_journal(&data),
            Some((42, 2, vec![(8, vec![0, 0]), (97, vec![5, 6])]))
        );
        assert_eq!(parse_journal(&[2, 42, 2]), None);
        assert_eq!(parse_journal(&[1, 42, 2, 8, 0]), None);
        assert_eq!(parse_journal(&[1, 42, 0]), None);
        assert_eq!(parse_journal(&[]), None);
    }

    #[test]
    fn bridge_rows_are_uniform_and_non_empty() {
        assert!(is_bridge_row(&[0xe9, 0xe9, 0xe9]));
        assert!(is_bridge_row(&[0xe9, 0xe9, 0xe9, 0xe9, 0, 0, 0]));
        assert!(!is_bridge_row(&[0, 0, 0]));
        assert!(!is_bridge_row(&[0, 0xe9]));
        assert!(!is_bridge_row(&[0xff1b, 0x61, 0xff1b]));
        assert!(!is_bridge_row(&[]));
    }
}
