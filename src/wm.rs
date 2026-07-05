//! Window-manager queries over EWMH/ICCCM (`_NET_*` / `WM_*` properties).
//!
//! This replaces the shell-out chain the JS executor uses on X11:
//! `xdotool getactivewindow` / `getwindowclassname` / `getwindowname`
//! (`frontmost-app`), `wmctrl -l` + per-window `xdotool` (`windows` /
//! `listRunningApps`), and `xdotool getmouselocation` (`app-under-point`).
//!
//! Everything is read from properties on the root window and on each managed
//! client window. Property reads are made tolerant of a missing EWMH window
//! manager (bare X, Xvfb): when `_NET_CLIENT_LIST_STACKING` is absent we fall
//! back to a `query_tree` walk of mapped top-levels, and every optional
//! property that is absent maps to `None` rather than an error.

use anyhow::{Context, Result, anyhow};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as XprotoConnectionExt, EventMask, MapState,
    StackMode, Window,
};

use crate::conn::Conn;
use crate::output::{AppRef, Rect, WindowAppRef, WindowInfo};

/// The set of X11 atoms this module needs, interned once per invocation.
///
/// Interning in a single round-trip (via `get_atoms`) keeps the one-shot
/// process fast: a `windows` call touches ~14 atoms but only pays for one
/// server round-trip to resolve all of them.
struct Atoms {
    net_client_list_stacking: u32,
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    net_wm_window_type: u32,
    net_wm_state: u32,
    net_wm_state_hidden: u32,
    net_wm_state_above: u32,
    wm_window_role: u32,
    kde_net_wm_desktop_file: u32,
    gtk_application_id: u32,
    // `_NET_WM_WINDOW_TYPE_*` values we classify.
    type_normal: u32,
    type_dialog: u32,
    type_dock: u32,
    type_desktop: u32,
    type_utility: u32,
    type_toolbar: u32,
    type_menu: u32,
    type_splash: u32,
    type_notification: u32,
    type_tooltip: u32,
    type_combo: u32,
    type_dnd: u32,
    type_popup_menu: u32,
    type_dropdown_menu: u32,
}

impl Atoms {
    /// Intern every atom in one round-trip. `only_if_exists=false` is fine here:
    /// atoms are cheap and creating a name that the WM never uses is harmless.
    fn intern(conn: &Conn) -> Result<Self> {
        const NAMES: &[&str] = &[
            "_NET_CLIENT_LIST_STACKING",
            "_NET_ACTIVE_WINDOW",
            "_NET_WM_NAME",
            "_NET_WM_PID",
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_STATE",
            "_NET_WM_STATE_HIDDEN",
            "_NET_WM_STATE_ABOVE",
            "WM_WINDOW_ROLE",
            "_KDE_NET_WM_DESKTOP_FILE",
            "_GTK_APPLICATION_ID",
            "_NET_WM_WINDOW_TYPE_NORMAL",
            "_NET_WM_WINDOW_TYPE_DIALOG",
            "_NET_WM_WINDOW_TYPE_DOCK",
            "_NET_WM_WINDOW_TYPE_DESKTOP",
            "_NET_WM_WINDOW_TYPE_UTILITY",
            "_NET_WM_WINDOW_TYPE_TOOLBAR",
            "_NET_WM_WINDOW_TYPE_MENU",
            "_NET_WM_WINDOW_TYPE_SPLASH",
            "_NET_WM_WINDOW_TYPE_NOTIFICATION",
            "_NET_WM_WINDOW_TYPE_TOOLTIP",
            "_NET_WM_WINDOW_TYPE_COMBO",
            "_NET_WM_WINDOW_TYPE_DND",
            "_NET_WM_WINDOW_TYPE_POPUP_MENU",
            "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU",
        ];

        // Fire all intern_atom requests, then collect replies: still bounded
        // round-trips, and x11rb pipelines the requests behind the scenes.
        let cookies = NAMES
            .iter()
            .map(|name| conn.conn.intern_atom(false, name.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .context("failed to request atom interning")?;
        let mut atoms = Vec::with_capacity(NAMES.len());
        for cookie in cookies {
            atoms.push(cookie.reply().context("failed to read interned atom")?.atom);
        }

        Ok(Self {
            net_client_list_stacking: atoms[0],
            net_active_window: atoms[1],
            net_wm_name: atoms[2],
            net_wm_pid: atoms[3],
            net_wm_window_type: atoms[4],
            net_wm_state: atoms[5],
            net_wm_state_hidden: atoms[6],
            net_wm_state_above: atoms[7],
            wm_window_role: atoms[8],
            kde_net_wm_desktop_file: atoms[9],
            gtk_application_id: atoms[10],
            type_normal: atoms[11],
            type_dialog: atoms[12],
            type_dock: atoms[13],
            type_desktop: atoms[14],
            type_utility: atoms[15],
            type_toolbar: atoms[16],
            type_menu: atoms[17],
            type_splash: atoms[18],
            type_notification: atoms[19],
            type_tooltip: atoms[20],
            type_combo: atoms[21],
            type_dnd: atoms[22],
            type_popup_menu: atoms[23],
            type_dropdown_menu: atoms[24],
        })
    }

    /// Map a `_NET_WM_WINDOW_TYPE` atom to a stable lowercase string, mirroring
    /// the KWin bridge's `window_type` (a `String(window.windowType)`), so the
    /// JS sees the same shape from either backend.
    fn window_type_name(&self, atom: u32) -> Option<&'static str> {
        Some(match atom {
            a if a == self.type_normal => "normal",
            a if a == self.type_dialog => "dialog",
            a if a == self.type_dock => "dock",
            a if a == self.type_desktop => "desktop",
            a if a == self.type_utility => "utility",
            a if a == self.type_toolbar => "toolbar",
            a if a == self.type_menu => "menu",
            a if a == self.type_splash => "splash",
            a if a == self.type_notification => "notification",
            a if a == self.type_tooltip => "tooltip",
            a if a == self.type_combo => "combo",
            a if a == self.type_dnd => "dnd",
            a if a == self.type_popup_menu => "popup_menu",
            a if a == self.type_dropdown_menu => "dropdown_menu",
            _ => return None,
        })
    }
}

/// Read a window property that is a list of 32-bit values (atoms/windows/ids).
///
/// Returns an empty vec if the property is absent or the wrong type — callers
/// treat "absent" and "empty" identically, so a missing EWMH WM degrades
/// gracefully instead of erroring.
fn get_prop_u32(conn: &Conn, window: Window, property: u32) -> Vec<u32> {
    let reply = conn
        .conn
        .get_property(false, window, property, AtomEnum::ANY, 0, u32::MAX)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    match reply {
        Some(reply) => reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Read a single cardinal (first 32-bit value) of a property, if present.
fn get_prop_cardinal(conn: &Conn, window: Window, property: u32) -> Option<u32> {
    get_prop_u32(conn, window, property).into_iter().next()
}

/// Read a string property, trying a UTF-8 property first (`_NET_WM_NAME`) then a
/// Latin-1 fallback (`WM_NAME`). Empty strings become `None`.
fn get_prop_string(conn: &Conn, window: Window, property: u32) -> Option<String> {
    let reply = conn
        .conn
        .get_property(false, window, property, AtomEnum::ANY, 0, u32::MAX)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    // UTF8_STRING and STRING (Latin-1) are both byte arrays; from_utf8_lossy
    // handles UTF-8 exactly and degrades Latin-1 high bytes to replacement
    // chars, which is acceptable for titles/roles.
    let text = String::from_utf8_lossy(&reply.value)
        .trim_end_matches('\0')
        .to_owned();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// `WM_CLASS` is two NUL-terminated strings: instance (resource name) then
/// class (resource class). Returns `(resource_name, resource_class)`.
fn get_wm_class(conn: &Conn, window: Window) -> (Option<String>, Option<String>) {
    let reply = conn
        .conn
        .get_property(
            false,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            u32::MAX,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let Some(reply) = reply else {
        return (None, None);
    };
    if reply.value.is_empty() {
        return (None, None);
    }
    let mut parts = reply
        .value
        .split(|&byte| byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let instance = parts.next();
    let class = parts.next();
    (instance, class)
}

/// A fully-read view of one window's properties, shared by the `windows`,
/// `frontmost-app`, and `app-under-point` code paths.
struct WindowProps {
    id: Window,
    title: Option<String>,
    resource_name: Option<String>,
    resource_class: Option<String>,
    desktop_file_name: Option<String>,
    window_role: Option<String>,
    pid: Option<u32>,
    window_type: Option<String>,
    is_dock: bool,
    is_desktop: bool,
    is_dialog: bool,
    is_normal_window: bool,
    is_hidden: bool,
    is_above: bool,
    map_state_viewable: bool,
    transient_for: Option<Window>,
    geometry: Rect,
}

impl WindowProps {
    fn read(conn: &Conn, atoms: &Atoms, window: Window) -> WindowProps {
        let title = get_prop_string(conn, window, atoms.net_wm_name)
            .or_else(|| get_prop_string(conn, window, AtomEnum::WM_NAME.into()));
        let (resource_name, resource_class) = get_wm_class(conn, window);
        let desktop_file_name = get_prop_string(conn, window, atoms.kde_net_wm_desktop_file)
            .or_else(|| get_prop_string(conn, window, atoms.gtk_application_id));
        let window_role = get_prop_string(conn, window, atoms.wm_window_role);
        let pid = get_prop_cardinal(conn, window, atoms.net_wm_pid);

        let types = get_prop_u32(conn, window, atoms.net_wm_window_type);
        let window_type = types
            .iter()
            .find_map(|&atom| atoms.window_type_name(atom))
            .map(|name| name.to_owned());
        let is_dock = types.contains(&atoms.type_dock);
        let is_desktop = types.contains(&atoms.type_desktop);
        let is_dialog = types.contains(&atoms.type_dialog);
        // A window with no _NET_WM_WINDOW_TYPE at all is a normal window per
        // EWMH; one that is explicitly _NET_WM_WINDOW_TYPE_NORMAL is too.
        let is_normal_window = types.is_empty()
            || types.contains(&atoms.type_normal)
            || !has_special_type(atoms, &types);

        let state = get_prop_u32(conn, window, atoms.net_wm_state);
        let is_hidden = state.contains(&atoms.net_wm_state_hidden);
        let is_above = state.contains(&atoms.net_wm_state_above);

        let transient_for = get_prop_cardinal(conn, window, AtomEnum::WM_TRANSIENT_FOR.into())
            .filter(|&id| id != 0);

        let (map_state_viewable, geometry) = geometry_and_map_state(conn, window);

        WindowProps {
            id: window,
            title,
            resource_name,
            resource_class,
            desktop_file_name,
            window_role,
            pid,
            window_type,
            is_dock,
            is_desktop,
            is_dialog,
            is_normal_window,
            is_hidden,
            is_above,
            map_state_viewable,
            transient_for,
            geometry,
        }
    }

    /// Best-effort bundle id derivation, mirroring the JS
    /// `normalizeBundleIdFromWindow` candidate order (`desktop_file_name`,
    /// `resource_class`, `resource_name`, `id`) with the trailing `.desktop`
    /// stripped. Used to build an `AppRef` for frontmost/under-point.
    fn bundle_id(&self) -> String {
        for value in [
            self.desktop_file_name.as_deref(),
            self.resource_class.as_deref(),
            self.resource_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return strip_desktop_suffix(trimmed).to_owned();
            }
        }
        // Last resort: the numeric window id as a string (matches the JS, which
        // falls through to `window.id`).
        self.id.to_string()
    }

    /// Display name for an `AppRef`, mirroring the JS `displayNameFromWindow`:
    /// prefer the window title, else the bundle id.
    fn display_name(&self) -> String {
        if let Some(title) = self.title.as_deref()
            && !title.trim().is_empty()
        {
            return title.to_owned();
        }
        self.bundle_id()
    }

    fn to_app_ref(&self) -> AppRef {
        AppRef {
            bundle_id: self.bundle_id(),
            display_name: self.display_name(),
        }
    }
}

/// Whether a `_NET_WM_WINDOW_TYPE` list contains any type that makes a window
/// non-"normal" (dialog, dock, menu, splash, ...). Presence of these means the
/// window should not be treated as a normal top-level.
fn has_special_type(atoms: &Atoms, types: &[u32]) -> bool {
    types.iter().any(|&atom| {
        atom == atoms.type_dialog
            || atom == atoms.type_dock
            || atom == atoms.type_desktop
            || atom == atoms.type_utility
            || atom == atoms.type_toolbar
            || atom == atoms.type_menu
            || atom == atoms.type_splash
            || atom == atoms.type_notification
            || atom == atoms.type_tooltip
            || atom == atoms.type_combo
            || atom == atoms.type_dnd
            || atom == atoms.type_popup_menu
            || atom == atoms.type_dropdown_menu
    })
}

/// Strip a trailing `.desktop` (case-insensitive), matching the JS
/// `.replace(/\.desktop$/i, "")`.
fn strip_desktop_suffix(value: &str) -> &str {
    if value.len() >= 8 && value[value.len() - 8..].eq_ignore_ascii_case(".desktop") {
        &value[..value.len() - 8]
    } else {
        value
    }
}

/// Read a window's map state and its root-relative geometry.
///
/// `get_geometry` yields the window's size and its position relative to its
/// parent; `translate_coordinates` converts the top-left corner into root
/// (global) coordinates, which is the space the JS contract expects.
fn geometry_and_map_state(conn: &Conn, window: Window) -> (bool, Rect) {
    let geometry = conn
        .conn
        .get_geometry(window)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let Some(geometry) = geometry else {
        return (
            false,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
        );
    };

    let translated = conn
        .conn
        .translate_coordinates(window, conn.root, 0, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok());

    let (root_x, root_y) = match translated {
        Some(reply) => (i32::from(reply.dst_x), i32::from(reply.dst_y)),
        None => (i32::from(geometry.x), i32::from(geometry.y)),
    };

    let attrs = conn
        .conn
        .get_window_attributes(window)
        .ok()
        .and_then(|cookie| cookie.reply().ok());
    let viewable = matches!(attrs.map(|a| a.map_state), Some(MapState::VIEWABLE));

    (
        viewable,
        Rect {
            x: root_x,
            y: root_y,
            width: i32::from(geometry.width),
            height: i32::from(geometry.height),
        },
    )
}

/// The center point of a rectangle, used to pick which monitor a window is on.
fn rect_center(rect: &Rect) -> (i32, i32) {
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

/// Name of the monitor whose rectangle contains `(x, y)`, else `None`.
fn monitor_containing(conn: &Conn, x: i32, y: i32) -> Option<String> {
    let monitors = conn.monitors().ok()?;
    monitors
        .into_iter()
        .find(|monitor| {
            x >= monitor.x
                && x < monitor.x + monitor.width
                && y >= monitor.y
                && y < monitor.y + monitor.height
        })
        .map(|monitor| monitor.name)
}

/// Whether a root-relative rect contains a global point (half-open on the
/// right/bottom edge, matching the JS `rectContains`).
fn rect_contains(rect: &Rect, x: i32, y: i32) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x + rect.width
        && y >= rect.y
        && y < rect.y + rect.height
}

/// Build the `transient_for` `WindowAppRef` chain by walking `WM_TRANSIENT_FOR`
/// up to a small depth, guarding against cycles via a seen-set (mirrors the JS
/// `normalizeBundleIdFromWindowRef` recursion, which is depth-8 in the KWin
/// bridge).
fn build_app_ref_chain(
    conn: &Conn,
    atoms: &Atoms,
    parent: Window,
    seen: &mut Vec<Window>,
    depth: usize,
) -> Option<Box<WindowAppRef>> {
    if depth == 0 || parent == 0 || seen.contains(&parent) {
        return None;
    }
    seen.push(parent);

    let props = WindowProps::read(conn, atoms, parent);
    let transient = props.transient_for.is_some();
    let transient_for = props
        .transient_for
        .and_then(|next| build_app_ref_chain(conn, atoms, next, seen, depth - 1));

    Some(Box::new(WindowAppRef {
        id: parent.to_string(),
        desktop_file_name: props.desktop_file_name,
        resource_class: props.resource_class,
        resource_name: props.resource_name,
        transient: Some(transient),
        transient_for,
    }))
}

/// The ordered list of managed client windows, bottom-to-top.
///
/// Prefers `_NET_CLIENT_LIST_STACKING` (a full EWMH WM). If that property is
/// absent — bare X, Xvfb, some minimal WMs — falls back to a `query_tree` of
/// the root's children, keeping only mapped windows that carry a `WM_CLASS`
/// (a decent heuristic for "real" application top-levels rather than override
/// -redirect popups). `query_tree` already returns children bottom-to-top.
fn client_list_stacking(conn: &Conn, atoms: &Atoms) -> Result<Vec<Window>> {
    let ewmh = get_prop_u32(conn, conn.root, atoms.net_client_list_stacking);
    if !ewmh.is_empty() {
        return Ok(ewmh);
    }

    let tree = conn
        .conn
        .query_tree(conn.root)
        .context("failed to query the window tree")?
        .reply()
        .context("failed to read the window tree reply")?;

    let mut windows = Vec::new();
    for &child in &tree.children {
        let attrs = conn
            .conn
            .get_window_attributes(child)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        let Some(attrs) = attrs else { continue };
        if attrs.override_redirect || attrs.map_state != MapState::VIEWABLE {
            continue;
        }
        // Only keep windows that look like application top-levels.
        let (instance, class) = get_wm_class(conn, child);
        if instance.is_none() && class.is_none() {
            continue;
        }
        windows.push(child);
    }
    Ok(windows)
}

/// Enumerate managed top-level windows in stacking order (bottom to top).
pub fn windows(conn: &Conn) -> Result<Vec<WindowInfo>> {
    let atoms = Atoms::intern(conn)?;
    let stacking = client_list_stacking(conn, &atoms)?;
    let active = get_prop_cardinal(conn, conn.root, atoms.net_active_window).unwrap_or(0);

    let mut result = Vec::with_capacity(stacking.len());
    for (index, &window) in stacking.iter().enumerate() {
        let props = WindowProps::read(conn, &atoms, window);

        let is_visible = props.map_state_viewable && !props.is_hidden;
        let transient = props.transient_for.is_some();
        let transient_for = props.transient_for.and_then(|parent| {
            let mut seen = vec![window];
            build_app_ref_chain(conn, &atoms, parent, &mut seen, 8)
        });
        let (cx, cy) = rect_center(&props.geometry);
        let output = monitor_containing(conn, cx, cy);

        result.push(WindowInfo {
            id: props.id.to_string(),
            title: props.title.clone().unwrap_or_default(),
            geometry: props.geometry.clone(),
            pid: props.pid,
            desktop_file_name: props.desktop_file_name.clone(),
            resource_class: props.resource_class.clone(),
            resource_name: props.resource_name.clone(),
            window_role: props.window_role.clone(),
            window_type: props.window_type.clone(),
            is_dock: Some(props.is_dock),
            is_desktop: Some(props.is_desktop),
            is_visible: Some(is_visible),
            is_minimized: Some(props.is_hidden),
            is_normal_window: Some(props.is_normal_window),
            is_dialog: Some(props.is_dialog),
            transient: Some(transient),
            transient_for,
            output,
            stacking_order: index,
            is_active: window == active,
            // X11 has no per-window capture-exclusion analogue (that is a KWin
            // compositor feature). Always false; see DESIGN.md.
            exclude_from_capture: false,
            keep_above: Some(props.is_above),
        });
    }

    Ok(result)
}

/// Report the currently active (focused/frontmost) app.
pub fn frontmost_app(conn: &Conn) -> Result<AppRef> {
    let atoms = Atoms::intern(conn)?;
    let active = get_prop_cardinal(conn, conn.root, atoms.net_active_window).filter(|&id| id != 0);

    // Fall back to the input-focus window when there is no EWMH
    // `_NET_ACTIVE_WINDOW` (minimal WMs, Xvfb).
    let window = match active {
        Some(window) => window,
        None => {
            let focus = conn
                .conn
                .get_input_focus()
                .context("failed to query input focus")?
                .reply()
                .context("failed to read input focus reply")?
                .focus;
            if focus == 0 || focus == x11rb::NONE {
                return Err(anyhow!(
                    "no active window (no _NET_ACTIVE_WINDOW and no input focus)"
                ));
            }
            focus
        }
    };

    let props = WindowProps::read(conn, &atoms, window);
    Ok(props.to_app_ref())
}

/// Report the topmost app whose frame contains the global point `(x, y)`.
pub fn app_under_point(conn: &Conn, x: i32, y: i32) -> Result<AppRef> {
    let atoms = Atoms::intern(conn)?;
    let stacking = client_list_stacking(conn, &atoms)?;

    // Walk top-to-bottom (stacking is bottom-to-top) and pick the first mapped,
    // non-dock/desktop window whose root-relative frame contains the point.
    for &window in stacking.iter().rev() {
        let props = WindowProps::read(conn, &atoms, window);
        if !props.map_state_viewable || props.is_hidden {
            continue;
        }
        if props.is_dock || props.is_desktop {
            continue;
        }
        if rect_contains(&props.geometry, x, y) {
            return Ok(props.to_app_ref());
        }
    }

    // Fallback for a missing client list: translate the point down the tree
    // from the root to the deepest child under it, then read that window.
    let mut current = conn.root;
    loop {
        let reply = conn
            .conn
            .translate_coordinates(
                conn.root,
                current,
                i16::try_from(x).unwrap_or(0),
                i16::try_from(y).unwrap_or(0),
            )
            .context("failed to translate coordinates during hit-test")?
            .reply()
            .context("failed to read coordinate translation")?;
        let child = reply.child;
        if child == 0 || child == x11rb::NONE {
            break;
        }
        current = child;
    }

    if current == conn.root {
        return Err(anyhow!("no window under point ({x}, {y})"));
    }
    let props = WindowProps::read(conn, &atoms, current);
    Ok(props.to_app_ref())
}

/// Parse a window id string accepted by `activate-window`. Accepts both decimal
/// (`"12583107"`, the form `windows` emits) and `0x`-prefixed hex
/// (`"0xC00003"`), so callers that copy an id from `xwininfo`/`xprop` also work.
fn parse_window_id(window: &str) -> Result<Window> {
    let trimmed = window.trim();
    let parsed = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u32>()
    };
    parsed.map_err(|_| anyhow!("invalid window id `{window}` (expected decimal or 0x-hex)"))
}

/// Raise and focus the window with the given id.
///
/// Primary path: a spec-compliant `_NET_ACTIVE_WINDOW` client message to the
/// root (source indication 2 = "pager", which WMs honour without focus-stealing
/// heuristics). Fallback for non-EWMH WMs: a direct `ConfigureWindow` raise so
/// the window at least comes to the top of the stack.
pub fn activate_window(conn: &Conn, window: &str) -> Result<serde_json::Value> {
    let atoms = Atoms::intern(conn)?;
    let window_id = parse_window_id(window)?;

    let event = ClientMessageEvent::new(
        32,
        window_id,
        atoms.net_active_window,
        // data: [source indication, timestamp, requestor window, 0, 0]
        [2, x11rb::CURRENT_TIME, 0, 0, 0],
    );
    conn.conn
        .send_event(
            false,
            conn.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .context("failed to send _NET_ACTIVE_WINDOW client message")?;

    // Fallback raise for WMs that ignore the client message.
    conn.conn
        .configure_window(
            window_id,
            &x11rb::protocol::xproto::ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )
        .context("failed to raise window")?;

    conn.conn
        .flush()
        .context("failed to flush activation requests")?;

    Ok(serde_json::json!({ "activated": window_id.to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_window_id_accepts_decimal() {
        assert_eq!(parse_window_id("12583107").unwrap(), 12_583_107);
        assert_eq!(parse_window_id("  42 ").unwrap(), 42);
    }

    #[test]
    fn parse_window_id_accepts_hex() {
        assert_eq!(parse_window_id("0xC00003").unwrap(), 0x00C0_0003);
        assert_eq!(parse_window_id("0Xff").unwrap(), 0xFF);
    }

    #[test]
    fn parse_window_id_rejects_garbage() {
        assert!(parse_window_id("notanumber").is_err());
        assert!(parse_window_id("").is_err());
        assert!(parse_window_id("0xZZ").is_err());
    }

    #[test]
    fn strip_desktop_suffix_is_case_insensitive() {
        assert_eq!(
            strip_desktop_suffix("org.kde.dolphin.desktop"),
            "org.kde.dolphin"
        );
        assert_eq!(strip_desktop_suffix("Foo.DESKTOP"), "Foo");
        assert_eq!(strip_desktop_suffix("firefox"), "firefox");
        // Only a trailing suffix is stripped.
        assert_eq!(strip_desktop_suffix("a.desktop.b"), "a.desktop.b");
    }

    #[test]
    fn rect_contains_is_half_open() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        };
        assert!(rect_contains(&rect, 10, 20)); // top-left inclusive
        assert!(rect_contains(&rect, 109, 69));
        assert!(!rect_contains(&rect, 110, 20)); // right edge exclusive
        assert!(!rect_contains(&rect, 10, 70)); // bottom edge exclusive
        assert!(!rect_contains(&rect, 9, 20));
        // Zero-area rect never contains anything.
        let empty = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert!(!rect_contains(&empty, 0, 0));
    }

    #[test]
    fn rect_center_is_midpoint() {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        assert_eq!(rect_center(&rect), (100, 50));
        let offset = Rect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(rect_center(&offset), (2880, 540));
    }
}
