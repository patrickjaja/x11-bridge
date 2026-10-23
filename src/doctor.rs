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
use crate::output::{DoctorContract, DoctorExtensions, DoctorKeyboard, DoctorReport};

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

    let probed = Conn::connect().and_then(|conn| doctor(&conn));
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
    }
}

#[cfg(test)]
mod tests {
    use super::session_match;

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
