//! Shared X11 connection state and RandR monitor enumeration.
//!
//! This is the one fully-implemented module in the v0.1 scaffold: every other
//! module (capture, input, wm, doctor) takes a `&Conn` and builds on the
//! monitor list and root window exposed here.

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as RandrConnectionExt;
use x11rb::protocol::xproto::{ConnectionExt as XprotoConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

/// A single RandR monitor in X11 root (global) pixel coordinates.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

/// The shared connection: a pure-Rust `RustConnection`, the default screen
/// index, and its root window (the drawable/coordinate origin for capture,
/// pointer, and window queries).
pub struct Conn {
    pub conn: RustConnection,
    /// Default screen index. Only read by the Xvfb integration tests (they build
    /// a focused window on this screen); the production subcommands work off
    /// `root` alone, so it is dead code in a non-test build.
    #[cfg_attr(not(test), allow(dead_code))]
    pub screen_num: usize,
    pub root: Window,
}

impl Conn {
    /// Connect to the X server named by `$DISPLAY` (or the passed `None`
    /// default) and resolve the default screen's root window.
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) =
            x11rb::connect(None).context("failed to connect to the X server (is $DISPLAY set?)")?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow!("X server reported no screen at index {screen_num}"))?
            .root;
        Ok(Self {
            conn,
            screen_num,
            root,
        })
    }

    /// Resolve an X11 ATOM to its string name (used to turn a RandR monitor's
    /// name atom into a human/JS-facing display id like `"DP-1"`).
    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        let reply = self
            .conn
            .get_atom_name(atom)
            .context("failed to request atom name")?
            .reply()
            .context("failed to read atom name reply")?;
        String::from_utf8(reply.name).context("atom name was not valid UTF-8")
    }

    /// Enumerate active RandR monitors on the root window.
    ///
    /// Returns them in the order the server reports. The JS side treats the
    /// first entry as index 0 and keys `--display` selection on each monitor's
    /// `name`, so the ordering must be stable within a session.
    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .context("failed to request RandR monitors")?
            .reply()
            .context("failed to read RandR monitor reply (is the RANDR extension present?)")?;

        let mut monitors = Vec::with_capacity(reply.monitors.len());
        for info in &reply.monitors {
            monitors.push(Monitor {
                name: self.atom_name(info.name)?,
                x: i32::from(info.x),
                y: i32::from(info.y),
                width: i32::from(info.width),
                height: i32::from(info.height),
                primary: info.primary,
            });
        }

        if monitors.is_empty() {
            bail!("RandR reported no active monitors");
        }
        Ok(monitors)
    }

    /// Round-trip with the server so every request already in the output buffer
    /// is sent AND fully processed before returning.
    ///
    /// XTEST input synthesis is fire-and-forget: `flush()` only guarantees the
    /// bytes reached the socket, not that the single-threaded server acted on
    /// them. A one-shot bridge process that only `flush()`es can exit (closing
    /// the connection) before the warp/click/keystroke is applied, so the input
    /// is silently lost. `sync()` issues a `GetInputFocus` whose reply the server
    /// cannot send until it has processed everything queued ahead of it, which
    /// is the standard xdotool `--sync` guarantee.
    pub fn sync(&self) -> Result<()> {
        use x11rb::wrapper::ConnectionExt as _;
        self.conn
            .sync()
            .context("failed to synchronize with the X server")
    }

    /// Map an optional `--display <name>` selector to a monitor.
    ///
    /// With a selector: match by monitor name, else error. Without: the primary
    /// monitor, else the first one.
    pub fn resolve_monitor(&self, selector: Option<&str>) -> Result<Monitor> {
        let monitors = self.monitors()?;
        if let Some(selector) = selector {
            return monitors
                .into_iter()
                .find(|monitor| monitor.name == selector)
                .ok_or_else(|| anyhow!("display `{selector}` not found"));
        }
        Ok(monitors
            .iter()
            .find(|monitor| monitor.primary)
            .or_else(|| monitors.first())
            .cloned()
            .expect("monitors() guarantees at least one entry"))
    }
}
