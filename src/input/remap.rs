//! Characters the layout cannot produce: bound to spare keycodes for a while.
//!
//! X clients translate a key event with the keymap they hold when they process
//! it, and they fetch a changed keymap only when they process the
//! notification of the change. A keycode remapped for one character and
//! restored right after the key (what this bridge and xdotool did) is
//! therefore read with the restored map by any client that gets to the event
//! after the restore: measured under Xvfb, GTK, urxvt and Chrome lost every
//! such character, and with no delay between characters they typed the
//! previous one instead.
//!
//! So a text's off-layout characters are bound in pages to the spare keycodes
//! (keycodes with no keysym at all, which no physical key produces). A
//! character with a case pair takes a keycode as `[small, capital]`, so the
//! server gives it a key type that uses Caps Lock; two characters without
//! case share one keycode at levels 1 and 2. A page's characters are typed
//! [`BIND_WAIT_MS`] after the binding (Chrome applies a new keymap a little
//! after the notification), and the keycodes change again only
//! [`SETTLE_MS`] after the last key typed with them - for the next page, or
//! for the restore at the end. When keys are sent faster than a client may
//! read them (`--delay-ms 0`), a change also waits until a client handling a
//! key per [`CLIENT_KEY_MS`] has caught up, at most [`MAX_BACKLOG_MS`].
//!
//! Only one bridge per display changes the keymap at a time: it owns the
//! selection [`LOCK_ATOM`] for the whole command (the server drops the
//! ownership when the client disconnects, also after SIGKILL). The lock
//! holder journals each binding in a root-window property before making it;
//! a later holder finds a journal of a dead holder and restores every keycode
//! that still carries exactly what was written ([`Remapper::new`]). At the
//! end, a keycode is restored only when it still carries the row the bridge
//! wrote: a layout loaded in between (setxkbmap, a compositor's new keymap on
//! XWayland) is left alone.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, Keycode, Keysym, PropMode, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::conn::Conn;
use crate::input::keymap::KeyboardMap;
use crate::input::layout::{LOCK_MASK, Layout, keysym_lower, keysym_upper, same_symbol};

/// Pause before a bound keycode changes again, so every client has
/// translated the keys typed with it. Chrome under load needed more than
/// 20 ms in the Xvfb experiments; GTK and urxvt needed 1 ms.
pub const SETTLE_MS: u64 = 200;

/// Pause after binding a page, before its first key. Chrome applies a
/// changed keymap a little after it reads the notification: without the
/// pause, 3 of 12 long texts had one character of a new page typed as the
/// character the same keycode carried in the previous page; with 50 ms,
/// none of 12.
pub const BIND_WAIT_MS: u64 = 50;

/// What a client may need per key it processes. A page's keycodes change only
/// once the client could have handled every key sent since the page was
/// bound at this rate (plus [`SETTLE_MS`]): with `--delay-ms 0` the bridge
/// sends faster than a busy client reads. At the app's 12 ms per character
/// this never adds a wait.
pub const CLIENT_KEY_MS: u64 = 4;

/// Upper bound of the backlog part of the wait: a burst of thousands of keys
/// must not hold the restore for longer than the caller's timeout.
pub const MAX_BACKLOG_MS: u64 = 2000;

/// How long a command waits for another bridge to release the keymap lock.
pub const LOCK_WAIT_MS: u64 = 3000;

/// Selection owned by the bridge that changes the keymap.
pub const LOCK_ATOM: &[u8] = b"_X11_BRIDGE_KEYMAP_LOCK";

/// Root-window property: the lock holder's bindings, CARDINAL/32,
/// `[2, owner window, keysyms_per_keycode, (keycode, original[per],
/// written[per], pending_a, pending_b)...]`. `written` is the row read back
/// after the binding; `pending_*` the keysyms of a binding being made.
const JOURNAL_ATOM: &[u8] = b"_X11_BRIDGE_KEYMAP_RESTORE";
const JOURNAL_VERSION: u32 = 2;

/// One thing to bind: a character with its case pair, or one without case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Cased { lower: Keysym, upper: Keysym },
    Single(Keysym),
}

impl Unit {
    pub fn keysyms(self) -> Vec<Keysym> {
        match self {
            Unit::Cased { lower, upper } => vec![lower, upper],
            Unit::Single(k) => vec![k],
        }
    }
}

/// The unit that binds `keysym`.
pub fn unit_for(keysym: Keysym) -> Unit {
    let lower = keysym_lower(keysym);
    let upper = keysym_upper(lower);
    // `µ` has the capital `Μ` (Greek), whose small letter is `μ`: still a
    // pair here, since under Caps Lock clients turn `µ` into `Μ`.
    if !same_symbol(lower, upper) && (same_symbol(keysym, lower) || same_symbol(keysym, upper)) {
        Unit::Cased { lower, upper }
    } else {
        Unit::Single(keysym)
    }
}

/// Keycodes a set of units needs: one per cased unit, one per two others.
pub fn slots_needed(units: &[Unit]) -> usize {
    let cased = units
        .iter()
        .filter(|u| matches!(u, Unit::Cased { .. }))
        .count();
    let singles = units.len() - cased;
    cased + singles.div_ceil(2)
}

/// Split a text into pages: `chars[i]` is the unit a character needs bound,
/// or `None` when the layout types it. Returns each page's first character
/// index and the distinct units it binds (fitting `slots` keycodes).
pub fn plan_pages(chars: &[Option<Unit>], slots: usize) -> Vec<(usize, Vec<Unit>)> {
    let slots = slots.max(1);
    let mut pages: Vec<(usize, Vec<Unit>)> = Vec::new();
    for (index, unit) in chars.iter().enumerate() {
        let Some(unit) = *unit else { continue };
        if let Some((_, units)) = pages.last_mut() {
            if units.contains(&unit) {
                continue;
            }
            units.push(unit);
            if slots_needed(units) <= slots {
                continue;
            }
            units.pop();
        }
        pages.push((index, vec![unit]));
    }
    pages
}

/// The core row for units sharing one keycode: `[l, u, l, u]` for a case
/// pair, `[a, b, a, b]` for two others, `[a, a, a, a]` for one; both groups
/// of the core mapping carry it, so any active group types it.
pub fn row_for(units: &[Unit], per: usize) -> Vec<Keysym> {
    let (a, b) = match units {
        [Unit::Cased { lower, upper }] => (*lower, *upper),
        [Unit::Single(a)] => (*a, *a),
        [Unit::Single(a), Unit::Single(b)] => (*a, *b),
        _ => (0, 0),
    };
    let mut row = vec![a, b, a, b];
    row.resize(per.max(1), 0);
    row.truncate(per.max(1));
    row
}

/// Assign a page's units to keycodes. `held[i]` is what slot `i` carries,
/// `order` the slot indices least recently changed first. Slots that carry a
/// unit of the page keep it; the rest go to free slots in `order`. Returns
/// the slots to (re)bind with their units, or `None` when the page does not
/// fit.
pub fn assign(
    held: &[Vec<Unit>],
    order: &[usize],
    page: &[Unit],
) -> Option<Vec<(usize, Vec<Unit>)>> {
    let try_assign = |keep: bool| -> Option<Vec<(usize, Vec<Unit>)>> {
        let kept: Vec<bool> = held
            .iter()
            .map(|units| keep && units.iter().any(|u| page.contains(u)))
            .collect();
        let done: Vec<Unit> = held
            .iter()
            .zip(&kept)
            .filter(|(_, k)| **k)
            .flat_map(|(units, _)| units.iter().copied())
            .collect();
        let todo: Vec<Unit> = page.iter().copied().filter(|u| !done.contains(u)).collect();
        let mut groups: Vec<Vec<Unit>> = todo
            .iter()
            .filter(|u| matches!(u, Unit::Cased { .. }))
            .map(|u| vec![*u])
            .collect();
        let singles: Vec<Unit> = todo
            .iter()
            .copied()
            .filter(|u| matches!(u, Unit::Single(_)))
            .collect();
        groups.extend(singles.chunks(2).map(<[Unit]>::to_vec));
        let mut free = order.iter().copied().filter(|&i| !kept[i]);
        let mut out = Vec::new();
        for g in groups {
            out.push((free.next()?, g));
        }
        Some(out)
    };
    try_assign(true).or_else(|| try_assign(false))
}

/// When bindings made at `changed` may change again: `settle` after the later
/// of the last key on them and the time a client needs for `keys` keys
/// (capped at [`MAX_BACKLOG_MS`]).
fn change_deadline(changed: Instant, last_used: Instant, keys: u32, settle: Duration) -> Instant {
    let backlog = Duration::from_millis((CLIENT_KEY_MS * u64::from(keys)).min(MAX_BACKLOG_MS));
    last_used.max(changed + backlog) + settle
}

/// Does `current` (a keycode's row now) still carry this bridge's binding:
/// exactly the row read back after it was made, or only keysyms of a binding
/// that was being made?
pub fn is_ours(current: &[Keysym], written: &[Keysym], pending: &[Keysym]) -> bool {
    let any = |row: &[Keysym]| row.iter().any(|&k| k != 0);
    if any(written) && current == written {
        return true;
    }
    let pending: Vec<Keysym> = pending.iter().copied().filter(|&k| k != 0).collect();
    !pending.is_empty()
        && any(current)
        && current
            .iter()
            .all(|&k| k == 0 || pending.iter().any(|&p| same_symbol(p, k)))
}

/// A journal entry: keycode, original row, written row, pending keysyms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub keycode: Keycode,
    pub original: Vec<Keysym>,
    pub written: Vec<Keysym>,
    pub pending: [Keysym; 2],
}

pub fn encode_journal(owner: Window, per: usize, entries: &[JournalEntry]) -> Vec<u32> {
    let mut data = vec![JOURNAL_VERSION, owner, per as u32];
    for e in entries {
        data.push(u32::from(e.keycode));
        data.extend(e.original.iter().copied());
        data.extend(e.written.iter().copied());
        data.extend(e.pending);
    }
    data
}

pub fn parse_journal(data: &[u32]) -> Option<(Window, usize, Vec<JournalEntry>)> {
    let [version, owner, per, rest @ ..] = data else {
        return None;
    };
    let per = *per as usize;
    if *version != JOURNAL_VERSION || per == 0 || per > 64 {
        return None;
    }
    let size = 1 + 2 * per + 2;
    if rest.len() % size != 0 {
        return None;
    }
    let mut out = Vec::new();
    for e in rest.chunks(size) {
        if e[0] > 255 {
            return None;
        }
        out.push(JournalEntry {
            keycode: e[0] as Keycode,
            original: e[1..=per].to_vec(),
            written: e[1 + per..1 + 2 * per].to_vec(),
            pending: [e[1 + 2 * per], e[2 + 2 * per]],
        });
    }
    Some((*owner, per, out))
}

/// The keymap lock: a selection owned by a window of this connection.
struct Lock {
    window: Window,
    atom: u32,
}

fn intern(conn: &Conn, name: &[u8]) -> Result<u32> {
    Ok(conn.conn.intern_atom(false, name)?.reply()?.atom)
}

fn acquire_lock(conn: &Conn, wait: Duration) -> Result<Lock> {
    let atom = intern(conn, LOCK_ATOM)?;
    let window = conn.conn.generate_id()?;
    conn.conn.create_window(
        0,
        window,
        conn.root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    let deadline = Instant::now() + wait;
    loop {
        // Check and take under a server grab, so two bridges cannot both see
        // no owner and both take it.
        conn.conn.grab_server()?;
        let owner = conn.conn.get_selection_owner(atom)?.reply();
        if let Ok(reply) = &owner
            && reply.owner == x11rb::NONE
        {
            conn.conn
                .set_selection_owner(window, atom, x11rb::CURRENT_TIME)?;
        }
        conn.conn.ungrab_server()?;
        let now_owner = conn.conn.get_selection_owner(atom)?.reply()?.owner;
        if now_owner == window {
            return Ok(Lock { window, atom });
        }
        if Instant::now() >= deadline {
            conn.conn.destroy_window(window)?;
            conn.conn.flush()?;
            bail!(
                "another x11-bridge (window {now_owner:#x}) is changing the keyboard mapping \
                 of this display; waited {} ms",
                wait.as_millis()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// The spare keycodes of one command, what they carry, and the lock.
pub struct Remapper<'a> {
    conn: &'a Conn,
    lock: Option<Lock>,
    journal_atom: u32,
    journaled: bool,
    per: usize,
    slots: Vec<Slot>,
    /// When the bindings last changed, and how many keys were sent since.
    changed: Option<Instant>,
    keys_since_change: u32,
    /// When a key was last typed on a bound keycode.
    last_used: Option<Instant>,
    settle: Duration,
    /// Keycodes of the change just made: their notifications, drained right
    /// after its sync, are ours.
    own_change: Vec<Keycode>,
    foreign_change: bool,
}

struct Slot {
    keycode: Keycode,
    original: Vec<Keysym>,
    units: Vec<Unit>,
    written: Vec<Keysym>,
    changed_at: Option<Instant>,
    /// The key type was set explicitly (XKB) and must be released again.
    explicit: bool,
}

/// XkbSetMap for one key: its symbols and type (`Some`), and its explicit
/// flags (key type 1 fixed, or none).
fn set_key_xkb(conn: &Conn, keycode: Keycode, map: Option<(u8, [Keysym; 2])>) -> Result<()> {
    let setup = conn.conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let mut aux = xkb::SetMapAux::new();
    let (n_syms, total_syms) = match map {
        Some((kt, syms)) => {
            aux = aux.syms(vec![xkb::KeySymMap {
                kt_index: [kt; 4],
                group_info: 1,
                width: 2,
                syms: syms.to_vec(),
            }]);
            aux = aux.explicit(vec![xkb::SetExplicit {
                keycode,
                explicit: xkb::Explicit::KEY_TYPE1,
            }]);
            (1, 2)
        }
        None => {
            aux = aux.explicit(Vec::new());
            (0, 0)
        }
    };
    let n_explicit = u8::from(map.is_some());
    conn.conn.xkb_set_map(
        xkb::ID::USE_CORE_KBD.into(),
        0u16.into(),
        min,
        max,
        0,
        0,
        if n_syms > 0 { keycode } else { 0 },
        n_syms,
        total_syms,
        0,
        0,
        0,
        0,
        0,
        0,
        keycode,
        1,
        n_explicit,
        0,
        0,
        0,
        0,
        0,
        0,
        0u16.into(),
        &aux,
    )?;
    Ok(())
}

impl<'a> Remapper<'a> {
    /// Start a keyboard command: watch for keymap changes and, when a bridge
    /// died with keycodes bound, take the lock and restore them.
    pub fn new(conn: &'a Conn) -> Result<Self> {
        let journal_atom = intern(conn, JOURNAL_ATOM)?;
        let mut r = Self {
            conn,
            lock: None,
            journal_atom,
            journaled: false,
            per: 0,
            slots: Vec::new(),
            changed: None,
            keys_since_change: 0,
            last_used: None,
            settle: Duration::from_millis(SETTLE_MS),
            own_change: Vec::new(),
            foreign_change: false,
        };
        r.watch_keymap();
        if !r.read_journal()?.is_empty() {
            r.take_lock()?;
            r.recover()?;
        }
        Ok(r)
    }

    fn watch_keymap(&self) {
        let c = &self.conn.conn;
        let ok = c
            .extension_information(xkb::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && c.xkb_use_extension(1, 0)
                .ok()
                .and_then(|k| k.reply().ok())
                .is_some_and(|r| r.supported);
        if !ok {
            return;
        }
        let events = xkb::EventType::MAP_NOTIFY | xkb::EventType::NEW_KEYBOARD_NOTIFY;
        let _ = c.xkb_select_events(
            xkb::ID::USE_CORE_KBD.into(),
            0u16.into(),
            events,
            0xffu16.into(),
            0xffu16.into(),
            &xkb::SelectEventsAux::new(),
        );
    }

    fn read_journal(&self) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .conn
            .get_property(
                false,
                self.conn.root,
                self.journal_atom,
                AtomEnum::CARDINAL,
                0,
                1 << 16,
            )?
            .reply()?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    fn take_lock(&mut self) -> Result<()> {
        if self.lock.is_none() {
            self.lock = Some(acquire_lock(
                self.conn,
                Duration::from_millis(LOCK_WAIT_MS),
            )?);
        }
        Ok(())
    }

    /// Restore what a dead lock holder left bound (we hold the lock, so any
    /// journal is from a holder that is gone).
    fn recover(&mut self) -> Result<()> {
        let data = self.read_journal()?;
        if let Some((_, per, entries)) = parse_journal(&data) {
            for e in entries {
                let current = self
                    .conn
                    .conn
                    .get_keyboard_mapping(e.keycode, 1)?
                    .reply()
                    .context("failed to read a journaled keycode")?;
                if usize::from(current.keysyms_per_keycode) == per
                    && is_ours(&current.keysyms, &e.written, &e.pending)
                {
                    self.conn
                        .conn
                        .change_keyboard_mapping(1, e.keycode, per as u8, &e.original)?;
                }
            }
        }
        self.conn
            .conn
            .delete_property(self.conn.root, self.journal_atom)?;
        self.conn.sync()?;
        self.drain_events();
        Ok(())
    }

    /// Take the lock and find the spare keycodes; returns how many there are.
    pub fn prepare(&mut self) -> Result<usize> {
        self.take_lock()?;
        if self.slots.is_empty() {
            let map = KeyboardMap::fetch(self.conn)?;
            self.per = map.keysyms_per_keycode();
            self.slots = map
                .spare_keycodes()
                .into_iter()
                .map(|(keycode, original)| Slot {
                    keycode,
                    original,
                    units: Vec::new(),
                    written: Vec::new(),
                    changed_at: None,
                    explicit: false,
                })
                .collect();
        }
        Ok(self.slots.len())
    }

    /// Record a key sent (`bound`: on a bound keycode).
    pub fn note_key(&mut self, bound: bool) {
        self.keys_since_change += 1;
        if bound {
            self.last_used = Some(Instant::now());
        }
    }

    /// Did the server report a keymap change that is not ours since the last
    /// call (setxkbmap, xmodmap, a compositor's new keymap on XWayland, or
    /// just the keyboard device switching between the user's keyboard and
    /// XTEST)? The caller compares the layout to tell a real change.
    pub fn keymap_suspect(&mut self) -> bool {
        self.drain_events();
        std::mem::take(&mut self.foreign_change)
    }

    fn drain_events(&mut self) {
        while let Ok(Some(event)) = self.conn.conn.poll_for_event() {
            match event {
                Event::XkbNewKeyboardNotify(_) => self.foreign_change = true,
                Event::XkbMapNotify(e) => {
                    let syms = u16::from(e.changed) & u16::from(xkb::MapPart::KEY_SYMS) != 0;
                    let first = u16::from(e.first_key_sym);
                    let own = (first..first + u16::from(e.n_key_syms))
                        .all(|k| self.own_change.iter().any(|&o| u16::from(o) == k));
                    if syms && !own {
                        self.foreign_change = true;
                    }
                }
                _ => {}
            }
        }
    }

    fn wait_before_change(&self) {
        let (Some(changed), Some(last_used)) = (self.changed, self.last_used) else {
            return;
        };
        let deadline = change_deadline(changed, last_used, self.keys_since_change, self.settle);
        let now = Instant::now();
        if deadline > now {
            thread::sleep(deadline - now);
        }
    }

    fn journal_entries(&self, pending: &[(usize, Vec<Unit>)]) -> Vec<JournalEntry> {
        let mut out = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            let next = pending.iter().find(|(j, _)| *j == i);
            if slot.units.is_empty() && next.is_none() {
                continue;
            }
            let mut p = [0; 2];
            if let Some((_, units)) = next {
                let syms: Vec<Keysym> = units.iter().flat_map(|u| u.keysyms()).collect();
                for (k, s) in p.iter_mut().zip(syms) {
                    *k = s;
                }
            }
            let mut written = slot.written.clone();
            written.resize(self.per, 0);
            out.push(JournalEntry {
                keycode: slot.keycode,
                original: slot.original.clone(),
                written,
                pending: p,
            });
        }
        out
    }

    fn write_journal(&mut self, pending: &[(usize, Vec<Unit>)]) -> Result<()> {
        let owner = self.lock.as_ref().map_or(0, |l| l.window);
        let data = encode_journal(owner, self.per, &self.journal_entries(pending));
        self.conn.conn.change_property32(
            PropMode::REPLACE,
            self.conn.root,
            self.journal_atom,
            AtomEnum::CARDINAL,
            &data,
        )?;
        self.journaled = true;
        Ok(())
    }

    fn read_rows(&self, keycodes: &[Keycode]) -> Result<Vec<Vec<Keysym>>> {
        let cookies = keycodes
            .iter()
            .map(|&k| self.conn.conn.get_keyboard_mapping(k, 1))
            .collect::<Result<Vec<_>, _>>()?;
        cookies
            .into_iter()
            .map(|c| Ok(c.reply()?.keysyms))
            .collect()
    }

    /// Bind a page's units, keeping keycodes that already carry one of them.
    pub fn load(&mut self, page: &[Unit]) -> Result<()> {
        let held: Vec<Vec<Unit>> = self.slots.iter().map(|s| s.units.clone()).collect();
        let mut order: Vec<usize> = (0..self.slots.len()).collect();
        order.sort_by_key(|&i| self.slots[i].changed_at);
        let Some(changes) = assign(&held, &order, page) else {
            bail!("more characters in one page than spare keycodes");
        };
        if changes.is_empty() {
            return Ok(());
        }
        self.wait_before_change();
        self.drain_events();
        self.write_journal(&changes)?;
        let per = self.per;
        for (index, units) in &changes {
            if self.slots[*index].explicit {
                set_key_xkb(self.conn, self.slots[*index].keycode, None)?;
                self.slots[*index].explicit = false;
            }
            let row = row_for(units, per);
            self.conn
                .conn
                .change_keyboard_mapping(1, self.slots[*index].keycode, per as u8, &row)
                .context("failed to bind a spare keycode")?;
        }
        self.conn.sync()?;
        let keycodes: Vec<Keycode> = changes
            .iter()
            .map(|(i, _)| self.slots[*i].keycode)
            .collect();
        let rows = self.read_rows(&keycodes)?;
        let now = Instant::now();
        for ((index, units), row) in changes.into_iter().zip(rows) {
            let slot = &mut self.slots[index];
            slot.units = units;
            slot.written = row;
            slot.changed_at = Some(now);
        }
        self.write_journal(&[])?;
        // Every notification of our change arrived before the sync reply.
        self.own_change = keycodes;
        self.drain_events();
        self.own_change.clear();
        thread::sleep(Duration::from_millis(BIND_WAIT_MS));
        self.changed = Some(Instant::now());
        self.keys_since_change = 0;
        self.last_used = None;
        Ok(())
    }

    /// Give every bound case pair a key type that uses Caps Lock. The server
    /// derives the type of a core row from the keysyms' case, which it knows
    /// only for legacy keysyms: a Unicode keysym pair (`ạ Ạ`, Armenian, ...)
    /// gets TWO_LEVEL, and under Caps Lock clients upper-case its level 1, so
    /// the small letter could not be typed. Returns whether a type changed
    /// (the caller then re-reads the layout).
    pub fn fix_case_types(&mut self, layout: &Layout) -> Result<bool> {
        let Some(alphabetic) = layout.alphabetic_type() else {
            return Ok(false);
        };
        let mut changed = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            let [Unit::Cased { lower, upper }] = slot.units.as_slice() else {
                continue;
            };
            if layout
                .key_type(slot.keycode)
                .is_some_and(|t| t.mods_mask & LOCK_MASK != 0)
            {
                continue;
            }
            set_key_xkb(
                self.conn,
                slot.keycode,
                Some((alphabetic, [*lower, *upper])),
            )?;
            changed.push(i);
        }
        if changed.is_empty() {
            return Ok(false);
        }
        self.conn.sync()?;
        let keycodes: Vec<Keycode> = changed.iter().map(|&i| self.slots[i].keycode).collect();
        let rows = self.read_rows(&keycodes)?;
        for (&i, row) in changed.iter().zip(rows) {
            self.slots[i].explicit = true;
            self.slots[i].written = row;
        }
        self.write_journal(&[])?;
        self.own_change = keycodes;
        self.drain_events();
        self.own_change.clear();
        Ok(true)
    }

    /// Restore every bound keycode that still carries our row (after the
    /// settle pause), drop the journal and release the lock. Returns the
    /// keycodes left alone because someone else changed them.
    pub fn finish(&mut self) -> Result<Vec<Keycode>> {
        let mut foreign = Vec::new();
        let bound: Vec<usize> = (0..self.slots.len())
            .filter(|&i| !self.slots[i].units.is_empty())
            .collect();
        if !bound.is_empty() {
            self.wait_before_change();
            let keycodes: Vec<Keycode> = bound.iter().map(|&i| self.slots[i].keycode).collect();
            let rows = self.read_rows(&keycodes)?;
            for (&i, row) in bound.iter().zip(rows) {
                let slot = &mut self.slots[i];
                if is_ours(&row, &slot.written, &[]) {
                    if slot.explicit {
                        set_key_xkb(self.conn, slot.keycode, None)?;
                    }
                    self.conn.conn.change_keyboard_mapping(
                        1,
                        slot.keycode,
                        self.per as u8,
                        &slot.original,
                    )?;
                } else {
                    foreign.push(slot.keycode);
                }
                slot.units.clear();
                slot.written.clear();
                slot.explicit = false;
            }
        }
        self.changed = None;
        self.last_used = None;
        if self.journaled {
            self.conn
                .conn
                .delete_property(self.conn.root, self.journal_atom)?;
            self.journaled = false;
        }
        if let Some(lock) = self.lock.take() {
            self.conn
                .conn
                .set_selection_owner(x11rb::NONE, lock.atom, x11rb::CURRENT_TIME)?;
            self.conn.conn.destroy_window(lock.window)?;
        }
        self.conn.sync()?;
        Ok(foreign)
    }
}

impl Drop for Remapper<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkeysym::key;

    fn s(k: Keysym) -> Option<Unit> {
        Some(Unit::Single(k))
    }

    #[test]
    fn cased_characters_bind_as_pairs() {
        let pair = Unit::Cased {
            lower: key::eacute,
            upper: key::Eacute,
        };
        assert_eq!(unit_for(key::eacute), pair);
        assert_eq!(unit_for(key::Eacute), pair);
        assert_eq!(unit_for(0x1004e2d), Unit::Single(0x1004e2d));
        // X's capital of ß is ẞ (Xlib upper-cases it under Caps Lock).
        assert_eq!(
            unit_for(key::ssharp),
            Unit::Cased {
                lower: key::ssharp,
                upper: 0x1001e9e
            }
        );
        assert_eq!(unit_for(key::_1), Unit::Single(key::_1));
        assert_eq!(
            unit_for(key::mu),
            Unit::Cased {
                lower: key::mu,
                upper: key::Greek_MU
            }
        );
        assert_eq!(
            row_for(&[pair], 7),
            vec![key::eacute, key::Eacute, key::eacute, key::Eacute, 0, 0, 0]
        );
        assert_eq!(
            row_for(&[Unit::Single(1), Unit::Single(2)], 4),
            vec![1, 2, 1, 2]
        );
        assert_eq!(row_for(&[Unit::Single(1)], 2), vec![1, 1]);
    }

    #[test]
    fn pages_fit_the_spare_keycodes() {
        let e = Some(unit_for(key::eacute));
        let chars = [None, s(1), s(2), e, s(1), None, s(3), s(4), e];
        // 2 keycodes: {1,2} share one, é takes one; 3 needs a new page.
        assert_eq!(
            plan_pages(&chars, 2),
            vec![
                (
                    1,
                    vec![Unit::Single(1), Unit::Single(2), unit_for(key::eacute)]
                ),
                (
                    6,
                    vec![Unit::Single(3), Unit::Single(4), unit_for(key::eacute)]
                ),
            ]
        );
        assert!(plan_pages(&[None, None], 4).is_empty());
        assert_eq!(
            plan_pages(&[s(7), s(7)], 0),
            vec![(0, vec![Unit::Single(7)])]
        );
        assert_eq!(slots_needed(&[Unit::Single(1)]), 1);
        assert_eq!(
            slots_needed(&[Unit::Single(1), Unit::Single(2), Unit::Single(3)]),
            2
        );
    }

    #[test]
    fn assignment_keeps_and_rotates_least_recent_first() {
        let a = Unit::Single(1);
        let b = Unit::Single(2);
        let c = Unit::Single(3);
        let held = vec![vec![a], vec![], vec![b]];
        // Slot 0 keeps a; of the others slot 2 changed longest ago.
        let out = assign(&held, &[0, 2, 1], &[a, c]).unwrap();
        assert_eq!(out, vec![(2, vec![c])]);
        // A page that does not fit while keeping is assigned from scratch.
        let held = vec![vec![a, b]];
        assert_eq!(assign(&held, &[0], &[a, c]), Some(vec![(0, vec![a, c])]));
        assert_eq!(assign(&[vec![]], &[0], &[a, b, c]), None);
    }

    #[test]
    fn change_waits_for_the_last_key_or_the_capped_backlog() {
        let t0 = Instant::now();
        let settle = Duration::from_millis(SETTLE_MS);
        let last = t0 + Duration::from_millis(12 * 50);
        assert_eq!(change_deadline(t0, last, 50, settle), last + settle);
        let burst = t0 + Duration::from_millis(5);
        assert_eq!(
            change_deadline(t0, burst, 100, settle),
            t0 + Duration::from_millis(CLIENT_KEY_MS * 100) + settle
        );
        assert_eq!(
            change_deadline(t0, burst, 4000, settle),
            t0 + Duration::from_millis(MAX_BACKLOG_MS) + settle
        );
    }

    #[test]
    fn journal_round_trip_and_rejects() {
        let e = JournalEntry {
            keycode: 97,
            original: vec![0, 0],
            written: vec![key::eacute, key::Eacute],
            pending: [0, 0],
        };
        let data = encode_journal(0x400001, 2, std::slice::from_ref(&e));
        assert_eq!(parse_journal(&data), Some((0x400001, 2, vec![e])));
        assert_eq!(parse_journal(&[1, 42, 2]), None);
        assert_eq!(parse_journal(&[2, 42, 2, 8, 0]), None);
        assert_eq!(parse_journal(&[2, 42, 0]), None);
        assert_eq!(parse_journal(&[]), None);
    }

    #[test]
    fn only_our_exact_rows_count_as_ours() {
        let written = [key::eacute, key::Eacute, key::eacute, key::Eacute, 0, 0, 0];
        assert!(is_ours(&written, &written, &[]));
        // A one-level key of a layout, or a user's xmodmap row, is not ours.
        let henkan = [0xff23, 0, 0xff23, 0, 0, 0, 0];
        assert!(!is_ours(&henkan, &written, &[]));
        assert!(!is_ours(&[0; 7], &written, &[]));
        // A binding in flight: only its keysyms.
        assert!(is_ours(&[key::a, key::A, 0, 0], &[0; 4], &[key::a, key::A]));
        assert!(!is_ours(
            &[key::a, key::b, 0, 0],
            &[0; 4],
            &[key::a, key::A]
        ));
        assert!(!is_ours(&[0; 4], &[0; 4], &[key::a, 0]));
    }
}
