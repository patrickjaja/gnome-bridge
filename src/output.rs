//! Shared, serde-serializable result types and JSON printing helpers.
//!
//! Field names here are the contract with the JS executor
//! (`js/executor_linux.js` in claude-desktop-bin). They mirror the
//! kwin-portal-bridge and x11-bridge JSON shapes 1:1 so this GNOME bridge is a
//! drop-in substitute on GNOME/Wayland sessions. Do NOT rename fields without
//! updating the JS parser in lockstep.
//!
//! Casing is deliberately non-uniform, matching the sibling bridges and the JS
//! reader: screenshots and app-refs are camelCase; screens and windows are
//! snake_case.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

/// A logical rectangle in global (compositor) pixel coordinates. Matches the
/// nested `geometry` object the JS reads from `screens` / `windows`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// what `--display <name>` selects on (the Mutter connector name, e.g. `eDP-1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Screen {
    pub id: String,
    pub name: String,
    pub geometry: Rect,
    /// Mutter logical scale (can be fractional, e.g. 1.25, 2.0).
    pub scale: Option<f64>,
    pub refresh_millihz: Option<u32>,
    pub is_active: bool,
    pub is_primary: bool,
}

/// A screenshot of a full monitor. camelCase to match the JS
/// `screenshotResultFromRust` reader (`result.displayWidth`, `result.originX`, ...).
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

/// A region (zoom) capture. Matches the JS `zoom` reader
/// (`result.base64`, `result.width`, `result.height`).
#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotCapture {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// A window entry. Field names match kwin-portal-bridge / x11-bridge `WindowInfo`
/// and the JS `normalizeBundleIdFromWindow` / `dockableWindow` readers exactly.
/// Snake_case (NOT camelCase). On GNOME this is best-effort via the Shell
/// Introspect D-Bus API, which is allowlist-restricted; when it is unavailable
/// the `windows` command returns an empty array (see DESIGN.md).
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

/// A reference up a window's transient (parent) chain. Mirrors the kwin/x11
/// bridge `WindowAppRef`. GNOME Introspect does not expose a transient chain, so
/// this is always `None` in practice; the type exists for JSON parity.
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
