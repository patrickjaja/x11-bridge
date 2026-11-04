//! Allowlist-filtered capture ("mask mode").
//!
//! `screenshot` / `zoom` with `--keep-window <id>` (repeatable) or `--mask-all`
//! paint every pixel that is NOT part of a kept window's visible region with a
//! solid fill (`#1E1E1E`). The desktop, panels, notifications, windows that
//! appear later, override-redirect windows of other apps and Claude's own window
//! are therefore masked unless the caller keeps them. A masked window stays a
//! solid rectangle at its position, so the model still sees where it is.
//!
//! ## Algorithm
//!
//! 1. Grab the server so the window tree and the `GetImage` pixels are one
//!    atomic snapshot (no window can map, move or close in between).
//! 2. `QueryTree(root)`: the root's children in the server's real stacking order
//!    (bottom to top). This is the order that decides which window owns a root
//!    pixel, so it is used instead of `_NET_CLIENT_LIST_STACKING` (which skips
//!    override-redirect windows and, on xfwm4, is not published at all). Each
//!    viewable `InputOutput` child is one layer. Its rectangle is the child's
//!    outer geometry (border included): on a reparenting WM the child is the
//!    frame, so title bar and borders of a window belong to it.
//!    A client with `_GTK_FRAME_EXTENTS` (client-side decorations) is reduced to
//!    its visible part, so its invisible shadow margin neither shows nor hides
//!    anything.
//! 3. Each kept id (a client id from `windows`, or a frame id) is walked up to its
//!    root child. Ids that do not exist or are not viewable are reported as
//!    `kept_missing` and masked.
//! 4. Ownership: a layer is also kept when it is a transient (`WM_TRANSIENT_FOR`)
//!    of a kept layer, and an override-redirect layer (menu, tooltip, dropdown)
//!    is kept when it is stacked above a kept layer of the same app: same
//!    `WM_CLIENT_LEADER`, or same `_NET_WM_PID` with a compatible `WM_CLASS`.
//! 5. Painter's algorithm over the capture rectangle: each pixel belongs to the
//!    topmost layer covering it (or to the root). A pixel is kept only when that
//!    owner is a kept layer. A kept window covered by a masked one is masked
//!    there: `GetImage` only has the top pixels.
//!
//! Any failure while reading the tree fails the command: an image is never
//! emitted unmasked when masking was requested.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as XprotoConnectionExt, MapState, Window, WindowClass,
};

use crate::conn::Conn;

/// Fill colour of masked pixels (R, G, B).
pub const MASK_FILL: [u8; 3] = [0x1E, 0x1E, 0x1E];
/// `MASK_FILL` as reported in the JSON.
pub const MASK_FILL_HEX: &str = "#1E1E1E";

/// What the caller asked to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskRequest {
    /// `--mask-all`: nothing is kept, every pixel is masked.
    All,
    /// `--keep-window <id>...`: keep only these windows (and what they own).
    Keep(Vec<Window>),
}

impl MaskRequest {
    /// Build from the CLI flags. `None` = no mask mode (unchanged behaviour).
    pub fn from_cli(keep_windows: Vec<Window>, mask_all: bool) -> Option<Self> {
        if mask_all {
            Some(MaskRequest::All)
        } else if keep_windows.is_empty() {
            None
        } else {
            Some(MaskRequest::Keep(keep_windows))
        }
    }

    fn ids(&self) -> &[Window] {
        match self {
            MaskRequest::All => &[],
            MaskRequest::Keep(ids) => ids,
        }
    }
}

/// `mask` object added to `screenshot` / `zoom` output in mask mode.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MaskReport {
    /// `"keep"` (`--keep-window`) or `"all"` (`--mask-all`).
    pub mode: &'static str,
    /// Fill colour of masked pixels.
    pub fill: &'static str,
    /// Requested ids with at least one visible pixel in the image.
    pub kept: Vec<String>,
    /// Requested ids that exist and are viewable but have no visible pixel in
    /// the image (outside the region, or fully covered by masked windows).
    pub kept_hidden: Vec<String>,
    /// Requested ids that do not exist or are not viewable (unmapped, other
    /// workspace, minimized). Masked like everything else.
    pub kept_missing: Vec<String>,
    /// Windows kept without being requested because a kept window owns them
    /// (transient dialogs, menus, tooltips), with visible pixels in the image.
    pub auto_kept: Vec<String>,
    /// Masked windows with at least one pixel in the image, topmost first.
    pub masked: Vec<MaskedWindow>,
}

/// A masked window, identified like a `windows` entry (no title: the title of
/// a window the user did not approve is not sent anywhere).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MaskedWindow {
    /// Client id (matches `windows` ids), or the root child when no client was found.
    pub id: String,
    /// The root child (WM frame, or the window itself).
    pub frame: String,
    pub pid: Option<u32>,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub override_redirect: bool,
}

/// A rectangle in root pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl PxRect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn intersect(&self, other: &PxRect) -> Option<PxRect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        (x1 > x0 && y1 > y0).then(|| PxRect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

/// The visible rectangle of a root child: its outer rect, or, when the client
/// has `_GTK_FRAME_EXTENTS` (left, right, top, bottom), the client rect minus
/// those invisible margins, clipped to the outer rect. An empty result has no
/// pixels.
pub fn visual_rect(outer: PxRect, client: Option<PxRect>, gtk_extents: Option<[u32; 4]>) -> PxRect {
    let (Some(client), Some([l, r, t, b])) = (client, gtk_extents) else {
        return outer;
    };
    let clamp = |v: u32| i32::try_from(v).unwrap_or(i32::MAX / 4);
    let inset = PxRect::new(
        client.x.saturating_add(clamp(l)),
        client.y.saturating_add(clamp(t)),
        client
            .width
            .saturating_sub(clamp(l).saturating_add(clamp(r))),
        client
            .height
            .saturating_sub(clamp(t).saturating_add(clamp(b))),
    );
    inset
        .intersect(&outer)
        .unwrap_or(PxRect::new(outer.x, outer.y, 0, 0))
}

/// One root child, bottom to top, with the properties ownership needs.
#[derive(Debug, Clone, Default)]
pub struct Layer {
    /// The root child.
    pub frame: Window,
    /// The ICCCM client (window with `WM_STATE`), when one was found.
    pub client: Option<Window>,
    pub rect: Option<PxRect>,
    pub override_redirect: bool,
    pub pid: Option<u32>,
    pub leader: Option<Window>,
    pub transient_for: Option<Window>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub desktop_file_name: Option<String>,
}

impl Layer {
    fn id(&self) -> Window {
        self.client.unwrap_or(self.frame)
    }

    fn is(&self, window: Window) -> bool {
        window != 0 && (self.frame == window || self.client == Some(window))
    }
}

/// Decide which layers are kept. `direct[i]` marks layers whose frame a
/// requested id resolved to. Returns `kept[i]`.
///
/// Rules (applied until nothing changes):
/// - a layer whose `WM_TRANSIENT_FOR` is a kept layer is kept (an
///   override-redirect one only when stacked above it);
/// - an override-redirect layer stacked above a kept layer is kept when both
///   share a non-zero `WM_CLIENT_LEADER`, or share `_NET_WM_PID` and their
///   `WM_CLASS` classes do not differ.
pub fn decide_kept(layers: &[Layer], direct: &[bool]) -> Vec<bool> {
    let mut kept = direct.to_vec();
    loop {
        let mut changed = false;
        for i in 0..layers.len() {
            if kept[i] {
                continue;
            }
            let me = &layers[i];
            let owned = (0..layers.len()).any(|j| {
                if j == i || !kept[j] {
                    return false;
                }
                let other = &layers[j];
                let above = i > j;
                if let Some(parent) = me.transient_for
                    && other.is(parent)
                    && (!me.override_redirect || above)
                {
                    return true;
                }
                if !me.override_redirect || !above {
                    return false;
                }
                if let (Some(a), Some(b)) = (me.leader, other.leader)
                    && a != 0
                    && a == b
                {
                    return true;
                }
                if let (Some(a), Some(b)) = (me.pid, other.pid)
                    && a != 0
                    && a == b
                {
                    return match (&me.resource_class, &other.resource_class) {
                        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
                        _ => true,
                    };
                }
                false
            });
            if owned {
                kept[i] = true;
                changed = true;
            }
        }
        if !changed {
            return kept;
        }
    }
}

/// Per-pixel owner map of a capture rectangle: 0 = root, `i + 1` = layer `i`.
pub struct Coverage {
    area: PxRect,
    owners: Vec<u16>,
}

impl Coverage {
    /// Painter's algorithm, bottom to top.
    pub fn build(area: PxRect, layers: &[Layer]) -> Result<Self> {
        if area.is_empty() {
            bail!("mask area is empty");
        }
        if layers.len() >= usize::from(u16::MAX) {
            bail!("too many windows to mask ({})", layers.len());
        }
        let w = area.width as usize;
        let mut owners = vec![0u16; w * area.height as usize];
        for (index, layer) in layers.iter().enumerate() {
            let Some(rect) = layer.rect.and_then(|r| r.intersect(&area)) else {
                continue;
            };
            let owner = (index + 1) as u16;
            let x0 = (rect.x - area.x) as usize;
            let x1 = x0 + rect.width as usize;
            for row in (rect.y - area.y) as usize..(rect.y - area.y + rect.height) as usize {
                owners[row * w + x0..row * w + x1].fill(owner);
            }
        }
        Ok(Self { area, owners })
    }

    /// Per layer: whether it owns at least one pixel.
    pub fn visible(&self, layer_count: usize) -> Vec<bool> {
        let mut seen = vec![false; layer_count];
        for &owner in &self.owners {
            if owner != 0 {
                seen[usize::from(owner) - 1] = true;
            }
        }
        seen
    }

    /// Whether the pixel at `(col, row)` of the area is kept.
    #[cfg(test)]
    pub fn is_kept(&self, kept: &[bool], col: usize, row: usize) -> bool {
        let owner = self.owners[row * self.area.width as usize + col];
        owner != 0 && kept[usize::from(owner) - 1]
    }

    /// Paint every pixel not owned by a kept layer with `MASK_FILL`. `pixels`
    /// is packed RGB of exactly the area's size.
    pub fn apply(&self, kept: &[bool], pixels: &mut [u8]) -> Result<()> {
        let expected = self.owners.len() * 3;
        if pixels.len() != expected {
            bail!(
                "mask/image size mismatch: {} bytes for {}x{}",
                pixels.len(),
                self.area.width,
                self.area.height
            );
        }
        for (pixel, &owner) in pixels.chunks_exact_mut(3).zip(&self.owners) {
            if owner == 0 || !kept[usize::from(owner) - 1] {
                pixel.copy_from_slice(&MASK_FILL);
            }
        }
        Ok(())
    }
}

/// Build the JSON report from the decided layers.
pub fn build_report(
    request: &MaskRequest,
    layers: &[Layer],
    direct_of: &[Option<usize>],
    kept: &[bool],
    visible: &[bool],
) -> MaskReport {
    let ids = request.ids();
    let mut report = MaskReport {
        mode: match request {
            MaskRequest::All => "all",
            MaskRequest::Keep(_) => "keep",
        },
        fill: MASK_FILL_HEX,
        kept: Vec::new(),
        kept_hidden: Vec::new(),
        kept_missing: Vec::new(),
        auto_kept: Vec::new(),
        masked: Vec::new(),
    };
    let mut seen_ids = HashSet::new();
    for (&id, layer) in ids.iter().zip(direct_of) {
        if !seen_ids.insert(id) {
            continue;
        }
        match layer {
            None => report.kept_missing.push(id.to_string()),
            Some(i) if visible[*i] => report.kept.push(id.to_string()),
            Some(_) => report.kept_hidden.push(id.to_string()),
        }
    }
    let direct: HashSet<usize> = direct_of.iter().flatten().copied().collect();
    for (i, layer) in layers.iter().enumerate() {
        if kept[i] && visible[i] && !direct.contains(&i) {
            report.auto_kept.push(layer.id().to_string());
        }
    }
    for (i, layer) in layers.iter().enumerate().rev() {
        if !kept[i] && visible[i] {
            report.masked.push(MaskedWindow {
                id: layer.id().to_string(),
                frame: layer.frame.to_string(),
                pid: layer.pid,
                desktop_file_name: layer.desktop_file_name.clone(),
                resource_class: layer.resource_class.clone(),
                resource_name: layer.resource_name.clone(),
                override_redirect: layer.override_redirect,
            });
        }
    }
    report
}

// ---------------------------------------------------------------------------
// X11 side
// ---------------------------------------------------------------------------

/// Atoms the mask reader needs. Interned before the server grab.
pub struct MaskAtoms {
    wm_state: u32,
    wm_client_leader: u32,
    net_wm_pid: u32,
    gtk_frame_extents: u32,
    kde_desktop_file: u32,
    gtk_application_id: u32,
    swm_vroot: u32,
}

impl MaskAtoms {
    pub fn intern(conn: &Conn) -> Result<Self> {
        const NAMES: [&str; 7] = [
            "WM_STATE",
            "WM_CLIENT_LEADER",
            "_NET_WM_PID",
            "_GTK_FRAME_EXTENTS",
            "_KDE_NET_WM_DESKTOP_FILE",
            "_GTK_APPLICATION_ID",
            "__SWM_VROOT",
        ];
        let cookies = NAMES
            .iter()
            .map(|name| conn.conn.intern_atom(false, name.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .context("failed to request atoms for masking")?;
        let mut atoms = [0u32; 7];
        for (slot, cookie) in atoms.iter_mut().zip(cookies) {
            *slot = cookie
                .reply()
                .context("failed to intern an atom for masking")?
                .atom;
        }
        Ok(Self {
            wm_state: atoms[0],
            wm_client_leader: atoms[1],
            net_wm_pid: atoms[2],
            gtk_frame_extents: atoms[3],
            kde_desktop_file: atoms[4],
            gtk_application_id: atoms[5],
            swm_vroot: atoms[6],
        })
    }
}

/// Holds an X server grab; released on drop (the connection closing releases
/// it too).
pub struct ServerGrab<'a> {
    conn: &'a Conn,
}

impl<'a> ServerGrab<'a> {
    pub fn new(conn: &'a Conn) -> Result<Self> {
        conn.conn
            .grab_server()
            .context("failed to grab the X server")?;
        Ok(Self { conn })
    }
}

impl Drop for ServerGrab<'_> {
    fn drop(&mut self) {
        let _ = self.conn.conn.ungrab_server();
        let _ = self.conn.conn.flush();
    }
}

/// Raw property bytes/words of `window`, `None` when absent. Errors (the
/// window is gone) propagate: the server is grabbed, so that is unexpected.
fn prop(
    conn: &Conn,
    window: Window,
    atom: u32,
) -> Result<Option<x11rb::protocol::xproto::GetPropertyReply>> {
    let reply = conn
        .conn
        .get_property(false, window, atom, AtomEnum::ANY, 0, 1024)
        .context("failed to request a window property")?
        .reply()
        .with_context(|| format!("failed to read a property of window {window}"))?;
    Ok((reply.type_ != u32::from(AtomEnum::NONE)).then_some(reply))
}

fn prop_u32s(conn: &Conn, window: Window, atom: u32) -> Result<Vec<u32>> {
    Ok(prop(conn, window, atom)?
        .and_then(|reply| reply.value32().map(|iter| iter.collect()))
        .unwrap_or_default())
}

fn prop_string(conn: &Conn, window: Window, atom: u32) -> Result<Option<String>> {
    Ok(prop(conn, window, atom)?.and_then(|reply| {
        let text = String::from_utf8_lossy(&reply.value)
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }))
}

fn has_wm_state(conn: &Conn, atoms: &MaskAtoms, window: Window) -> Result<bool> {
    Ok(prop(conn, window, atoms.wm_state)?.is_some())
}

/// Find the ICCCM client (a window carrying `WM_STATE`) at or below `frame`,
/// searching at most three levels deep.
fn find_client(conn: &Conn, atoms: &MaskAtoms, frame: Window) -> Result<Option<Window>> {
    if has_wm_state(conn, atoms, frame)? {
        return Ok(Some(frame));
    }
    let mut level = vec![frame];
    for _ in 0..3 {
        let mut next = Vec::new();
        for window in level {
            let tree = conn
                .conn
                .query_tree(window)
                .context("failed to request a window subtree")?
                .reply()
                .context("failed to read a window subtree")?;
            for &child in &tree.children {
                if has_wm_state(conn, atoms, child)? {
                    return Ok(Some(child));
                }
                next.push(child);
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    Ok(None)
}

/// Root-relative rectangle (inside size, no border) of a window.
fn inner_root_rect(conn: &Conn, window: Window) -> Result<PxRect> {
    let geom = conn
        .conn
        .get_geometry(window)
        .context("failed to request window geometry")?
        .reply()
        .context("failed to read window geometry")?;
    let origin = conn
        .conn
        .translate_coordinates(window, conn.root, 0, 0)
        .context("failed to request window position")?
        .reply()
        .context("failed to read window position")?;
    Ok(PxRect::new(
        i32::from(origin.dst_x),
        i32::from(origin.dst_y),
        i32::from(geom.width),
        i32::from(geom.height),
    ))
}

/// Read every viewable `InputOutput` root child, bottom to top. Must run under
/// a server grab.
pub fn read_layers(conn: &Conn, atoms: &MaskAtoms) -> Result<Vec<Layer>> {
    let tree = conn
        .conn
        .query_tree(conn.root)
        .context("mask: failed to request the window tree")?
        .reply()
        .context("mask: failed to read the window tree")?;

    // Pipeline attributes + geometry for every root child.
    let mut cookies = Vec::with_capacity(tree.children.len());
    for &child in &tree.children {
        cookies.push((
            child,
            conn.conn
                .get_window_attributes(child)
                .context("mask: failed to request window attributes")?,
            conn.conn
                .get_geometry(child)
                .context("mask: failed to request window geometry")?,
        ));
    }

    let mut layers = Vec::new();
    for (frame, attrs, geom) in cookies {
        let attrs = attrs
            .reply()
            .with_context(|| format!("mask: failed to read attributes of window {frame}"))?;
        let geom = geom
            .reply()
            .with_context(|| format!("mask: failed to read geometry of window {frame}"))?;
        if attrs.map_state != MapState::VIEWABLE || attrs.class == WindowClass::INPUT_ONLY {
            continue;
        }
        if prop(conn, frame, atoms.swm_vroot)?.is_some() {
            bail!(
                "mask: window {frame} is a virtual root (__SWM_VROOT); masking is not supported on this window manager"
            );
        }
        let border = i32::from(geom.border_width);
        let outer = PxRect::new(
            i32::from(geom.x),
            i32::from(geom.y),
            i32::from(geom.width) + 2 * border,
            i32::from(geom.height) + 2 * border,
        );

        let client = if attrs.override_redirect {
            Some(frame)
        } else {
            find_client(conn, atoms, frame)?
        };
        let ident = client.unwrap_or(frame);

        let gtk = prop_u32s(conn, ident, atoms.gtk_frame_extents)?;
        let gtk = (gtk.len() >= 4).then(|| [gtk[0], gtk[1], gtk[2], gtk[3]]);
        let client_rect = match (client, gtk) {
            (Some(c), Some(_)) => Some(inner_root_rect(conn, c)?),
            _ => None,
        };

        let (resource_name, resource_class) = wm_class(conn, ident)?;
        let desktop_file_name = match prop_string(conn, ident, atoms.kde_desktop_file)? {
            Some(name) => Some(name),
            None => prop_string(conn, ident, atoms.gtk_application_id)?,
        };

        layers.push(Layer {
            frame,
            client,
            rect: Some(visual_rect(outer, client_rect, gtk)),
            override_redirect: attrs.override_redirect,
            pid: prop_u32s(conn, ident, atoms.net_wm_pid)?.first().copied(),
            leader: prop_u32s(conn, ident, atoms.wm_client_leader)?
                .first()
                .copied()
                .filter(|&w| w != 0),
            transient_for: prop_u32s(conn, ident, AtomEnum::WM_TRANSIENT_FOR.into())?
                .first()
                .copied()
                .filter(|&w| w != 0 && w != conn.root),
            resource_class,
            resource_name,
            desktop_file_name,
        });
    }
    Ok(layers)
}

fn wm_class(conn: &Conn, window: Window) -> Result<(Option<String>, Option<String>)> {
    let Some(reply) = prop(conn, window, AtomEnum::WM_CLASS.into())? else {
        return Ok((None, None));
    };
    let mut parts = reply
        .value
        .split(|&byte| byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned());
    Ok((parts.next(), parts.next()))
}

/// Map each requested id to the index of the layer (root child) it lives in.
/// `None` = the id does not exist, is the root, or is not viewable.
pub fn resolve_requested(
    conn: &Conn,
    layers: &[Layer],
    request: &MaskRequest,
) -> Result<Vec<Option<usize>>> {
    request
        .ids()
        .iter()
        .map(|&id| resolve_one(conn, layers, id))
        .collect()
}

fn resolve_one(conn: &Conn, layers: &[Layer], id: Window) -> Result<Option<usize>> {
    if id == 0 || id == conn.root {
        return Ok(None);
    }
    // The id itself must be viewable (a mapped frame can hold an unmapped
    // client on some WMs). BadWindow = the id does not exist: missing.
    let Ok(attrs) = conn
        .conn
        .get_window_attributes(id)
        .context("mask: failed to request window attributes")?
        .reply()
    else {
        return Ok(None);
    };
    if attrs.map_state != MapState::VIEWABLE {
        return Ok(None);
    }
    let mut current = id;
    for _ in 0..64 {
        let Ok(tree) = conn
            .conn
            .query_tree(current)
            .context("mask: failed to request the window parent")?
            .reply()
        else {
            return Ok(None);
        };
        if tree.parent == conn.root {
            return Ok(layers.iter().position(|layer| layer.frame == current));
        }
        if tree.parent == 0 {
            return Ok(None);
        }
        current = tree.parent;
    }
    Err(anyhow!("mask: window {id} is nested too deeply"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(frame: Window, rect: PxRect) -> Layer {
        Layer {
            frame,
            client: Some(frame),
            rect: Some(rect),
            ..Layer::default()
        }
    }

    fn kept_grid(area: PxRect, layers: &[Layer], direct: &[bool]) -> (Coverage, Vec<bool>) {
        let kept = decide_kept(layers, direct);
        (Coverage::build(area, layers).unwrap(), kept)
    }

    #[test]
    fn stacking_overlap_masks_covered_part_of_kept_window() {
        // A (kept) at 0,0 10x10; B (masked) above at 5,5 10x10.
        let layers = [
            layer(1, PxRect::new(0, 0, 10, 10)),
            layer(2, PxRect::new(5, 5, 10, 10)),
        ];
        let area = PxRect::new(0, 0, 20, 20);
        let (cov, kept) = kept_grid(area, &layers, &[true, false]);
        assert!(cov.is_kept(&kept, 0, 0));
        assert!(cov.is_kept(&kept, 4, 9));
        assert!(!cov.is_kept(&kept, 5, 5), "covered by masked B");
        assert!(!cov.is_kept(&kept, 14, 14), "B itself");
        assert!(!cov.is_kept(&kept, 19, 0), "root");
    }

    #[test]
    fn kept_window_above_masked_one_shows() {
        let layers = [
            layer(1, PxRect::new(0, 0, 10, 10)),
            layer(2, PxRect::new(5, 5, 10, 10)),
        ];
        let (cov, kept) = kept_grid(PxRect::new(0, 0, 20, 20), &layers, &[false, true]);
        assert!(cov.is_kept(&kept, 5, 5));
        assert!(!cov.is_kept(&kept, 0, 0));
    }

    #[test]
    fn empty_allowlist_masks_everything() {
        let layers = [layer(1, PxRect::new(0, 0, 4, 4))];
        let area = PxRect::new(0, 0, 4, 4);
        let (cov, kept) = kept_grid(area, &layers, &[false]);
        let mut pixels = vec![0xFFu8; 4 * 4 * 3];
        cov.apply(&kept, &mut pixels).unwrap();
        assert!(pixels.chunks(3).all(|p| p == MASK_FILL));
    }

    #[test]
    fn apply_keeps_kept_pixels_untouched() {
        let layers = [layer(1, PxRect::new(0, 0, 2, 1))];
        let area = PxRect::new(0, 0, 3, 1);
        let (cov, kept) = kept_grid(area, &layers, &[true]);
        let mut pixels = vec![9u8; 9];
        cov.apply(&kept, &mut pixels).unwrap();
        assert_eq!(pixels, vec![9, 9, 9, 9, 9, 9, 0x1E, 0x1E, 0x1E]);
        assert!(cov.apply(&kept, &mut [0u8; 3]).is_err(), "size mismatch");
    }

    #[test]
    fn partially_off_screen_and_negative_origin() {
        // Kept window hangs off the top-left of the area.
        let layers = [layer(1, PxRect::new(-50, -50, 60, 60))];
        let area = PxRect::new(0, 0, 20, 20);
        let (cov, kept) = kept_grid(area, &layers, &[true]);
        assert!(cov.is_kept(&kept, 0, 0));
        assert!(cov.is_kept(&kept, 9, 9));
        assert!(!cov.is_kept(&kept, 10, 10));
        assert_eq!(cov.visible(1), vec![true]);
        // Entirely outside the area: not visible.
        let layers = [layer(1, PxRect::new(-50, -50, 10, 10))];
        let cov = Coverage::build(area, &layers).unwrap();
        assert_eq!(cov.visible(1), vec![false]);
    }

    #[test]
    fn multi_monitor_offset_area() {
        // Second monitor at 1920,0; zoom region 100x100 at monitor (10,10).
        let area = PxRect::new(1930, 10, 100, 100);
        let layers = [
            layer(1, PxRect::new(0, 0, 1920, 1080)), // on monitor 1, kept
            layer(2, PxRect::new(1900, 0, 80, 80)),  // straddles, masked
            layer(3, PxRect::new(1990, 50, 500, 500)), // kept
        ];
        let (cov, kept) = kept_grid(area, &layers, &[true, false, true]);
        assert_eq!(cov.visible(3), vec![false, true, true]);
        // root pixel (1930,10) is layer 2 -> masked
        assert!(!cov.is_kept(&kept, 0, 0));
        // (1990,50) -> layer 3
        assert!(cov.is_kept(&kept, 60, 40));
        // (1985,90) -> root
        assert!(!cov.is_kept(&kept, 55, 80));
    }

    #[test]
    fn frame_with_border_and_gtk_extents() {
        let outer = PxRect::new(100, 100, 400, 300);
        assert_eq!(visual_rect(outer, None, None), outer);
        assert_eq!(visual_rect(outer, Some(outer), None), outer);
        // CSD client = whole outer, 20px shadow left/right, 10 top, 30 bottom.
        assert_eq!(
            visual_rect(outer, Some(outer), Some([20, 20, 10, 30])),
            PxRect::new(120, 110, 360, 260)
        );
        // Extents bigger than the window: no pixels.
        assert!(visual_rect(outer, Some(outer), Some([300, 300, 0, 0])).is_empty());
    }

    #[test]
    fn override_redirect_ownership() {
        let base = |frame, pid, class: &str| Layer {
            frame,
            client: Some(frame),
            rect: Some(PxRect::new(0, 0, 1, 1)),
            pid: Some(pid),
            resource_class: Some(class.into()),
            ..Layer::default()
        };
        let kept_app = base(10, 100, "Firefox");
        let other_app = base(20, 200, "Xterm");
        // OR menu of the kept app (same pid + class), above it.
        let menu = Layer {
            override_redirect: true,
            ..base(30, 100, "firefox")
        };
        // OR tooltip of the other app.
        let tip = Layer {
            override_redirect: true,
            ..base(40, 200, "Xterm")
        };
        // OR popup, same pid as kept app but a different class (pid reuse
        // across PID namespaces): masked.
        let flatpak = Layer {
            override_redirect: true,
            ..base(50, 100, "Other")
        };
        // OR popup without pid but transient for the kept app.
        let transient = Layer {
            override_redirect: true,
            pid: None,
            transient_for: Some(10),
            ..base(60, 0, "")
        };
        // OR popup sharing the client leader.
        let leader = Layer {
            override_redirect: true,
            pid: None,
            leader: Some(77),
            ..base(70, 0, "x")
        };
        let mut kept_with_leader = kept_app.clone();
        kept_with_leader.leader = Some(77);
        let layers = [
            kept_with_leader,
            other_app,
            menu,
            tip,
            flatpak,
            transient,
            leader,
        ];
        let kept = decide_kept(&layers, &[true, false, false, false, false, false, false]);
        assert_eq!(kept, vec![true, false, true, false, false, true, true]);
    }

    #[test]
    fn override_redirect_below_kept_window_is_not_owned() {
        let popup = Layer {
            frame: 1,
            override_redirect: true,
            pid: Some(5),
            rect: Some(PxRect::new(0, 0, 1, 1)),
            ..Layer::default()
        };
        let app = Layer {
            frame: 2,
            pid: Some(5),
            rect: Some(PxRect::new(0, 0, 1, 1)),
            ..Layer::default()
        };
        assert_eq!(
            decide_kept(&[popup, app], &[false, true]),
            vec![false, true]
        );
    }

    #[test]
    fn managed_transient_dialog_is_kept_and_chains() {
        let parent = layer(1, PxRect::new(0, 0, 1, 1));
        let dialog = Layer {
            transient_for: Some(1),
            ..layer(2, PxRect::new(0, 0, 1, 1))
        };
        // Sub-dialog of the dialog, listed BELOW it: needs a second pass.
        let sub = Layer {
            transient_for: Some(2),
            ..layer(3, PxRect::new(0, 0, 1, 1))
        };
        let unrelated = layer(4, PxRect::new(0, 0, 1, 1));
        let layers = [sub, parent, dialog, unrelated];
        assert_eq!(
            decide_kept(&layers, &[false, true, false, false]),
            vec![true, true, true, false]
        );
    }

    #[test]
    fn report_lists_kept_hidden_missing_and_masked() {
        let layers = [
            layer(1, PxRect::new(0, 0, 10, 10)),  // kept, covered fully
            layer(2, PxRect::new(0, 0, 10, 10)),  // masked
            layer(3, PxRect::new(20, 0, 10, 10)), // kept, visible
            Layer {
                transient_for: Some(3),
                ..layer(4, PxRect::new(25, 0, 2, 2))
            },
        ];
        let request = MaskRequest::Keep(vec![1, 3, 99, 3]);
        let direct_of = vec![Some(0), Some(2), None, Some(2)];
        let mut direct = vec![false; 4];
        for i in direct_of.iter().flatten() {
            direct[*i] = true;
        }
        let kept = decide_kept(&layers, &direct);
        let cov = Coverage::build(PxRect::new(0, 0, 40, 10), &layers).unwrap();
        let visible = cov.visible(layers.len());
        let report = build_report(&request, &layers, &direct_of, &kept, &visible);
        assert_eq!(report.mode, "keep");
        assert_eq!(report.kept, vec!["3"]);
        assert_eq!(report.kept_hidden, vec!["1"]);
        assert_eq!(report.kept_missing, vec!["99"]);
        assert_eq!(report.auto_kept, vec!["4"]);
        assert_eq!(report.masked.len(), 1);
        assert_eq!(report.masked[0].id, "2");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["fill"], "#1E1E1E");
        assert!(json["masked"][0].get("title").is_none());
    }

    #[test]
    fn request_from_cli() {
        assert_eq!(MaskRequest::from_cli(vec![], false), None);
        assert_eq!(MaskRequest::from_cli(vec![], true), Some(MaskRequest::All));
        assert_eq!(
            MaskRequest::from_cli(vec![5], false),
            Some(MaskRequest::Keep(vec![5]))
        );
    }

    #[test]
    fn rect_intersect() {
        let a = PxRect::new(0, 0, 10, 10);
        assert_eq!(
            a.intersect(&PxRect::new(5, -5, 10, 10)),
            Some(PxRect::new(5, 0, 5, 5))
        );
        assert_eq!(a.intersect(&PxRect::new(10, 0, 5, 5)), None);
        assert_eq!(a.intersect(&PxRect::new(0, 0, 0, 5)), None);
    }
}
