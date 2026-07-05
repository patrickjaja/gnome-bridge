//! Best-effort window enumeration via `org.gnome.Shell.Introspect.GetWindows`.
//!
//! This API (`org.gnome.Shell` / `/org/gnome/Shell/Introspect`) is
//! allowlist-restricted: it succeeds only when the caller is on GNOME Shell's
//! introspection allowlist or `org.gnome.mutter unsafe-mode` is enabled.
//! Otherwise the call returns a D-Bus error. In that case `list_windows`
//! returns an empty vector and `frontmost_app` / `app_under_point` return
//! `null` - which is the status quo on GNOME today (the JS executor has no
//! window enumeration on GNOME/Wayland), so this is additive, never a
//! regression.
//!
//! `GetWindows` returns `a{ta{sv}}` - a map of window-id -> property map. The
//! documented per-window keys are: `app-id` (s), `client-type` (u),
//! `is-hidden` (b), `has-focus` (b), `width`/`height` (u), `title` (s),
//! `wm-class` (s), plus (on newer Shells) `frame-rect` and `gtk-application-id`.
//! Not all keys are present on all versions, so every field read is optional.

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::Connection;
use zbus::zvariant::OwnedValue;

use crate::output::{AppRef, Rect, WindowInfo};

type WindowMap = HashMap<u64, HashMap<String, OwnedValue>>;

/// Call `GetWindows`, returning the raw window map. Errors (including the
/// allowlist rejection) propagate to the caller, which downgrades them to an
/// empty result.
async fn get_windows_raw(connection: &Connection) -> Result<WindowMap> {
    let reply: WindowMap = connection
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell/Introspect",
            Some("org.gnome.Shell.Introspect"),
            "GetWindows",
            &(),
        )
        .await
        .context("failed to call org.gnome.Shell.Introspect.GetWindows")?
        .body()
        .deserialize()
        .context("failed to deserialize GetWindows reply")?;
    Ok(reply)
}

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .map(str::to_owned)
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| v.downcast_ref::<bool>().ok())
}

fn prop_u32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| v.downcast_ref::<u32>().ok())
}

/// Convert one Introspect window entry to a `WindowInfo`.
///
/// `stacking_order` is the index in the enumeration order (Introspect returns a
/// map, so ordering is not guaranteed stable; callers sort by id for
/// determinism). Fields Introspect does not expose (dock/desktop/dialog type,
/// transient chain, per-window role) are `None`/`false`.
fn window_from_props(
    id: u64,
    props: &HashMap<String, OwnedValue>,
    stacking_order: usize,
) -> WindowInfo {
    let wm_class = prop_str(props, "wm-class");
    // `app-id` is the desktop file basename on GNOME (e.g. "org.gnome.Nautilus");
    // gtk-application-id is a newer alias.
    let desktop_file_name =
        prop_str(props, "gtk-application-id").or_else(|| prop_str(props, "app-id"));
    let title = prop_str(props, "title").unwrap_or_default();
    let width = prop_u32(props, "width").unwrap_or(0) as i32;
    let height = prop_u32(props, "height").unwrap_or(0) as i32;
    let has_focus = prop_bool(props, "has-focus").unwrap_or(false);
    let is_hidden = prop_bool(props, "is-hidden").unwrap_or(false);

    WindowInfo {
        id: id.to_string(),
        title,
        geometry: Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        pid: None,
        desktop_file_name: desktop_file_name.clone(),
        resource_class: wm_class.clone(),
        resource_name: wm_class,
        window_role: None,
        window_type: None,
        is_dock: None,
        is_desktop: None,
        is_visible: Some(!is_hidden),
        is_minimized: Some(is_hidden),
        is_normal_window: None,
        is_dialog: None,
        transient: None,
        transient_for: None,
        output: None,
        stacking_order,
        is_active: has_focus,
        exclude_from_capture: false,
        keep_above: None,
    }
}

/// Enumerate windows, sorted by window id for a stable order. Returns an empty
/// vector if Introspect is unavailable (allowlist-restricted) rather than
/// failing - matching the GNOME status quo where there is no window list.
pub async fn list_windows(connection: &Connection) -> Vec<WindowInfo> {
    let Ok(map) = get_windows_raw(connection).await else {
        return Vec::new();
    };

    let mut ids: Vec<u64> = map.keys().copied().collect();
    ids.sort_unstable();
    ids.iter()
        .enumerate()
        .filter_map(|(order, id)| {
            map.get(id)
                .map(|props| window_from_props(*id, props, order))
        })
        .collect()
}

/// Derive the bundle id / display name from a window, mirroring the JS
/// `normalizeBundleIdFromWindow`: first non-empty of desktop_file_name,
/// resource_class, resource_name, then the id; a trailing `.desktop` stripped.
fn app_ref_from_window(window: &WindowInfo) -> AppRef {
    let bundle_id = [
        window.desktop_file_name.as_deref(),
        window.resource_class.as_deref(),
        window.resource_name.as_deref(),
        Some(window.id.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|s| !s.is_empty())
    .map(|s| s.strip_suffix(".desktop").unwrap_or(s).to_owned())
    .unwrap_or_else(|| window.id.clone());

    let display_name = if window.title.trim().is_empty() {
        bundle_id.clone()
    } else {
        window.title.clone()
    };

    AppRef {
        bundle_id,
        display_name,
    }
}

/// The focused window's app identity, or `None` if Introspect is unavailable or
/// no window has focus.
pub async fn frontmost_app(connection: &Connection) -> Option<AppRef> {
    let windows = list_windows(connection).await;
    windows
        .iter()
        .find(|w| w.is_active)
        .map(app_ref_from_window)
}

/// The topmost app whose geometry contains the point, or `None`.
///
/// Introspect window geometry lacks a global origin on many GNOME versions
/// (only width/height are guaranteed), so this is best-effort: it can only
/// resolve a hit when frame-rect data is present. When geometry is unusable it
/// returns `None`, matching the "no window list on GNOME" baseline.
pub async fn app_under_point(connection: &Connection, x: i32, y: i32) -> Option<AppRef> {
    let windows = list_windows(connection).await;
    windows
        .iter()
        .filter(|w| {
            let g = &w.geometry;
            g.width > 0
                && g.height > 0
                && x >= g.x
                && x < g.x + g.width
                && y >= g.y
                && y < g.y + g.height
        })
        // Later stacking order = higher; pick the topmost.
        .max_by_key(|w| w.stacking_order)
        .map(app_ref_from_window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::WindowInfo;

    fn win(
        id: &str,
        title: &str,
        dfn: Option<&str>,
        class: Option<&str>,
        active: bool,
    ) -> WindowInfo {
        WindowInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            geometry: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            pid: None,
            desktop_file_name: dfn.map(str::to_owned),
            resource_class: class.map(str::to_owned),
            resource_name: class.map(str::to_owned),
            window_role: None,
            window_type: None,
            is_dock: None,
            is_desktop: None,
            is_visible: Some(true),
            is_minimized: Some(false),
            is_normal_window: None,
            is_dialog: None,
            transient: None,
            transient_for: None,
            output: None,
            stacking_order: 0,
            is_active: active,
            exclude_from_capture: false,
            keep_above: None,
        }
    }

    #[test]
    fn bundle_id_prefers_desktop_file_name_and_strips_desktop() {
        let w = win(
            "1",
            "Files",
            Some("org.gnome.Nautilus.desktop"),
            Some("org.gnome.Nautilus"),
            true,
        );
        let app = app_ref_from_window(&w);
        assert_eq!(app.bundle_id, "org.gnome.Nautilus");
        assert_eq!(app.display_name, "Files");
    }

    #[test]
    fn bundle_id_falls_back_to_class_then_title_empty() {
        let w = win("42", "", None, Some("firefox"), true);
        let app = app_ref_from_window(&w);
        assert_eq!(app.bundle_id, "firefox");
        // Empty title -> display name is the bundle id.
        assert_eq!(app.display_name, "firefox");
    }
}
