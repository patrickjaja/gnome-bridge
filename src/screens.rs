//! Monitor enumeration via `org.gnome.Mutter.DisplayConfig.GetCurrentState`.
//!
//! This is a public session-bus API on GNOME (the same one gnome-control-center
//! uses). It returns logical monitors with their position, size, scale, primary
//! flag, and the connector/vendor/product of each physical monitor assigned to
//! them. We map that to the shared `Screen` JSON shape (snake_case, matching
//! kwin-portal-bridge / x11-bridge).
//!
//! Fallback: if the Mutter API is unavailable (non-GNOME compositor exposing the
//! portal, or DisplayConfig missing), the caller derives screens from the portal
//! ScreenCast streams instead (see `portal.rs`).

use anyhow::{Context, Result};
use zbus::Connection;
use zbus::zvariant::OwnedValue;

use crate::output::{Rect, Screen};

/// The `GetCurrentState` reply signature (see the Mutter DisplayConfig docs):
///
/// `(u a((ssss)a(siiddada{sv})a{sv}) a(iiduba(ssss)a{sv}) a{sv})`
///
/// We only need the third element, the array of *logical* monitors:
///   `a(i i d u b a(ssss) a{sv})`
///     = (x, y, scale, transform, primary, [ (connector, vendor, product, serial) ], props)
type LogicalMonitor = (
    i32,                                           // x
    i32,                                           // y
    f64,                                           // scale
    u32,                                           // transform
    bool,                                          // primary
    Vec<(String, String, String, String)>,         // monitors: (connector, vendor, product, serial)
    std::collections::HashMap<String, OwnedValue>, // properties
);

/// A physical monitor descriptor, the second element of `GetCurrentState`:
///   `a((ssss) a(siiddada{sv}) a{sv})`
/// We need the connector -> current-mode (width/height/refresh) association so
/// a logical monitor's pixel geometry can be computed.
type PhysicalMonitor = (
    (String, String, String, String), // (connector, vendor, product, serial)
    Vec<PhysicalMode>,                // modes
    std::collections::HashMap<String, OwnedValue>, // properties
);

/// A mode: `(s i i d d a d a{sv})` = (id, width, height, refresh,
/// preferred_scale, supported_scales, props).
///
/// The shape must match Mutter's wire signature `(siiddada{sv})` EXACTLY:
/// zbus checks the body signature against this type before decoding, so a
/// missing field is not a partial read but a hard "signature mismatch" error
/// for the whole call. The first release of this bridge dropped the
/// `preferred_scale` double, which made every `GetCurrentState` call fail and
/// silently pushed `screens` (and every pointer/capture command, which resolve
/// screens the same way) onto the portal-stream fallback - one consent dialog
/// per command (claude-desktop-extra issue #293).
type PhysicalMode = (
    String,                                        // mode id
    i32,                                           // width (physical pixels)
    i32,                                           // height (physical pixels)
    f64,                                           // refresh (Hz)
    f64,                                           // preferred scale
    Vec<f64>,                                      // supported scales
    std::collections::HashMap<String, OwnedValue>, // properties (contains "is-current")
);

type GetCurrentStateReply = (
    u32,                                           // serial
    Vec<PhysicalMonitor>,                          // monitors
    Vec<LogicalMonitor>,                           // logical monitors
    std::collections::HashMap<String, OwnedValue>, // properties
);

/// Fetch logical monitors from Mutter and map them to `Screen` entries.
///
/// Returns an error if the DisplayConfig service is not reachable; callers may
/// then fall back to the portal streams.
pub async fn list_screens(connection: &Connection) -> Result<Vec<Screen>> {
    let reply: GetCurrentStateReply = connection
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await
        .context("failed to call org.gnome.Mutter.DisplayConfig.GetCurrentState")?
        .body()
        .deserialize()
        .context("failed to deserialize DisplayConfig GetCurrentState reply")?;

    let (_serial, physical, logical, _props) = reply;
    Ok(map_logical_monitors(&physical, &logical))
}

/// Pure mapping from the DisplayConfig reply structs to `Screen` entries,
/// factored out so it can be unit-tested against canned data.
pub fn map_logical_monitors(
    physical: &[PhysicalMonitor],
    logical: &[LogicalMonitor],
) -> Vec<Screen> {
    let single_head = logical.len() == 1;

    logical
        .iter()
        .map(|(x, y, scale, _transform, primary, monitors, _props)| {
            // A logical monitor's connector name is the first assigned physical
            // monitor's connector; use it as both `id` and `name`.
            let connector = monitors
                .first()
                .map(|(c, _, _, _)| c.clone())
                .unwrap_or_default();

            // Logical (layout) size = current physical mode size / scale, since
            // GNOME layouts in logical coordinates. The portal / cursor
            // coordinate space the JS uses is logical, so report logical size.
            let (phys_w, phys_h, refresh) = connector_current_mode(physical, &connector);
            let logical_w = (f64::from(phys_w) / scale).round() as i32;
            let logical_h = (f64::from(phys_h) / scale).round() as i32;

            let is_primary = *primary || single_head;

            Screen {
                geometry: Rect {
                    x: *x,
                    y: *y,
                    width: logical_w.max(1),
                    height: logical_h.max(1),
                },
                scale: Some(*scale),
                refresh_millihz: refresh.map(|r| (r * 1000.0).round() as u32),
                is_active: is_primary,
                is_primary,
                name: connector.clone(),
                id: connector,
            }
        })
        .collect()
}

/// Find the current mode (width, height, refresh) for a connector, if present.
fn connector_current_mode(
    physical: &[PhysicalMonitor],
    connector: &str,
) -> (i32, i32, Option<f64>) {
    for ((c, _, _, _), modes, _props) in physical {
        if c != connector {
            continue;
        }
        for (_id, width, height, refresh, _preferred, _scales, props) in modes {
            if mode_is_current(props) {
                return (*width, *height, Some(*refresh));
            }
        }
        // No mode flagged current: use the first as a best effort.
        if let Some((_id, width, height, refresh, _pref, _s, _p)) = modes.first() {
            return (*width, *height, Some(*refresh));
        }
    }
    (0, 0, None)
}

/// Whether a mode's property map flags it as the current mode.
fn mode_is_current(props: &std::collections::HashMap<String, OwnedValue>) -> bool {
    props
        .get("is-current")
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{Type, Value};

    /// Pin the decoder shape to Mutter's published `GetCurrentState` signature
    /// (`data/dbus-interfaces/org.gnome.Mutter.DisplayConfig.xml`). zbus
    /// refuses to decode a reply whose body signature differs from this, so a
    /// drift here is a total loss of the Mutter path, not a field mismatch.
    #[test]
    fn reply_type_matches_mutter_wire_signature() {
        assert_eq!(
            GetCurrentStateReply::SIGNATURE.to_string(),
            "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})"
        );
    }

    fn ident(name: &str) -> (String, String, String, String) {
        (
            name.to_owned(),
            "VEN".to_owned(),
            "PROD".to_owned(),
            "SER".to_owned(),
        )
    }

    fn current_mode(width: i32, height: i32, refresh: f64) -> PhysicalMode {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "is-current".to_owned(),
            OwnedValue::try_from(Value::Bool(true)).unwrap(),
        );
        (
            "mode".to_owned(),
            width,
            height,
            refresh,
            1.0,
            vec![1.0, 2.0],
            props,
        )
    }

    #[test]
    fn maps_single_monitor_with_scale_to_logical_geometry() {
        let physical = vec![(
            ident("eDP-1"),
            vec![current_mode(3840, 2160, 60.0)],
            HashMap::new(),
        )];
        let logical = vec![(0, 0, 2.0, 0u32, true, vec![ident("eDP-1")], HashMap::new())];

        let screens = map_logical_monitors(&physical, &logical);
        assert_eq!(screens.len(), 1);
        let s = &screens[0];
        assert_eq!(s.id, "eDP-1");
        assert_eq!(s.name, "eDP-1");
        // 3840x2160 physical / 2.0 scale = 1920x1080 logical.
        assert_eq!(s.geometry.width, 1920);
        assert_eq!(s.geometry.height, 1080);
        assert_eq!(s.geometry.x, 0);
        assert_eq!(s.geometry.y, 0);
        assert_eq!(s.scale, Some(2.0));
        assert_eq!(s.refresh_millihz, Some(60_000));
        assert!(s.is_primary);
        assert!(s.is_active);
    }

    #[test]
    fn maps_dual_monitors_with_offset_and_primary_flag() {
        let physical = vec![
            (
                ident("eDP-1"),
                vec![current_mode(1920, 1080, 60.0)],
                HashMap::new(),
            ),
            (
                ident("DP-2"),
                vec![current_mode(2560, 1440, 144.0)],
                HashMap::new(),
            ),
        ];
        let logical = vec![
            (0, 0, 1.0, 0u32, true, vec![ident("eDP-1")], HashMap::new()),
            (
                1920,
                0,
                1.0,
                0u32,
                false,
                vec![ident("DP-2")],
                HashMap::new(),
            ),
        ];

        let screens = map_logical_monitors(&physical, &logical);
        assert_eq!(screens.len(), 2);
        assert_eq!(screens[0].id, "eDP-1");
        assert!(screens[0].is_primary);
        assert_eq!(screens[1].id, "DP-2");
        assert!(!screens[1].is_primary);
        // Second monitor is offset to the right of the first.
        assert_eq!(screens[1].geometry.x, 1920);
        assert_eq!(screens[1].geometry.width, 2560);
        assert_eq!(screens[1].refresh_millihz, Some(144_000));
    }
}
