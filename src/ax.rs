//! AT-SPI subcommands (`ax-enable`, `ax-restore`, `ax-find`, `ax-action`) and
//! the `atspi` key of `doctor`, thin wrappers over the `cu-atspi` crate.
//!
//! These commands never open a portal and never touch the input or screenshot
//! paths. `ax-find` only reads the bridge's own window enumeration (the same
//! data `windows` prints) to fill the crate's `WindowHint`.

use std::io::Read;

use anyhow::{Context, Result, bail};
use cu_atspi::{Action, AtspiDoctor, FindRequest, Outcome, Rect, WindowHint};
use serde::Serialize;

use crate::cli::AxActionKind;
use crate::output::WindowInfo;

/// Parse a window id (`0x3a00007` or decimal) to a number for comparison.
fn parse_id(id: &str) -> Option<u64> {
    let id = id.trim();
    match id.strip_prefix("0x").or_else(|| id.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => id.parse().ok(),
    }
}

/// Whether `candidate` (a `windows` id) names the same window as `wanted`.
pub fn same_window_id(candidate: &str, wanted: &str) -> bool {
    if candidate.eq_ignore_ascii_case(wanted) {
        return true;
    }
    matches!((parse_id(candidate), parse_id(wanted)), (Some(a), Some(b)) if a == b)
}

/// `_NET_FRAME_EXTENTS` of a window as (left, right, top, bottom). Best effort:
/// `None` when the window manager sets none (undecorated or no EWMH WM).
pub fn frame_extents(conn: &crate::conn::Conn, window_id: &str) -> Option<[i32; 4]> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    let window = u32::try_from(parse_id(window_id)?).ok()?;
    let atom = conn
        .conn
        .intern_atom(true, b"_NET_FRAME_EXTENTS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if atom == 0 {
        return None;
    }
    let reply = conn
        .conn
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 4)
        .ok()?
        .reply()
        .ok()?;
    let v: Vec<u32> = reply.value32()?.collect();
    let [l, r, t, b] = <[u32; 4]>::try_from(v).ok()?;
    Some([l, r, t, b].map(|n| i32::try_from(n).unwrap_or(0)))
}

/// Fill the crate's `WindowHint` from one `windows` entry. X11 has pid
/// (`_NET_WM_PID`) and global geometry, so `x11` is true. The `windows`
/// geometry is the CLIENT area in root coordinates, so it is the client rect;
/// the frame rect is the client expanded by `_NET_FRAME_EXTENTS` when the
/// window manager sets them, else the client rect itself.
pub fn window_hint(window: &WindowInfo, extents: Option<[i32; 4]>) -> WindowHint {
    let g = &window.geometry;
    let client = Rect::new(g.x, g.y, g.width, g.height);
    let frame = match extents {
        Some([l, r, t, b]) => Rect::new(g.x - l, g.y - t, g.width + l + r, g.height + t + b),
        None => client,
    };
    WindowHint {
        id: Some(window.id.clone()),
        output: window.output.clone(),
        pid: window.pid,
        app_id: window.desktop_file_name.clone(),
        wm_class: window.resource_class.clone(),
        title: Some(window.title.clone()),
        rect_screen_logical: Some(frame),
        client_rect_screen_logical: Some(client),
        x11: true,
    }
}

pub struct FindArgs {
    pub window: String,
    pub role: Option<String>,
    pub title_contains: Option<String>,
    pub all: bool,
    pub include_hidden: bool,
    pub max_nodes: Option<usize>,
    pub max_depth: Option<u32>,
}

pub fn find_request(
    window: &WindowInfo,
    extents: Option<[i32; 4]>,
    args: &FindArgs,
) -> FindRequest {
    FindRequest {
        window: window_hint(window, extents),
        role: args.role.clone(),
        title_contains: args.title_contains.clone(),
        all: args.all,
        include_hidden: args.include_hidden,
        max_nodes: args.max_nodes,
        max_depth: args.max_depth,
    }
}

/// The `unavailable` result for a window id the bridge does not know.
pub fn window_not_found(id: &str) -> serde_json::Value {
    serde_json::json!({
        "unavailable": "window_not_found",
        "hint": format!(
            "No window with id {id} in this bridge's window list. Run `windows` for current ids; the window may have closed."
        ),
    })
}

/// Result of `ax-find`: the crate result verbatim, or our own unavailable.
#[derive(Serialize)]
#[serde(untagged)]
pub enum FindOut {
    Crate(Box<Outcome<cu_atspi::FindResult>>),
    Local(serde_json::Value),
}

pub fn find(conn: &crate::conn::Conn, windows: &[WindowInfo], args: &FindArgs) -> Result<FindOut> {
    let Some(window) = windows.iter().find(|w| same_window_id(&w.id, &args.window)) else {
        return Ok(FindOut::Local(window_not_found(&args.window)));
    };
    let extents = frame_extents(conn, &window.id);
    let req = find_request(window, extents, args);
    let out = cu_atspi::blocking::find(&req).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(FindOut::Crate(Box::new(out)))
}

pub fn enable(holder_pid: u32) -> Result<Outcome<cu_atspi::EnableResult>> {
    cu_atspi::blocking::enable(holder_pid).map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn restore(holder_pid: u32) -> Result<Outcome<cu_atspi::RestoreResult>> {
    cu_atspi::blocking::restore(holder_pid).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Caller error (bad input): the bridge exits 2 instead of 1.
#[derive(Debug)]
pub struct UsageError(pub &'static str);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UsageError {}

/// Read all of `reader` as UTF-8, no trimming. Text never comes from argv
/// (visible in `ps`) and is never logged.
pub fn read_text(mut reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .context("failed to read text from stdin")?;
    String::from_utf8(bytes)
        .map_err(|_| anyhow::Error::new(UsageError("stdin text is not valid UTF-8")))
}

pub fn build_action(kind: AxActionKind, stdin: impl Read) -> Result<Action> {
    Ok(match kind {
        AxActionKind::Click => Action::Click,
        AxActionKind::Focus => Action::Focus,
        AxActionKind::SetText => Action::SetText(read_text(stdin)?),
    })
}

pub fn action(
    scope: &str,
    element_ref: &str,
    kind: AxActionKind,
    stdin: impl Read,
) -> Result<Outcome<cu_atspi::ActionResult>> {
    if scope.is_empty() || element_ref.is_empty() {
        bail!("--scope and --ref must not be empty");
    }
    let action = build_action(kind, stdin)?;
    cu_atspi::blocking::action(element_ref, &action, scope).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Longest `doctor` waits for the AT-SPI probe (the launcher kills doctor at 5 s).
const DOCTOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run `probe` on a thread and give up after `limit`; the thread is detached.
pub fn doctor_with(
    probe: impl FnOnce() -> AtspiDoctor + Send + 'static,
    limit: std::time::Duration,
) -> AtspiDoctor {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(probe());
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| AtspiDoctor {
        error: Some("accessibility probe timed out".into()),
        ..Default::default()
    })
}

/// Value of the `atspi` key in `doctor`. Never fails and is capped at 3 s; an
/// error or timeout is reported inside the value.
pub fn doctor() -> AtspiDoctor {
    doctor_with(cu_atspi::blocking::doctor, DOCTOR_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Rect as OutRect;

    fn window() -> WindowInfo {
        WindowInfo {
            id: "0x3a00007".into(),
            title: "Untitled 1 - Mousepad".into(),
            geometry: OutRect {
                x: 100,
                y: 50,
                width: 800,
                height: 600,
            },
            pid: Some(4242),
            desktop_file_name: Some("org.xfce.mousepad".into()),
            resource_class: Some("Mousepad".into()),
            resource_name: None,
            window_role: None,
            window_type: None,
            is_dock: None,
            is_desktop: None,
            is_visible: Some(true),
            is_minimized: None,
            is_normal_window: Some(true),
            is_dialog: None,
            transient: None,
            transient_for: None,
            output: Some("DP-1".into()),
            stacking_order: 0,
            is_active: true,
            exclude_from_capture: false,
            keep_above: None,
        }
    }

    #[test]
    fn hint_maps_window_fields() {
        let h = window_hint(&window(), None);
        assert_eq!(h.id.as_deref(), Some("0x3a00007"));
        assert_eq!(h.pid, Some(4242));
        assert_eq!(h.app_id.as_deref(), Some("org.xfce.mousepad"));
        assert_eq!(h.wm_class.as_deref(), Some("Mousepad"));
        assert_eq!(h.title.as_deref(), Some("Untitled 1 - Mousepad"));
        assert_eq!(h.output.as_deref(), Some("DP-1"));
        assert_eq!(h.rect_screen_logical, Some(Rect::new(100, 50, 800, 600)));
        assert_eq!(
            h.client_rect_screen_logical,
            Some(Rect::new(100, 50, 800, 600))
        );
        assert!(h.x11);
    }

    #[test]
    fn hint_expands_frame_by_extents() {
        let h = window_hint(&window(), Some([1, 2, 30, 3]));
        assert_eq!(h.rect_screen_logical, Some(Rect::new(99, 20, 803, 633)));
        assert_eq!(
            h.client_rect_screen_logical,
            Some(Rect::new(100, 50, 800, 600))
        );
    }

    #[test]
    fn doctor_probe_times_out() {
        let d = doctor_with(
            || {
                std::thread::sleep(std::time::Duration::from_secs(2));
                AtspiDoctor::default()
            },
            std::time::Duration::from_millis(50),
        );
        assert!(!d.bus);
        assert_eq!(d.error.as_deref(), Some("accessibility probe timed out"));
    }

    #[test]
    fn window_id_matching() {
        assert!(same_window_id("0x3a00007", "0x3a00007"));
        assert!(same_window_id("0x3a00007", "0X3A00007"));
        assert!(same_window_id("0x3a00007", &0x3a00007u64.to_string()));
        assert!(!same_window_id("0x3a00007", "0x3a00008"));
        assert!(!same_window_id("0x3a00007", "nope"));
    }

    #[test]
    fn unknown_window_is_unavailable_exit_zero_shape() {
        let args = FindArgs {
            window: "0x1".into(),
            role: None,
            title_contains: None,
            all: false,
            include_hidden: false,
            max_nodes: None,
            max_depth: None,
        };
        let _ = &args;
        let out = FindOut::Local(window_not_found("0x1"));
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["unavailable"], "window_not_found");
        assert!(v["hint"].as_str().unwrap().contains("windows"));
    }

    #[test]
    fn find_request_carries_flags() {
        let args = FindArgs {
            window: "0x3a00007".into(),
            role: Some("AXButton".into()),
            title_contains: Some("Save".into()),
            all: true,
            include_hidden: true,
            max_nodes: Some(10),
            max_depth: Some(4),
        };
        let r = find_request(&window(), None, &args);
        assert_eq!(r.role.as_deref(), Some("AXButton"));
        assert_eq!(r.title_contains.as_deref(), Some("Save"));
        assert!(r.all && r.include_hidden);
        assert_eq!((r.max_nodes, r.max_depth), (Some(10), Some(4)));
    }

    #[test]
    fn set_text_reads_all_stdin_untrimmed() {
        let a = build_action(AxActionKind::SetText, "  héllo\n".as_bytes()).unwrap();
        assert_eq!(a, Action::SetText("  héllo\n".into()));
        let e = build_action(AxActionKind::SetText, &[0xff, 0xfe][..]).unwrap_err();
        assert!(e.downcast_ref::<UsageError>().is_some());
    }

    #[test]
    fn click_and_focus_ignore_stdin() {
        assert_eq!(
            build_action(AxActionKind::Click, &b"x"[..]).unwrap(),
            Action::Click
        );
        assert_eq!(
            build_action(AxActionKind::Focus, &b"x"[..]).unwrap(),
            Action::Focus
        );
    }
}
