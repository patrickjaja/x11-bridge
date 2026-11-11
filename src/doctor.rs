//! Environment / connection diagnostics.
//!
//! Reports the connected `$DISPLAY`, the X server vendor and protocol version,
//! the RandR monitor count, presence of the XTEST / RANDR / XFIXES extensions,
//! and the keyboard min/max keycode range. Human-facing only (the JS never
//! parses the extras); useful when triaging why capture or input is failing.
//! The leading keys follow the Computer Use bridge CLI contract v1 and `doctor`
//! always succeeds, even without an X display.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;

use crate::conn::Conn;
use crate::output::{
    DoctorCapabilities, DoctorContract, DoctorExtensions, DoctorKeyboard, DoctorReport,
};

/// Whether the named extension is present on the server.
///
/// `query_extension` is the portable presence probe: it works for any
/// extension name and does not require the extension's own request opcodes to
/// be wired up, so it is a safe superset of "the XTEST/RANDR/XFIXES features we
/// use are available".
fn has_extension(conn: &Conn, name: &str) -> Result<bool> {
    let reply = conn
        .conn
        .query_extension(name.as_bytes())
        .with_context(|| format!("failed to query extension `{name}`"))?
        .reply()
        .with_context(|| format!("failed to read extension reply for `{name}`"))?;
    Ok(reply.present)
}

/// Gather connection info and extension availability into a `DoctorReport`.
pub fn doctor(conn: &Conn) -> Result<DoctorReport> {
    let setup = conn.conn.setup();

    let display = std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
    let server_vendor = String::from_utf8_lossy(&setup.vendor).into_owned();

    // Screen count comes from RandR monitors so it matches what `screens`
    // enumerates; fall back to the X core screen count if RandR is absent.
    let screen_count = conn
        .monitors()
        .map(|monitors| monitors.len())
        .unwrap_or_else(|_| setup.roots.len());

    let extensions = DoctorExtensions {
        xtest: has_extension(conn, "XTEST")?,
        randr: has_extension(conn, "RANDR")?,
        xfixes: has_extension(conn, "XFIXES")?,
    };

    let keyboard = DoctorKeyboard {
        min_keycode: setup.min_keycode,
        max_keycode: setup.max_keycode,
    };

    Ok(DoctorReport {
        display,
        server_vendor,
        protocol_major: setup.protocol_major_version,
        protocol_minor: setup.protocol_minor_version,
        screen_count,
        extensions,
        keyboard,
    })
}

/// Whether a compositing manager owns `_NET_WM_CM_S<screen>` (EWMH
/// "Compositing Managers"). One InternAtom + one GetSelectionOwner round trip.
pub fn compositing(conn: &Conn) -> Result<bool> {
    let name = format!("_NET_WM_CM_S{}", conn.screen_num);
    let atom = conn
        .conn
        .intern_atom(false, name.as_bytes())
        .with_context(|| format!("failed to request atom {name}"))?
        .reply()
        .with_context(|| format!("failed to intern atom {name}"))?
        .atom;
    let owner = conn
        .conn
        .get_selection_owner(atom)
        .context("failed to request the compositing manager selection owner")?
        .reply()
        .context("failed to read the compositing manager selection owner")?
        .owner;
    Ok(owner != x11rb::NONE)
}

/// `(compositing, compositing_error)` for the doctor: never fails.
pub fn compositing_field(conn: Result<&Conn, &anyhow::Error>) -> (Option<bool>, Option<String>) {
    match conn {
        Err(error) => (None, Some(format!("no X connection: {error:#}"))),
        Ok(conn) => match compositing(conn) {
            Ok(state) => (Some(state), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        },
    }
}

/// Whether this bridge serves the current graphical session: `$DISPLAY` is set
/// and the session is X11. On a Wayland session x11-bridge is only the XWayland
/// fallback behind the Wayland bridges, so it does not claim the session. When
/// `XDG_SESSION_TYPE` is unset, empty or unknown (`tty`, ...), a reachable
/// `$DISPLAY` is enough.
pub fn session_match(display_set: bool, session_type: Option<&str>, reachable: bool) -> bool {
    if !display_set {
        return false;
    }
    match session_type
        .map(|t| t.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("x11") => true,
        Some("wayland") => false,
        _ => reachable,
    }
}

/// Build the contract `doctor` report. Never fails: connection errors become
/// `backend_reachable: false` plus `backend_error`.
pub fn contract_report() -> DoctorContract {
    let display_set = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();

    let conn = Conn::connect();
    let (compositing, compositing_error) = compositing_field(conn.as_ref());
    let probed = conn.and_then(|conn| doctor(&conn));
    let (details, backend_error) = match probed {
        Ok(report) => (Some(report), None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let backend_reachable = details.is_some();

    DoctorContract {
        contract: 1,
        bridge: "x11-bridge",
        version: env!("CARGO_PKG_VERSION"),
        backend: "x11",
        session_match: session_match(display_set, session_type.as_deref(), backend_reachable),
        backend_reachable,
        backend_error,
        session_supported: false,
        restore_token_present: false,
        restore_token_path: None,
        daemon_running: false,
        session_socket: None,
        details,
        atspi: crate::ax::doctor(),
        capabilities: DoctorCapabilities {
            screenshot_mask: true,
            screenshot_mask_ignore: true,
            compositing,
            compositing_error,
        },
        abort_watch: crate::abort::doctor(),
    }
}

#[cfg(test)]
mod tests {
    use super::{compositing_field, session_match};

    /// Owning `_NET_WM_CM_S<screen>` (what a compositing manager does) flips
    /// the answer on the next read; releasing it flips it back.
    #[test]
    #[ignore = "requires a live X server; run under scripts/test-xvfb.sh"]
    fn compositing_follows_the_cm_selection_owner() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        use x11rb::wrapper::ConnectionExt as _;
        let probe = crate::conn::Conn::connect().expect("connect to Xvfb");
        assert!(!super::compositing(&probe).unwrap(), "bare Xvfb");
        let cm = crate::conn::Conn::connect().unwrap();
        let win = cm.conn.generate_id().unwrap();
        cm.conn
            .create_window(
                0,
                win,
                cm.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap();
        let name = format!("_NET_WM_CM_S{}", cm.screen_num);
        let atom = cm
            .conn
            .intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        cm.conn
            .set_selection_owner(win, atom, x11rb::CURRENT_TIME)
            .unwrap();
        cm.conn.sync().unwrap();
        assert!(super::compositing(&probe).unwrap(), "owned");
        assert_eq!(compositing_field(Ok(&probe)), (Some(true), None));
        drop(cm); // the owner's connection closes: selection released
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!super::compositing(&probe).unwrap(), "released");
    }

    #[test]
    fn compositing_unknown_without_a_display() {
        let error = anyhow::anyhow!("cannot open display");
        let (state, why) = compositing_field(Err(&error));
        assert_eq!(state, None);
        assert!(why.unwrap().contains("cannot open display"));
    }

    #[test]
    fn session_match_rules() {
        assert!(!session_match(false, Some("x11"), true));
        assert!(session_match(true, Some("x11"), false));
        assert!(session_match(true, Some("X11"), false));
        assert!(!session_match(true, Some("wayland"), true));
        assert!(session_match(true, None, true));
        assert!(session_match(true, Some("tty"), true));
        assert!(!session_match(true, None, false));
    }
}
