//! Environment / connection diagnostics.
//!
//! Reports the connected `$DISPLAY`, the X server vendor and protocol version,
//! the RandR monitor count, presence of the XTEST / RANDR / XFIXES extensions,
//! and the keyboard min/max keycode range. Human-facing only (the JS never
//! parses `doctor`); useful when triaging why capture or input is failing.

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;

use crate::conn::Conn;
use crate::output::{DoctorExtensions, DoctorKeyboard, DoctorReport};

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
