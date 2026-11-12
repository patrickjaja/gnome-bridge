//! AT-SPI accessibility subcommands (`ax-enable`, `ax-restore`, `ax-find`,
//! `ax-action`) and the `atspi` key of `doctor`, backed by the shared
//! `cu-atspi` crate (claude-desktop-extra issue #306).
//!
//! None of this touches the XDG desktop portal or the session daemon: the
//! accessibility bus is a plain D-Bus peer of the session bus, so these
//! commands never open a ScreenCast/RemoteDesktop session, never show a
//! consent dialog, are not blocked by the stopped-by-user marker, and work the
//! same whether or not a session daemon runs. Running them in the daemon would
//! only add a socket hop and tie them to a portal session they do not need.
//!
//! Window identity on GNOME: `org.gnome.Shell.Introspect.GetWindows` returns,
//! per window, `app-id`, `client-type`, `is-hidden`, `has-focus`, `width`,
//! `height` and, when set, `title`, `wm-class`, `sandboxed-app-id` (identical
//! in GNOME Shell 45 through 51, `js/misc/introspect.js`). It exposes no pid
//! and no position, and `GetRunningApplications` only `active-on-seats` and
//! `sandboxed-app-id`. So the accessible app is matched by app id / WM_CLASS
//! and the window by title, and positions stay `window_only` except for X11
//! (XWayland) clients, whose AT-SPI SCREEN coordinates are global.
//!
//! Introspect answers only GNOME's own portal backends unless GNOME Shell
//! runs in unsafe mode (`DBusSenderChecker` in `js/misc/util.js`). When it
//! refuses, `--app-id` (and `--title`) carry the identity instead.

use anyhow::{Result, bail};
use cu_atspi::{RequestError, WindowHint};
use serde_json::{Value, json};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use crate::output::print_json;

/// `Meta.WindowClientType.X11` as reported in `client-type`.
const CLIENT_TYPE_X11: u32 = 1;

/// The arguments of `ax-find` (see `cli::Command::AxFind`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindArgs {
    pub window: Option<String>,
    pub app_id: Option<String>,
    pub title: Option<String>,
    pub flags: cu_atspi::cli::AxFindFlags,
}

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props
        .get(key)
        .and_then(|v| v.downcast_ref::<&str>().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn prop_u32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| v.downcast_ref::<u32>().ok())
}

/// The app id to match against AT-SPI application names: Shell's `app-id`
/// (the desktop file id, `.desktop` dropped), unless Shell made one up for a
/// window it could not tie to a desktop file (`window:<n>`); then the Flatpak
/// `sandboxed-app-id`.
fn app_id_from_props(props: &HashMap<String, OwnedValue>) -> Option<String> {
    prop_str(props, "app-id")
        .filter(|id| !id.starts_with("window:"))
        .map(|id| id.strip_suffix(".desktop").unwrap_or(&id).to_owned())
        .or_else(|| prop_str(props, "sandboxed-app-id"))
}

/// Turn one Introspect window entry into the crate's window hint. No pid and
/// no rect: Introspect has neither (see the module docs).
pub fn hint_from_props(id: u64, props: &HashMap<String, OwnedValue>) -> WindowHint {
    WindowHint {
        id: Some(id.to_string()),
        app_id: app_id_from_props(props),
        wm_class: prop_str(props, "wm-class"),
        title: prop_str(props, "title"),
        x11: prop_u32(props, "client-type") == Some(CLIENT_TYPE_X11),
        ..Default::default()
    }
}

/// Fill what Introspect did not give from the caller's `--app-id` / `--title`.
fn apply_overrides(mut hint: WindowHint, args: &FindArgs) -> WindowHint {
    if hint.app_id.is_none() {
        hint.app_id = args.app_id.clone();
    }
    if hint.title.is_none() {
        hint.title = args.title.clone();
    }
    hint
}

fn window_not_found(hint: &str) -> Value {
    json!({"unavailable": "window_not_found", "hint": hint})
}

/// Resolve the window hint. `windows` is the Introspect reply, or the error
/// text when Introspect refused or is missing. `Err(value)` is the
/// `unavailable` result to print.
pub fn resolve_hint(
    args: &FindArgs,
    windows: std::result::Result<&crate::windows::WindowMap, &str>,
) -> std::result::Result<WindowHint, Value> {
    let Some(window) = args.window.as_deref() else {
        // Identity from the caller alone (`--app-id` is required by clap).
        return Ok(WindowHint {
            app_id: args.app_id.clone(),
            title: args.title.clone(),
            ..Default::default()
        });
    };
    match windows {
        Ok(map) => {
            let found = window
                .trim()
                .parse::<u64>()
                .ok()
                .and_then(|id| map.get(&id).map(|props| hint_from_props(id, props)));
            match found {
                Some(hint) => Ok(apply_overrides(hint, args)),
                None => Err(window_not_found(&format!(
                    "No window with id {window}; call `windows` again for current ids."
                ))),
            }
        }
        Err(_) if args.app_id.is_some() => Ok(WindowHint {
            id: Some(window.to_owned()),
            app_id: args.app_id.clone(),
            title: args.title.clone(),
            ..Default::default()
        }),
        Err(error) => Err(window_not_found(&format!(
            "GNOME Shell did not list its windows ({error}); its Introspect API answers \
             only GNOME's own portal backends unless Shell runs in unsafe mode. Pass \
             --app-id (and --title) to match the app directly."
        ))),
    }
}

/// Bad input that is not a clap error (exit 2, contract v1 "bad usage").
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// `RequestError::Usage` exits 2, `RequestError::Io` exits 1.
fn from_request_error(error: RequestError) -> anyhow::Error {
    match error {
        RequestError::Usage(message) => UsageError(message).into(),
        RequestError::Io(message) => anyhow::anyhow!(message),
    }
}

fn print_outcome<T: serde::Serialize>(
    outcome: cu_atspi::Result<cu_atspi::Outcome<T>>,
) -> Result<()> {
    match outcome {
        Ok(value) => print_json(&value),
        // The crate's messages never carry element text or set-text input.
        Err(error) => bail!("accessibility: {}", error.0),
    }
}

pub async fn enable(holder_pid: u32) -> Result<()> {
    print_outcome(cu_atspi::enable(holder_pid).await)
}

pub async fn restore(holder_pid: u32) -> Result<()> {
    print_outcome(cu_atspi::restore(holder_pid).await)
}

pub async fn find(args: FindArgs) -> Result<()> {
    let hint = if args.window.is_some() {
        let windows = match zbus::Connection::session().await {
            Ok(connection) => crate::windows::get_windows_raw(&connection)
                .await
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("session bus unreachable: {e}")),
        };
        resolve_hint(&args, windows.as_ref().map_err(String::as_str))
    } else {
        resolve_hint(&args, Err("not asked"))
    };
    let hint = match hint {
        Ok(hint) => hint,
        Err(unavailable) => return print_json(&unavailable),
    };
    // `--window-ref`: when the crate accepts the ref (`window_ref_used: true`)
    // it takes the window from the ref and never compares the hint's title,
    // so a `--title` that no longer matches cannot fail the find; the app
    // match (`--app-id` / wm-class) is still made first.
    let request = args.flags.request(hint);
    print_outcome(cu_atspi::find(&request).await)
}

pub async fn action(flags: &cu_atspi::cli::AxActionFlags) -> Result<()> {
    // GNOME Wayland bridge: no X11 session.
    let request = flags
        .request(false, std::io::stdin().lock())
        .map_err(from_request_error)?;
    print_outcome(cu_atspi::act(&request).await)
}

/// Cap for the `atspi` doctor probe. The crate allows itself 5 s; `doctor`
/// as a whole must stay inside the launcher's 5 s budget, like the other
/// probes (1.5 s each, daemon ping 3 s) it runs next to.
const DOCTOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The `atspi` key of `doctor`: never fails, never activates the a11y bus.
pub async fn doctor() -> cu_atspi::AtspiDoctor {
    tokio::time::timeout(DOCTOR_TIMEOUT, cu_atspi::doctor())
        .await
        .unwrap_or_else(|_| cu_atspi::AtspiDoctor {
            error: Some(format!(
                "accessibility probe did not finish within {} ms",
                DOCTOR_TIMEOUT.as_millis()
            )),
            // `Default` carries this build's `features`.
            ..Default::default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value as ZValue;

    fn props(entries: &[(&str, ZValue<'_>)]) -> HashMap<String, OwnedValue> {
        entries
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_owned(),
                    OwnedValue::try_from(v.try_clone().unwrap()).unwrap(),
                )
            })
            .collect()
    }

    fn map(id: u64, p: HashMap<String, OwnedValue>) -> crate::windows::WindowMap {
        HashMap::from([(id, p)])
    }

    fn args(window: Option<&str>) -> FindArgs {
        FindArgs {
            window: window.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn wayland_window_maps_to_app_id_title_no_pid_no_rect() {
        let p = props(&[
            ("app-id", ZValue::from("org.gnome.TextEditor.desktop")),
            ("client-type", ZValue::from(0u32)),
            ("is-hidden", ZValue::from(false)),
            ("has-focus", ZValue::from(true)),
            ("width", ZValue::from(800u32)),
            ("height", ZValue::from(600u32)),
            ("title", ZValue::from("Untitled Document 1")),
            ("wm-class", ZValue::from("org.gnome.TextEditor")),
        ]);
        let h = hint_from_props(42, &p);
        assert_eq!(h.id.as_deref(), Some("42"));
        assert_eq!(h.app_id.as_deref(), Some("org.gnome.TextEditor"));
        assert_eq!(h.wm_class.as_deref(), Some("org.gnome.TextEditor"));
        assert_eq!(h.title.as_deref(), Some("Untitled Document 1"));
        assert_eq!(h.pid, None);
        assert!(!h.x11);
        assert!(h.rect_screen_logical.is_none());
        assert!(h.client_rect_screen_logical.is_none());
    }

    #[test]
    fn x11_client_sets_x11_and_synthetic_app_id_falls_back_to_sandboxed() {
        let p = props(&[
            ("app-id", ZValue::from("window:17")),
            ("client-type", ZValue::from(1u32)),
            ("sandboxed-app-id", ZValue::from("org.mozilla.firefox")),
            ("title", ZValue::from("")),
        ]);
        let h = hint_from_props(7, &p);
        assert!(h.x11);
        assert_eq!(h.app_id.as_deref(), Some("org.mozilla.firefox"));
        // An empty title is no title.
        assert_eq!(h.title, None);
        assert!(h.rect_screen_logical.is_none());

        let bare = props(&[("app-id", ZValue::from("window:3"))]);
        assert_eq!(hint_from_props(3, &bare).app_id, None);
    }

    #[test]
    fn resolve_known_unknown_and_refused() {
        let windows = map(
            42,
            props(&[
                ("app-id", ZValue::from("org.gnome.Nautilus.desktop")),
                ("title", ZValue::from("Home")),
            ]),
        );
        let h = resolve_hint(&args(Some("42")), Ok(&windows)).unwrap();
        assert_eq!(h.app_id.as_deref(), Some("org.gnome.Nautilus"));

        for id in ["43", "0x2a", ""] {
            let v = resolve_hint(&args(Some(id)), Ok(&windows)).unwrap_err();
            assert_eq!(v["unavailable"], "window_not_found", "{id}");
        }

        let v = resolve_hint(&args(Some("42")), Err("AccessDenied")).unwrap_err();
        assert_eq!(v["unavailable"], "window_not_found");
        assert!(v["hint"].as_str().unwrap().contains("--app-id"));

        let mut a = args(Some("42"));
        a.app_id = Some("org.gnome.Nautilus".into());
        a.title = Some("Home".into());
        let h = resolve_hint(&a, Err("AccessDenied")).unwrap();
        assert_eq!(h.id.as_deref(), Some("42"));
        assert_eq!(h.app_id.as_deref(), Some("org.gnome.Nautilus"));
        assert_eq!(h.title.as_deref(), Some("Home"));

        let mut a = args(None);
        a.app_id = Some("org.gnome.Calculator".into());
        let h = resolve_hint(&a, Err("not asked")).unwrap();
        assert_eq!(h.id, None);
        assert_eq!(h.app_id.as_deref(), Some("org.gnome.Calculator"));
    }

    #[test]
    fn overrides_fill_only_missing_fields() {
        let windows = map(5, props(&[("app-id", ZValue::from("window:5"))]));
        let mut a = args(Some("5"));
        a.app_id = Some("org.example.App".into());
        a.title = Some("Main".into());
        let h = resolve_hint(&a, Ok(&windows)).unwrap();
        assert_eq!(h.app_id.as_deref(), Some("org.example.App"));
        assert_eq!(h.title.as_deref(), Some("Main"));

        let windows = map(5, props(&[("title", ZValue::from("Real"))]));
        let h = resolve_hint(&a, Ok(&windows)).unwrap();
        assert_eq!(h.title.as_deref(), Some("Real"));
    }

    #[test]
    fn title_never_decides_the_window_here() {
        // The window-ref rule (contract: `--title` need not match when
        // `window_ref_used`) holds because this bridge only forwards the
        // title as a hint and the crate ignores the hint's title once the
        // ref qualifies. Even a title that matches nothing must resolve.
        let windows = map(
            9,
            props(&[
                ("app-id", ZValue::from("org.gnome.Nautilus.desktop")),
                ("title", ZValue::from("Home")),
            ]),
        );
        let mut a = args(Some("9"));
        a.title = Some("renamed since".into());
        a.flags.window_ref = Some(":1.4/org/a11y/atspi/accessible/1".into());
        let hint = resolve_hint(&a, Ok(&windows)).unwrap();
        assert_eq!(hint.app_id.as_deref(), Some("org.gnome.Nautilus"));
        let request = a.flags.request(hint);
        assert_eq!(request.window_ref, a.flags.window_ref);
    }
}
