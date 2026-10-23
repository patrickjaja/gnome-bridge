//! Monitor enumeration via `org.gnome.Mutter.DisplayConfig.GetCurrentState`.
//!
//! This is a public session-bus API on GNOME (the same one gnome-control-center
//! uses). It returns logical monitors with their position, size, scale, primary
//! flag, and the connector/vendor/product of each physical monitor assigned to
//! them. We map that to the shared `Screen` JSON shape (snake_case, matching
//! kwin-portal-bridge / x11-bridge).
//!
//! Units: `geometry` is in Mutter's LAYOUT units, the same space the portal
//! reports ScreenCast stream positions/sizes in. In the default logical layout
//! mode (`layout-mode` = 1) that is logical pixels and the stream's video is
//! `layout * scale` physical pixels, so `Screen.scale` is the monitor scale. In
//! physical layout mode (`layout-mode` = 2) the layout already is physical
//! pixels, so geometry is the mode size and `Screen.scale` is 1.0.
//!
//! Fallback: if the Mutter API is unavailable (non-GNOME compositor exposing the
//! portal, or DisplayConfig missing), the caller derives screens from the portal
//! ScreenCast streams instead (see `portal.rs`).

use anyhow::{Context, Result};
use zbus::Connection;
use zbus::zvariant::OwnedValue;

use crate::output::{Rect, Screen, ScreenSource};

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

    Ok(screens_from_reply(&reply))
}

/// Map a whole `GetCurrentState` reply, honoring its `layout-mode` property.
pub(crate) fn screens_from_reply(reply: &GetCurrentStateReply) -> Vec<Screen> {
    let (_serial, physical, logical, props) = reply;
    map_logical_monitors(physical, logical, LayoutMode::from_props(props))
}

/// Mutter's `layout-mode` global property (GetCurrentState's 4th element).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// `1`: the layout is in logical pixels; a monitor's layout size is its
    /// mode size divided by its scale. Also the default when the property is
    /// missing (it is what every Mutter without the property does).
    Logical,
    /// `2`: the layout is in physical pixels; scale only affects how clients
    /// render, not the layout, and the pointer maps 1:1 onto stream pixels.
    Physical,
}

impl LayoutMode {
    fn from_props(props: &std::collections::HashMap<String, OwnedValue>) -> Self {
        match props
            .get("layout-mode")
            .and_then(|v| v.downcast_ref::<u32>().ok())
        {
            Some(2) => Self::Physical,
            _ => Self::Logical,
        }
    }
}

/// Pure mapping from the DisplayConfig reply structs to `Screen` entries,
/// factored out so it can be unit-tested against canned data.
pub fn map_logical_monitors(
    physical: &[PhysicalMonitor],
    logical: &[LogicalMonitor],
    layout_mode: LayoutMode,
) -> Vec<Screen> {
    let single_head = logical.len() == 1;

    logical
        .iter()
        .map(|(x, y, scale, transform, primary, monitors, _props)| {
            // A logical monitor's connector name is the first assigned physical
            // monitor's connector; use it as both `id` and `name`.
            let connector = monitors
                .first()
                .map(|(c, _, _, _)| c.clone())
                .unwrap_or_default();

            // Layout size: in logical mode the current physical mode size /
            // scale, in physical mode the mode size itself. `layout_scale` is
            // what one layout unit is worth in stream (video) pixels - the
            // factor the pointer path multiplies by (see `Screen.scale`).
            let (mode_w, mode_h, refresh) = connector_current_mode(physical, &connector);
            // The mode is in the panel's native orientation; a logical
            // monitor rotated by 90/270 degrees (odd Mutter transforms: 1, 3
            // and their flipped variants 5, 7) is laid out - and streamed -
            // with width and height swapped.
            let (phys_w, phys_h) = if transform % 2 == 1 {
                (mode_h, mode_w)
            } else {
                (mode_w, mode_h)
            };
            let layout_scale = match layout_mode {
                LayoutMode::Logical if scale.is_finite() && *scale > 0.0 => *scale,
                _ => 1.0,
            };
            let logical_w = (f64::from(phys_w) / layout_scale).round() as i32;
            let logical_h = (f64::from(phys_h) / layout_scale).round() as i32;

            let is_primary = *primary || single_head;

            Screen {
                geometry: Rect {
                    x: *x,
                    y: *y,
                    width: logical_w.max(1),
                    height: logical_h.max(1),
                },
                scale: Some(layout_scale),
                refresh_millihz: refresh.map(|r| (r * 1000.0).round() as u32),
                is_active: is_primary,
                is_primary,
                name: connector.clone(),
                id: connector,
                is_shared: None,
                source: ScreenSource::Mutter,
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
pub(crate) mod tests {
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

        let screens = map_logical_monitors(&physical, &logical, LayoutMode::Logical);
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

        let screens = map_logical_monitors(&physical, &logical, LayoutMode::Logical);
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

    #[test]
    fn layout_mode_prop_decodes() {
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        assert_eq!(LayoutMode::from_props(&props), LayoutMode::Logical);
        props.insert(
            "layout-mode".to_owned(),
            OwnedValue::try_from(Value::U32(1)).unwrap(),
        );
        assert_eq!(LayoutMode::from_props(&props), LayoutMode::Logical);
        props.insert(
            "layout-mode".to_owned(),
            OwnedValue::try_from(Value::U32(2)).unwrap(),
        );
        assert_eq!(LayoutMode::from_props(&props), LayoutMode::Physical);
    }

    /// The layout that exposed claude-desktop-extra #293: a 1.3333-scaled
    /// laptop panel below-left of a scale-1 ultrawide, logical layout mode.
    #[test]
    fn maps_fractional_logical_layout_of_issue_293() {
        let physical = vec![
            (
                ident("eDP-1"),
                vec![current_mode(2560, 1600, 60.0)],
                HashMap::new(),
            ),
            (
                ident("HDMI-1"),
                vec![current_mode(3440, 1440, 60.0)],
                HashMap::new(),
            ),
        ];
        let logical = vec![
            (
                0,
                240,
                4.0 / 3.0,
                0u32,
                true,
                vec![ident("eDP-1")],
                HashMap::new(),
            ),
            (
                1920,
                0,
                1.0,
                0u32,
                false,
                vec![ident("HDMI-1")],
                HashMap::new(),
            ),
        ];
        let screens = map_logical_monitors(&physical, &logical, LayoutMode::Logical);
        let edp = &screens[0];
        assert_eq!(
            (
                edp.geometry.x,
                edp.geometry.y,
                edp.geometry.width,
                edp.geometry.height
            ),
            (0, 240, 1920, 1200)
        );
        assert!((edp.scale.unwrap() - 4.0 / 3.0).abs() < 1e-9);
        assert_eq!(screens[1].geometry.width, 3440);
        assert_eq!(screens[1].scale, Some(1.0));
    }

    /// A whole `GetCurrentState` reply in physical layout mode
    /// (`layout-mode` = 2): geometry is the mode size, NOT divided by the
    /// scale, and the layout -> stream factor is 1.
    pub(crate) fn physical_mode_reply() -> GetCurrentStateReply {
        let physical = vec![(
            ident("eDP-1"),
            vec![current_mode(2560, 1600, 60.0)],
            HashMap::new(),
        )];
        let logical = vec![(
            0,
            0,
            4.0 / 3.0,
            0u32,
            true,
            vec![ident("eDP-1")],
            HashMap::new(),
        )];
        let mut props: HashMap<String, OwnedValue> = HashMap::new();
        props.insert(
            "layout-mode".to_owned(),
            OwnedValue::try_from(Value::U32(2)).unwrap(),
        );
        (1, physical, logical, props)
    }

    #[test]
    fn maps_physical_layout_mode_without_dividing() {
        let screens = screens_from_reply(&physical_mode_reply());
        let s = &screens[0];
        assert_eq!((s.geometry.width, s.geometry.height), (2560, 1600));
        assert_eq!(s.scale, Some(1.0));
    }

    /// A portrait (90-degree, transform 1) and a flipped-270 (transform 7)
    /// monitor are laid out with width and height swapped; 180 (2) is not.
    #[test]
    fn rotated_monitors_swap_width_and_height() {
        let physical = vec![
            (
                ident("DP-1"),
                vec![current_mode(2560, 1440, 60.0)],
                HashMap::new(),
            ),
            (
                ident("DP-2"),
                vec![current_mode(3840, 2160, 60.0)],
                HashMap::new(),
            ),
            (
                ident("DP-3"),
                vec![current_mode(1920, 1080, 60.0)],
                HashMap::new(),
            ),
        ];
        let logical = vec![
            (0, 0, 1.0, 1u32, true, vec![ident("DP-1")], HashMap::new()),
            (
                1440,
                0,
                2.0,
                7u32,
                false,
                vec![ident("DP-2")],
                HashMap::new(),
            ),
            (
                2520,
                0,
                1.0,
                2u32,
                false,
                vec![ident("DP-3")],
                HashMap::new(),
            ),
        ];
        let screens = map_logical_monitors(&physical, &logical, LayoutMode::Logical);
        let size = |s: &Screen| (s.geometry.width, s.geometry.height);
        assert_eq!(size(&screens[0]), (1440, 2560));
        assert_eq!(size(&screens[1]), (1080, 1920));
        assert_eq!(size(&screens[2]), (1920, 1080));
        // Physical layout mode: swapped, not divided.
        let screens = map_logical_monitors(&physical, &logical, LayoutMode::Physical);
        assert_eq!(size(&screens[1]), (2160, 3840));
    }
}
