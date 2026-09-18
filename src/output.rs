//! Shared, serde-serializable result types and JSON printing helpers.
//!
//! Field names here are the contract with the JS executor
//! (`js/cu_linux_executor.js` in claude-desktop-extra). They mirror the
//! kwin-portal-bridge JSON shapes 1:1 so the X11 bridge is a drop-in
//! substitute on X11 sessions. Do NOT rename fields without updating the
//! JS parser in lockstep.

use anyhow::{Context, Result};
use serde::Serialize;

/// Print any serializable value as a single compact JSON object/array on stdout.
///
/// This is the ONLY success-path output the JS side reads (it does
/// `JSON.parse(stdout.trim())`), so every subcommand handler funnels its
/// result through here.
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let rendered = serde_json::to_string(value).context("failed to serialize JSON result")?;
    println!("{rendered}");
    Ok(())
}

/// A logical rectangle in X11 root (global) pixel coordinates.
///
/// Matches kwin-portal-bridge `Rect` and the nested `geometry` object the JS
/// reads from `screens` / `windows`.
#[derive(Debug, Clone, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A single monitor/screen entry.
///
/// JS (`mapRustScreenToDisplay`) reads: `geometry.{x,y,width,height}`, `scale`,
/// `is_primary`, `name`, `id`. `id` becomes the display's `_bridgeId` and is
/// what `--display <name>` selects on. `output` mirrors `id` for parity with
/// the kwin bridge (windows reference their screen by this name).
#[derive(Debug, Clone, Serialize)]
pub struct Screen {
    pub id: String,
    pub name: String,
    pub geometry: Rect,
    /// X11 has no per-monitor logical scale exposed here; always 1.0 for v0.1.
    /// (Xft.dpi-derived scaling is a future enhancement — see DESIGN.md.)
    pub scale: Option<f64>,
    pub refresh_millihz: Option<u32>,
    pub is_active: bool,
    pub is_primary: bool,
}

/// A screenshot of a full monitor. camelCase to match kwin-portal-bridge
/// `ScreenshotResult` and the JS `screenshotResultFromRust` reader
/// (`result.displayWidth`, `result.originX`, ...).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    pub base64: String,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub display_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
}

/// A region (zoom) capture. Matches kwin-portal-bridge `ScreenshotCapture`
/// and the JS `zoom` reader (`result.base64`, `result.width`, `result.height`).
#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotCapture {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// A window entry. Field names match kwin-portal-bridge `WindowInfo` and the
/// JS `normalizeBundleIdFromWindow` / `dockableWindow` / window readers exactly.
/// Snake_case (NOT camelCase) — the JS reads `window.desktop_file_name`,
/// `window.resource_class`, `window.is_minimized`, `window.stacking_order`, etc.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub geometry: Rect,
    pub pid: Option<u32>,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub window_role: Option<String>,
    pub window_type: Option<String>,
    pub is_dock: Option<bool>,
    pub is_desktop: Option<bool>,
    pub is_visible: Option<bool>,
    pub is_minimized: Option<bool>,
    pub is_normal_window: Option<bool>,
    pub is_dialog: Option<bool>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
    /// Name of the screen this window is on; matches a `Screen::id`.
    pub output: Option<String>,
    pub stacking_order: usize,
    pub is_active: bool,
    pub exclude_from_capture: bool,
    pub keep_above: Option<bool>,
}

/// A reference up a window's transient (parent) chain. Mirrors the kwin bridge
/// `WindowAppRef`; JS `normalizeBundleIdFromWindowRef` walks it recursively.
#[derive(Debug, Clone, Serialize)]
pub struct WindowAppRef {
    pub id: String,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
}

/// The frontmost / under-point app identity. camelCase to match the JS
/// `commandOutputToAppRef` reader (`result.bundleId`, `result.displayName`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRef {
    pub bundle_id: String,
    pub display_name: String,
}

/// Current global cursor position in X11 root coordinates. JS reads `.x`/`.y`.
#[derive(Debug, Clone, Serialize)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

/// One probed capability/tool for `doctor`. Free-form; only consumed by humans.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub display: String,
    pub server_vendor: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub screen_count: usize,
    pub extensions: DoctorExtensions,
    pub keyboard: DoctorKeyboard,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorExtensions {
    pub xtest: bool,
    pub randr: bool,
    pub xfixes: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorKeyboard {
    /// Min/max keycode range reported by the server.
    pub min_keycode: u8,
    pub max_keycode: u8,
}
