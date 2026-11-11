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
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shape::{self, ConnectionExt as ShapeConnectionExt};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as XprotoConnectionExt, EventMask, MapState,
    Rectangle, StackMode, Window, WindowClass,
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
    net_client_list: u32,
    wm_state: u32,
    gtk_frame_extents: u32,
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
            "_NET_CLIENT_LIST",
            "WM_STATE",
            "_GTK_FRAME_EXTENTS",
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
            net_client_list: atoms[25],
            wm_state: atoms[26],
            gtk_frame_extents: atoms[27],
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

    /// Bundle id derivation, mirroring the JS `normalizeBundleIdFromWindow`
    /// candidate order (`desktop_file_name`, `resource_class`,
    /// `resource_name`, `id`) with the trailing `.desktop` stripped. Used to
    /// build an `AppRef` for frontmost/under-point.
    ///
    /// A window that names no app answers with its own id (decimal, the form
    /// `windows` prints), so the executor can still find it in the `windows`
    /// list and resolve it by pid, or refuse it as an unattributed window.
    /// Callers only ever pass an application window here (the ICCCM client,
    /// or an override-redirect window), never a WM frame or decoration window.
    fn bundle_id(&self) -> String {
        bundle_id_from(
            self.desktop_file_name.as_deref(),
            self.resource_class.as_deref(),
            self.resource_name.as_deref(),
        )
        .unwrap_or_else(|| self.id.to_string())
    }

    /// `{bundleId, displayName}` for this window. The display name mirrors the
    /// JS `displayNameFromWindow`: the window title, else the bundle id.
    fn to_app_ref(&self) -> AppRef {
        let bundle_id = self.bundle_id();
        let display_name = match self.title.as_deref() {
            Some(title) if !title.trim().is_empty() => title.to_owned(),
            _ => bundle_id.clone(),
        };
        AppRef {
            bundle_id,
            display_name,
        }
    }
}

/// First non-empty of the window's own app ids, `.desktop` stripped.
fn bundle_id_from(
    desktop_file_name: Option<&str>,
    resource_class: Option<&str>,
    resource_name: Option<&str>,
) -> Option<String> {
    [desktop_file_name, resource_class, resource_name]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(|value| strip_desktop_suffix(value).to_owned())
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

/// Pick the EWMH client list to use: `_NET_CLIENT_LIST_STACKING` when it is
/// non-empty, else `_NET_CLIENT_LIST`, else `None` (caller falls back to the
/// window tree).
fn select_client_list(stacking: Vec<Window>, plain: Vec<Window>) -> Option<Vec<Window>> {
    if !stacking.is_empty() {
        Some(stacking)
    } else if !plain.is_empty() {
        Some(plain)
    } else {
        None
    }
}

/// Whether `window` is a viewable, non-override-redirect window.
fn is_viewable_managed(conn: &Conn, window: Window) -> bool {
    conn.conn
        .get_window_attributes(window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|attrs| !attrs.override_redirect && attrs.map_state == MapState::VIEWABLE)
}

/// The ordered list of managed client windows, bottom-to-top.
///
/// Prefers `_NET_CLIENT_LIST_STACKING`. Some WMs (xfwm4) set only
/// `_NET_CLIENT_LIST`; that list is used next, in the WM's own order (mapping
/// order, not stacking), so callers that need "topmost"/"frontmost" rely on
/// `_NET_ACTIVE_WINDOW` there. Only when both are absent (bare X, Xvfb, minimal
/// WMs) does it fall back to a `query_tree` of the root's children, keeping
/// mapped windows that carry a `WM_CLASS` (a heuristic for application
/// top-levels rather than override-redirect popups). A root child without a
/// `WM_CLASS` is a WM frame of a reparenting WM; its viewable children are
/// searched one level down for the client. `query_tree` returns children
/// bottom-to-top.
fn client_list_stacking(conn: &Conn, atoms: &Atoms) -> Result<Vec<Window>> {
    let stacking = get_prop_u32(conn, conn.root, atoms.net_client_list_stacking);
    let plain = get_prop_u32(conn, conn.root, atoms.net_client_list);
    if let Some(list) = select_client_list(stacking, plain) {
        return Ok(list);
    }

    let tree = conn
        .conn
        .query_tree(conn.root)
        .context("failed to query the window tree")?
        .reply()
        .context("failed to read the window tree reply")?;

    let has_class = |window: Window| {
        let (instance, class) = get_wm_class(conn, window);
        instance.is_some() || class.is_some()
    };

    let mut windows = Vec::new();
    for &child in &tree.children {
        if !is_viewable_managed(conn, child) {
            continue;
        }
        // Only keep windows that look like application top-levels.
        if has_class(child) {
            windows.push(child);
            continue;
        }
        // Reparenting WM: the client is a child of the frame window.
        let inner = conn
            .conn
            .query_tree(child)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        if let Some(inner) = inner {
            windows.extend(
                inner
                    .children
                    .iter()
                    .copied()
                    .filter(|&c| is_viewable_managed(conn, c) && has_class(c)),
            );
        }
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

/// Map an X11 input-focus value to a window that can be a frontmost app.
///
/// `GetInputFocus` reports `None` (0) or `PointerRoot` (1) when no window holds
/// focus (bare Xvfb), and the root window when nothing but the desktop is
/// focused. None of those is an app, so they map to `None`.
fn focus_to_window(focus: Window, root: Window) -> Option<Window> {
    const POINTER_ROOT: Window = 1;
    if focus == x11rb::NONE || focus == POINTER_ROOT || focus == root {
        None
    } else {
        Some(focus)
    }
}

/// Report the currently active (focused/frontmost) app, or `None` when no
/// window is active (serialized as JSON `null`).
pub fn frontmost_app(conn: &Conn) -> Result<Option<AppRef>> {
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
            let Some(focus) = focus_to_window(focus, conn.root) else {
                return Ok(None);
            };
            // The focus can sit on a frame or on a subwindow of a client;
            // report the client it belongs to (never a frame's own id).
            let clients = managed_clients(conn, &atoms);
            match top_level_of(conn, focus)
                .and_then(|top| resolve_client(conn, &atoms, top, &clients))
            {
                Some(client) => client,
                None => return Ok(None),
            }
        }
    };

    let props = WindowProps::read(conn, &atoms, window);
    Ok(Some(props.to_app_ref()))
}

/// Every window in `_NET_CLIENT_LIST_STACKING` and `_NET_CLIENT_LIST` (empty
/// without an EWMH window manager).
fn managed_clients(conn: &Conn, atoms: &Atoms) -> Vec<Window> {
    let mut clients = get_prop_u32(conn, conn.root, atoms.net_client_list_stacking);
    for client in get_prop_u32(conn, conn.root, atoms.net_client_list) {
        if !clients.contains(&client) {
            clients.push(client);
        }
    }
    clients
}

/// The first managed client (from the WM's client list) whose root-child
/// ancestor is `top`: finds a client that sits deeper below its frame than
/// the `WM_STATE` search reaches.
fn client_in_frame(
    top: Window,
    clients: &[Window],
    mut top_level: impl FnMut(Window) -> Option<Window>,
) -> Option<Window> {
    clients
        .iter()
        .copied()
        .find(|&client| client != 0 && top_level(client) == Some(top))
}

/// Whether the server's SHAPE extension can report input shapes (version
/// 1.1 or later), which the hit test uses to look through click-through
/// windows.
pub fn input_shape_supported(conn: &Conn) -> Result<bool> {
    let present = conn
        .conn
        .extension_information(shape::X11_EXTENSION_NAME)
        .context("failed to query the SHAPE extension")?
        .is_some();
    if !present {
        return Ok(false);
    }
    let version = conn
        .conn
        .shape_query_version()
        .context("failed to request the SHAPE version")?
        .reply()
        .context("failed to read the SHAPE version")?;
    Ok(version.major_version > 1 || (version.major_version == 1 && version.minor_version >= 1))
}

/// Whether the point `(x, y)` lies in a region given as rectangles relative
/// to `(origin_x, origin_y)`. An empty list is an empty region.
fn region_contains(rects: &[Rectangle], origin_x: i32, origin_y: i32, x: i32, y: i32) -> bool {
    rects.iter().any(|r| {
        rect_contains(
            &Rect {
                x: origin_x + i32::from(r.x),
                y: origin_y + i32::from(r.y),
                width: i32::from(r.width),
                height: i32::from(r.height),
            },
            x,
            y,
        )
    })
}

/// Whether root child `top` takes a click at `(x, y)`: its input shape (the
/// region the server routes pointer events by) contains the point. A window
/// without an input shape reports its whole rectangle, so it counts in full;
/// a click-through window (an empty input shape, or a 1x1 one) does not. When
/// the shape cannot be read the window counts (the outer-rectangle rule).
fn takes_input_at(conn: &Conn, top: Window, origin_x: i32, origin_y: i32, x: i32, y: i32) -> bool {
    match conn
        .conn
        .shape_get_rectangles(top, shape::SK::INPUT)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    {
        Some(reply) => region_contains(&reply.rectangles, origin_x, origin_y, x, y),
        None => true,
    }
}

/// How deep below a root child the ICCCM client is searched for. Reparenting
/// WMs put it one level down (xfwm4, Openbox, Marco, Metacity, Mutter X11) or
/// two (KWin X11, Fluxbox); three leaves a margin, like the mask reader.
const CLIENT_SEARCH_DEPTH: usize = 3;

/// The ICCCM client below a root child (the `XmuClientWindow` route): `top`
/// itself when it carries `WM_STATE`, else the first window that does in a
/// breadth-first descent of at most [`CLIENT_SEARCH_DEPTH`] levels. The window
/// manager sets `WM_STATE` on every client it manages and never on its own
/// frame or decoration windows, so this finds the application window behind a
/// title bar or border without guessing from geometry.
fn find_client_with(
    top: Window,
    mut has_wm_state: impl FnMut(Window) -> bool,
    mut children: impl FnMut(Window) -> Vec<Window>,
) -> Option<Window> {
    if has_wm_state(top) {
        return Some(top);
    }
    let mut level = vec![top];
    for _ in 0..CLIENT_SEARCH_DEPTH {
        let mut next = Vec::new();
        for window in level {
            for child in children(window) {
                if has_wm_state(child) {
                    return Some(child);
                }
                next.push(child);
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    None
}

fn has_wm_state(conn: &Conn, atoms: &Atoms, window: Window) -> bool {
    conn.conn
        .get_property(false, window, atoms.wm_state, AtomEnum::ANY, 0, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.type_ != u32::from(AtomEnum::NONE))
}

/// A window's children, bottom to top; empty when it cannot be read (gone).
fn children_of(conn: &Conn, window: Window) -> Vec<Window> {
    conn.conn
        .query_tree(window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|tree| tree.children)
        .unwrap_or_default()
}

/// The application window behind root child `top`: the ICCCM client (see
/// [`find_client_with`]); else a member of the WM's client list (`clients`)
/// that sits below `top` deeper than that search reaches. Without a window
/// manager nobody sets `WM_STATE` (bare X, Xvfb); there `top` itself counts
/// when it carries a `WM_CLASS`, else its first viewable child that does (the
/// same heuristic as the `windows` tree fallback). `None` = a window of the
/// WM's own (no client below it).
fn resolve_client(conn: &Conn, atoms: &Atoms, top: Window, clients: &[Window]) -> Option<Window> {
    if let Some(client) = find_client_with(
        top,
        |window| has_wm_state(conn, atoms, window),
        |window| children_of(conn, window),
    ) {
        return Some(client);
    }
    if let Some(client) = client_in_frame(top, clients, |client| top_level_of(conn, client)) {
        return Some(client);
    }
    let has_class = |window: Window| {
        let (instance, class) = get_wm_class(conn, window);
        instance.is_some() || class.is_some()
    };
    if has_class(top) {
        return Some(top);
    }
    children_of(conn, top)
        .into_iter()
        .rev()
        .find(|&child| is_viewable_managed(conn, child) && has_class(child))
}

/// The root child that holds `window` (`window` itself when it is one).
fn top_level_of(conn: &Conn, window: Window) -> Option<Window> {
    let mut current = window;
    // Bounded: a window tree is never deeper than this in practice.
    for _ in 0..64 {
        let tree = conn.conn.query_tree(current).ok()?.reply().ok()?;
        if tree.parent == conn.root || tree.parent == x11rb::NONE {
            return Some(current);
        }
        current = tree.parent;
    }
    None
}

/// What one root child at the point means for the hit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// An application window (its frame, decorations or client area): the
    /// answer.
    App,
    /// A dock, the desktop or an override-redirect popup: the answer only when
    /// no application window is at the point.
    Fallback,
    /// Not a target here: hidden, no client, or a point in the invisible
    /// shadow margin of a client-side-decorated window. Look below.
    Skip,
}

/// Pick the answer from the root children at the point, top to bottom: the
/// first [`Hit::App`]; else the topmost [`Hit::Fallback`]; else nothing.
fn pick_hit<T>(layers: impl IntoIterator<Item = (T, Hit)>) -> Option<T> {
    let mut fallback = None;
    for (window, hit) in layers {
        match hit {
            Hit::App => return Some(window),
            Hit::Fallback => {
                if fallback.is_none() {
                    fallback = Some(window);
                }
            }
            Hit::Skip => {}
        }
    }
    fallback
}

/// The part of a root child that shows the window: its outer rect, or for a
/// client with `_GTK_FRAME_EXTENTS` (client-side decorations) the client rect
/// minus those invisible shadow margins. Same rule as the mask reader
/// ([`crate::mask::visual_rect`]), so a hit and a kept region agree.
fn visible_rect(outer: &Rect, client: &Rect, gtk_extents: &[u32]) -> Rect {
    let px = |r: &Rect| crate::mask::PxRect::new(r.x, r.y, r.width, r.height);
    let extents = (gtk_extents.len() >= 4).then(|| {
        [
            gtk_extents[0],
            gtk_extents[1],
            gtk_extents[2],
            gtk_extents[3],
        ]
    });
    let visible = crate::mask::visual_rect(px(outer), extents.map(|_| px(client)), extents);
    Rect {
        x: visible.x,
        y: visible.y,
        width: visible.width,
        height: visible.height,
    }
}

/// Classify root child `top` (known to cover the point with its outer rect)
/// and return the window whose app it stands for.
#[allow(clippy::too_many_arguments)]
fn classify_hit(
    conn: &Conn,
    atoms: &Atoms,
    clients: &[Window],
    top: Window,
    override_redirect: bool,
    outer: &Rect,
    x: i32,
    y: i32,
) -> (Window, Hit) {
    if override_redirect {
        return (top, Hit::Fallback);
    }
    let Some(client) = resolve_client(conn, atoms, top, clients) else {
        return (top, Hit::Skip);
    };
    let props = WindowProps::read(conn, atoms, client);
    if props.is_hidden {
        return (client, Hit::Skip);
    }
    let extents = get_prop_u32(conn, client, atoms.gtk_frame_extents);
    if !rect_contains(&visible_rect(outer, &props.geometry, &extents), x, y) {
        return (client, Hit::Skip);
    }
    if props.is_dock || props.is_desktop {
        (client, Hit::Fallback)
    } else {
        (client, Hit::App)
    }
}

/// Report the app of the topmost window at the global point `(x, y)`, or
/// `None` (JSON `null`, the no-app answer) when no window is there (the empty
/// desktop).
///
/// Walks the root's children top to bottom (the X stacking order, which every
/// window manager keeps for its frames), skipping the ones whose input shape
/// does not contain the point (click-through overlays: the click goes past
/// them). A point anywhere in a managed window's frame - title bar, buttons,
/// borders - or in its client area resolves to the client below that frame.
/// Docks, the desktop window and override-redirect popups only answer when no
/// application window is at the point; the invisible shadow margin of a
/// client-side-decorated window is not the window.
pub fn app_under_point(conn: &Conn, x: i32, y: i32) -> Result<Option<AppRef>> {
    let atoms = Atoms::intern(conn)?;
    let tree = conn
        .conn
        .query_tree(conn.root)
        .context("failed to query the window tree")?
        .reply()
        .context("failed to read the window tree reply")?;
    // Without SHAPE input shapes, every window counts in its full rectangle.
    let input_shapes = input_shape_supported(conn).unwrap_or(false);
    let clients = managed_clients(conn, &atoms);

    let mut layers = Vec::new();
    for &top in tree.children.iter().rev() {
        let Some(attrs) = conn
            .conn
            .get_window_attributes(top)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        if attrs.map_state != MapState::VIEWABLE || attrs.class == WindowClass::INPUT_ONLY {
            continue;
        }
        let Some(geometry) = conn
            .conn
            .get_geometry(top)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            continue;
        };
        let border = i32::from(geometry.border_width);
        let outer = Rect {
            x: i32::from(geometry.x),
            y: i32::from(geometry.y),
            width: i32::from(geometry.width) + 2 * border,
            height: i32::from(geometry.height) + 2 * border,
        };
        if !rect_contains(&outer, x, y) {
            continue;
        }
        // Shape rectangles are relative to the window origin (inside the
        // border).
        if input_shapes && !takes_input_at(conn, top, outer.x + border, outer.y + border, x, y) {
            continue;
        }
        let layer = classify_hit(
            conn,
            &atoms,
            &clients,
            top,
            attrs.override_redirect,
            &outer,
            x,
            y,
        );
        let done = layer.1 == Hit::App;
        layers.push(layer);
        if done {
            break;
        }
    }

    Ok(pick_hit(layers).map(|window| WindowProps::read(conn, &atoms, window).to_app_ref()))
}

/// Parse a window id string accepted by `activate-window`. Accepts both decimal
/// (`"12583107"`, the form `windows` emits) and `0x`-prefixed hex
/// (`"0xC00003"`), so callers that copy an id from `xwininfo`/`xprop` also work.
pub(crate) fn parse_window_id(window: &str) -> Result<Window> {
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
    fn focus_none_pointer_root_and_root_are_not_an_app() {
        const ROOT: Window = 0x123;
        assert_eq!(focus_to_window(x11rb::NONE, ROOT), None);
        assert_eq!(focus_to_window(1, ROOT), None);
        assert_eq!(focus_to_window(ROOT, ROOT), None);
        assert_eq!(focus_to_window(0x40_0001, ROOT), Some(0x40_0001));
    }

    #[test]
    fn client_list_prefers_stacking_then_plain_then_none() {
        assert_eq!(select_client_list(vec![1, 2], vec![3]), Some(vec![1, 2]));
        assert_eq!(select_client_list(vec![], vec![3, 4]), Some(vec![3, 4]));
        assert_eq!(select_client_list(vec![], vec![]), None);
    }

    #[test]
    fn no_frontmost_app_serializes_to_null() {
        let none: Option<AppRef> = None;
        assert_eq!(serde_json::to_string(&none).unwrap(), "null");
    }

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

    /// A window tree for `find_client_with`: (window, children, has WM_STATE).
    fn search(tree: &[(Window, &[Window], bool)], top: Window) -> Option<Window> {
        let node = |w: Window| tree.iter().find(|(id, _, _)| *id == w);
        find_client_with(
            top,
            |w| node(w).is_some_and(|(_, _, state)| *state),
            |w| {
                node(w)
                    .map(|(_, kids, _)| kids.to_vec())
                    .unwrap_or_default()
            },
        )
    }

    #[test]
    fn client_is_the_top_level_itself_when_it_carries_wm_state() {
        // Non-reparenting WM (or a client mapped straight on the root).
        assert_eq!(search(&[(10, &[11], true), (11, &[], true)], 10), Some(10));
    }

    #[test]
    fn client_is_found_below_an_xfwm4_style_frame() {
        // xfwm4: the frame holds a dozen decoration windows (title, buttons,
        // borders, corners) and the client, all one level down. Only the
        // client carries WM_STATE.
        let tree: &[(Window, &[Window], bool)] = &[
            (100, &[101, 102, 103, 104, 50], false),
            (101, &[], false),
            (102, &[], false),
            (103, &[], false),
            (104, &[], false),
            (50, &[51], true),
            (51, &[], false),
        ];
        assert_eq!(search(tree, 100), Some(50));
    }

    #[test]
    fn client_is_found_two_levels_down() {
        // KWin X11 / Fluxbox style: frame -> wrapper -> client.
        let tree: &[(Window, &[Window], bool)] = &[
            (100, &[101, 102], false),
            (101, &[], false),
            (102, &[60], false),
            (60, &[], true),
        ];
        assert_eq!(search(tree, 100), Some(60));
    }

    #[test]
    fn no_client_without_wm_state_or_beyond_the_search_depth() {
        let flat: &[(Window, &[Window], bool)] = &[(100, &[101], false), (101, &[], false)];
        assert_eq!(search(flat, 100), None);
        // WM_STATE four levels down is out of reach.
        let deep: &[(Window, &[Window], bool)] = &[
            (100, &[1], false),
            (1, &[2], false),
            (2, &[3], false),
            (3, &[4], false),
            (4, &[], true),
        ];
        assert_eq!(search(deep, 100), None);
    }

    #[test]
    fn pick_hit_prefers_the_topmost_app() {
        assert_eq!(pick_hit([(1, Hit::App), (2, Hit::App)]), Some(1));
        // A dock or popup above an app window does not win over it.
        assert_eq!(pick_hit([(1, Hit::Fallback), (2, Hit::App)]), Some(2));
        // A shadow margin / hidden window is looked through.
        assert_eq!(pick_hit([(1, Hit::Skip), (2, Hit::App)]), Some(2));
    }

    #[test]
    fn pick_hit_falls_back_to_the_topmost_dock_or_popup() {
        assert_eq!(
            pick_hit([(1, Hit::Skip), (2, Hit::Fallback), (3, Hit::Fallback)]),
            Some(2)
        );
        assert_eq!(pick_hit([(1, Hit::Skip)]), None);
        assert_eq!(pick_hit(Vec::<(u32, Hit)>::new()), None);
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn visible_rect_drops_the_csd_shadow_margin() {
        // An undecorated frame the size of the client, 20 px shadow all round.
        let outer = rect(100, 100, 300, 200);
        let visible = visible_rect(&outer, &outer, &[20, 20, 20, 20]);
        assert_eq!(
            (visible.x, visible.y, visible.width, visible.height),
            (120, 120, 260, 160)
        );
        assert!(!rect_contains(&visible, 105, 200)); // left shadow
        assert!(!rect_contains(&visible, 250, 395)); // bottom shadow
        assert!(rect_contains(&visible, 121, 121)); // visible corner
    }

    #[test]
    fn visible_rect_is_the_outer_rect_without_gtk_extents() {
        // A decorated frame: the title bar and borders are outside the client
        // but inside the outer rect, so they count.
        let outer = rect(200, 150, 410, 334);
        let client = rect(205, 179, 400, 300);
        let visible = visible_rect(&outer, &client, &[]);
        assert_eq!(
            (visible.x, visible.y, visible.width, visible.height),
            (200, 150, 410, 334)
        );
        assert!(rect_contains(&visible, 405, 164)); // title bar
        assert!(rect_contains(&visible, 202, 329)); // left border
        // Malformed (short) extents are ignored too.
        let short = visible_rect(&outer, &client, &[1, 2]);
        assert_eq!(short.width, 410);
    }

    fn r(x: i16, y: i16, width: u16, height: u16) -> Rectangle {
        Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn input_region_empty_and_1x1_take_no_click_elsewhere() {
        // Electron's setIgnoreMouseEvents(true): an empty input shape, or a
        // 1x1 one at the origin. Window origin at (100, 50).
        assert!(!region_contains(&[], 100, 50, 640, 400));
        assert!(!region_contains(&[r(0, 0, 1, 1)], 100, 50, 640, 400));
        assert!(region_contains(&[r(0, 0, 1, 1)], 100, 50, 100, 50));
        assert!(!region_contains(&[r(0, 0, 1, 1)], 100, 50, 101, 50));
    }

    #[test]
    fn input_region_default_is_the_whole_window() {
        // No input shape set: the server reports the window's own rectangle
        // (border included, so it can start at a negative offset).
        let full = [r(-2, -2, 404, 304)];
        assert!(region_contains(&full, 100, 50, 98, 48));
        assert!(region_contains(&full, 100, 50, 501, 351));
        assert!(!region_contains(&full, 100, 50, 502, 351));
        // Several rectangles: a hole between them takes no click.
        let split = [r(0, 0, 10, 10), r(20, 0, 10, 10)];
        assert!(region_contains(&split, 0, 0, 25, 5));
        assert!(!region_contains(&split, 0, 0, 15, 5));
    }

    #[test]
    fn client_in_frame_picks_the_listed_client_below_this_root_child() {
        // Clients 7 and 9 live under root child 100, client 8 under 200.
        let top = |c: Window| match c {
            7 | 9 => Some(100),
            8 => Some(200),
            _ => None,
        };
        assert_eq!(client_in_frame(100, &[8, 7, 9], top), Some(7));
        assert_eq!(client_in_frame(200, &[8, 7, 9], top), Some(8));
        // A WM-internal root child (nothing listed under it).
        assert_eq!(client_in_frame(300, &[8, 7, 9], top), None);
        assert_eq!(client_in_frame(100, &[], top), None);
        assert_eq!(client_in_frame(100, &[0], |_| Some(100)), None);
    }

    #[test]
    fn bundle_id_from_takes_the_first_app_id() {
        assert_eq!(
            bundle_id_from(Some("org.kde.dolphin.desktop"), Some("dolphin"), None).as_deref(),
            Some("org.kde.dolphin")
        );
        assert_eq!(
            bundle_id_from(None, Some(" "), Some("frametest")).as_deref(),
            Some("frametest")
        );
        assert_eq!(
            bundle_id_from(None, Some("com.anthropic.Claude"), Some("claude")).as_deref(),
            Some("com.anthropic.Claude")
        );
        assert_eq!(bundle_id_from(None, None, None), None);
        assert_eq!(bundle_id_from(Some(""), Some("  "), None), None);
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
