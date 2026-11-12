//! Environment / capability diagnostics.
//!
//! The leading keys are the Computer Use bridge CLI contract v1 (`contract`
//! .. `session_socket`, read by claude-desktop-extra's launcher and
//! executors), then the contract's optional `stopped_by_user`; the rest are
//! bridge-specific extras for humans: the desktop environment, the
//! RemoteDesktop / ScreenCast portal versions, the GNOME Shell version,
//! whether the Shell Introspect API is callable (drives whether `windows`
//! returns data), and the daemon log path.
//!
//! `doctor` never opens a portal session, never creates a file or directory,
//! and exits 0 even when the session bus is unreachable: every D-Bus call is a
//! property read or a ping, bounded by `PROBE_TIMEOUT`, and the probes run
//! concurrently.

use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use zbus::Connection;

use crate::daemon;
use crate::token_store::TokenStore;

/// Bound for each D-Bus probe (and for connecting to the bus). The probes run
/// concurrently, next to the daemon ping (3 s), so `doctor` stays inside the
/// launcher's 5 s budget even on a wedged bus.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

const MUTTER_DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    // ---- contract v1 ----
    pub contract: u32,
    pub bridge: &'static str,
    pub version: &'static str,
    pub backend: &'static str,
    /// A GNOME Wayland session, by the same environment check the executor
    /// uses to pick this bridge.
    pub session_match: bool,
    /// The session bus, the XDG desktop portal (RemoteDesktop or ScreenCast)
    /// and Mutter's DisplayConfig all answered.
    pub backend_reachable: bool,
    pub backend_error: Option<String>,
    pub session_supported: bool,
    pub restore_token_present: bool,
    pub restore_token_path: Option<String>,
    pub daemon_running: bool,
    pub session_socket: Option<String>,
    // ---- contract v1, optional ----
    /// The stopped-by-user marker exists: the user stopped screen sharing and
    /// input/capture exit 5 until the next `session-start`.
    pub stopped_by_user: bool,
    // ---- bridge-specific ----
    pub xdg_current_desktop: String,
    pub xdg_session_type: String,
    pub portal_present: bool,
    pub remote_desktop_version: Option<u32>,
    pub screen_cast_version: Option<u32>,
    pub mutter_present: bool,
    pub gnome_shell_version: Option<String>,
    pub introspect_available: bool,
    /// The directory of `restore_token_path`. Deprecated: kept for one
    /// release for readers from before contract v1.
    pub restore_token_dir: String,
    /// Where a detached daemon's stderr lands.
    pub daemon_log: String,
    /// Accessibility bus state for the `ax-*` commands (optional contract
    /// key, issue #306). Never fails `doctor`, never activates the a11y bus.
    pub atspi: cu_atspi::AtspiDoctor,
    /// What the `overlay` subcommand can do here (additive; the same keys as
    /// wlroots-bridge and kwin-portal-bridge).
    pub capabilities: Capabilities,
}

/// `capabilities`: `overlay` = the overlay can run here (a GNOME Wayland
/// session with an XWayland display to connect to); `overlay_alpha_modifier`
/// = fades, capture hides and opacity work (`_NET_WM_WINDOW_OPACITY`, always
/// with the x11 backend); `overlay_fractional_scale` = drawn at a monitor's
/// exact fractional scale (never on X). The same values cu-overlay's x11
/// backend reports in `ready`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub overlay: bool,
    pub overlay_alpha_modifier: bool,
    pub overlay_fractional_scale: bool,
}

impl Capabilities {
    pub fn new(session_match: bool, xwayland: bool) -> Self {
        let overlay = session_match && xwayland;
        Self {
            overlay,
            overlay_alpha_modifier: overlay,
            overlay_fractional_scale: false,
        }
    }
}

/// The X display number of a local `$DISPLAY` (`:0`, `:1.0`, `unix:0`);
/// `None` for an empty, malformed or remote (TCP) display.
pub fn local_display_number(display: &str) -> Option<u32> {
    let rest = display
        .strip_prefix("unix:")
        .or_else(|| display.strip_prefix(':'))?;
    let number = rest.split('.').next()?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Whether `$DISPLAY` names an X server the overlay can reach, WITHOUT
/// connecting: Mutter starts XWayland on demand when a client connects
/// (180-270 ms cold), so `doctor` only checks that `$DISPLAY` is local and
/// its socket exists. Mutter creates `/tmp/.X11-unix/X<n>` itself and keeps
/// it while it can start XWayland; a Mutter without XWayland sets no
/// `$DISPLAY` and creates no socket. A plain stat, no I/O on the socket.
fn xwayland_reachable(display: Option<&str>) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let Some(number) = display.and_then(local_display_number) else {
        return false;
    };
    std::fs::metadata(format!("/tmp/.X11-unix/X{number}"))
        .is_ok_and(|meta| meta.file_type().is_socket())
}

/// Run a probe bounded by `PROBE_TIMEOUT`; a timeout is `None`.
async fn bounded<T>(probe: impl Future<Output = Option<T>>) -> Option<T> {
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .ok()
        .flatten()
}

/// Read a `u` property from an interface on the desktop portal.
async fn portal_version(connection: &Connection, interface: &str) -> Option<u32> {
    let reply = connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, "version"),
        )
        .await
        .ok()?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    value.downcast_ref::<u32>().ok()
}

/// Whether Mutter's DisplayConfig answers a `Peer.Ping`: proves the service
/// `screens` reads is there without querying the monitor state.
async fn mutter_answers(connection: &Connection) -> Result<(), String> {
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        connection.call_method(
            Some(MUTTER_DISPLAY_CONFIG),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        ),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("Mutter DisplayConfig did not answer: {error}")),
        Err(_) => Err(format!(
            "Mutter DisplayConfig did not answer within {} ms",
            PROBE_TIMEOUT.as_millis()
        )),
    }
}

async fn gnome_shell_version(connection: &Connection) -> Option<String> {
    let reply = connection
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.gnome.Shell", "ShellVersion"),
        )
        .await
        .ok()?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    value.downcast_ref::<&str>().ok().map(str::to_owned)
}

/// Whether `org.gnome.Shell.Introspect.GetWindows` is callable (i.e. we are
/// allowlisted or unsafe-mode is on). A returned error means restricted.
async fn introspect_available(connection: &Connection) -> Option<bool> {
    Some(
        connection
            .call_method(
                Some("org.gnome.Shell"),
                "/org/gnome/Shell/Introspect",
                Some("org.gnome.Shell.Introspect"),
                "GetWindows",
                &(),
            )
            .await
            .is_ok(),
    )
}

/// A GNOME Wayland session: the executor's own test (`_isGnomeWayland` in
/// claude-desktop-extra's `js/cu_linux_executor.js`), so `session_match` is
/// true exactly where this bridge gets picked.
pub fn is_gnome_wayland(
    session_type: Option<&str>,
    wayland_display: Option<&str>,
    current_desktop: Option<&str>,
) -> bool {
    let wayland = session_type.is_some_and(|t| t.eq_ignore_ascii_case("wayland"))
        || wayland_display.is_some_and(|d| !d.is_empty());
    let gnome = current_desktop.is_some_and(|d| d.to_ascii_lowercase().contains("gnome"));
    wayland && gnome
}

/// The contract's reachability verdict from what the probes found.
fn backend_verdict(
    bus: Result<(), String>,
    portal_present: bool,
    mutter: Result<(), String>,
) -> (bool, Option<String>) {
    if let Err(error) = bus {
        return (false, Some(error));
    }
    let mut errors = Vec::new();
    if !portal_present {
        errors.push("the XDG desktop portal (RemoteDesktop/ScreenCast) did not answer".to_owned());
    }
    if let Err(error) = mutter {
        errors.push(error);
    }
    if errors.is_empty() {
        (true, None)
    } else {
        (false, Some(errors.join("; ")))
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

pub async fn doctor() -> DoctorReport {
    let current_desktop = env("XDG_CURRENT_DESKTOP");
    let session_type = env("XDG_SESSION_TYPE");
    let session_match = is_gnome_wayland(
        session_type.as_deref(),
        env("WAYLAND_DISPLAY").as_deref(),
        current_desktop.as_deref(),
    );

    let bus_probes = async {
        let connection = match tokio::time::timeout(PROBE_TIMEOUT, Connection::session()).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(error)) => return Err(format!("session bus unreachable: {error}")),
            Err(_) => {
                return Err(format!(
                    "session bus did not accept a connection within {} ms",
                    PROBE_TIMEOUT.as_millis()
                ));
            }
        };
        let conn = &connection;
        Ok(tokio::join!(
            bounded(portal_version(conn, "org.freedesktop.portal.RemoteDesktop")),
            bounded(portal_version(conn, "org.freedesktop.portal.ScreenCast")),
            mutter_answers(conn),
            bounded(gnome_shell_version(conn)),
            bounded(introspect_available(conn)),
        ))
    };
    let (bus, daemon_running, atspi) =
        tokio::join!(bus_probes, daemon::daemon_running(), crate::ax::doctor());

    let (bus, rd, sc, mutter, shell, introspect) = match bus {
        Ok((rd, sc, mutter, shell, introspect)) => {
            (Ok(()), rd, sc, mutter, shell, introspect.unwrap_or(false))
        }
        Err(error) => (
            Err(error),
            None,
            None,
            Err("no session bus".to_owned()),
            None,
            false,
        ),
    };
    let portal_present = rd.is_some() || sc.is_some();
    let mutter_present = mutter.is_ok();
    let (backend_reachable, backend_error) = backend_verdict(bus, portal_present, mutter);

    // Locate only: `TokenStore::new` would create the state directory.
    let token = TokenStore::locate().ok();
    let restore_token_present = token.as_ref().is_some_and(TokenStore::exists);
    let restore_token_path = token
        .as_ref()
        .map(|store| store.path().display().to_string());
    let restore_token_dir = token
        .as_ref()
        .and_then(|store| store.path().parent())
        .map_or_else(|| "<unknown>".to_owned(), |dir| dir.display().to_string());

    DoctorReport {
        contract: 1,
        bridge: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        backend: "gnome-portal",
        session_match,
        backend_reachable,
        backend_error,
        session_supported: true,
        restore_token_present,
        restore_token_path,
        daemon_running,
        session_socket: daemon::socket_path()
            .ok()
            .map(|path| path.display().to_string()),
        stopped_by_user: crate::stopped::is_set(),
        xdg_current_desktop: current_desktop.unwrap_or_else(|| "<unset>".to_owned()),
        xdg_session_type: session_type.unwrap_or_else(|| "<unset>".to_owned()),
        portal_present,
        remote_desktop_version: rd,
        screen_cast_version: sc,
        mutter_present,
        gnome_shell_version: shell,
        introspect_available: introspect,
        restore_token_dir,
        daemon_log: daemon::log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_owned()),
        atspi,
        capabilities: Capabilities::new(
            session_match,
            xwayland_reachable(env("DISPLAY").as_deref()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnome_wayland_matches_like_the_executor() {
        assert!(is_gnome_wayland(Some("wayland"), None, Some("GNOME")));
        assert!(is_gnome_wayland(
            Some("Wayland"),
            None,
            Some("ubuntu:GNOME")
        ));
        assert!(is_gnome_wayland(None, Some("wayland-0"), Some("pop:GNOME")));
        assert!(!is_gnome_wayland(Some("x11"), None, Some("GNOME")));
        assert!(!is_gnome_wayland(Some("wayland"), None, Some("KDE")));
        assert!(!is_gnome_wayland(Some("wayland"), Some(""), None));
        assert!(!is_gnome_wayland(None, None, None));
    }

    #[test]
    fn capabilities_json() {
        let json = |c: Capabilities| serde_json::to_value(c).unwrap();
        let all_false = serde_json::json!({
            "overlay": false, "overlay_alpha_modifier": false, "overlay_fractional_scale": false
        });
        assert_eq!(json(Capabilities::default()), all_false);
        assert_eq!(
            json(Capabilities::new(false, true)),
            all_false,
            "not GNOME Wayland"
        );
        assert_eq!(
            json(Capabilities::new(true, false)),
            all_false,
            "no XWayland"
        );
        assert_eq!(
            json(Capabilities::new(true, true)),
            serde_json::json!({
                "overlay": true, "overlay_alpha_modifier": true, "overlay_fractional_scale": false
            })
        );
    }

    #[test]
    fn display_numbers() {
        assert_eq!(local_display_number(":0"), Some(0));
        assert_eq!(local_display_number(":1.0"), Some(1));
        assert_eq!(local_display_number("unix:12"), Some(12));
        assert_eq!(local_display_number(""), None);
        assert_eq!(local_display_number(":"), None);
        assert_eq!(local_display_number("host:0"), None);
        assert_eq!(local_display_number("localhost:10.0"), None);
        assert_eq!(local_display_number(":x"), None);
        assert!(!xwayland_reachable(None));
        assert!(!xwayland_reachable(Some("")));
        assert!(!xwayland_reachable(Some(":4242")));
    }

    #[test]
    fn backend_verdict_names_every_missing_piece() {
        assert_eq!(backend_verdict(Ok(()), true, Ok(())), (true, None));
        let (ok, error) = backend_verdict(Err("session bus unreachable: x".into()), false, Ok(()));
        assert!(!ok);
        assert_eq!(error.as_deref(), Some("session bus unreachable: x"));
        let (ok, error) = backend_verdict(
            Ok(()),
            false,
            Err("Mutter DisplayConfig did not answer: y".into()),
        );
        assert!(!ok);
        let error = error.unwrap();
        assert!(error.contains("portal"), "{error}");
        assert!(error.contains("Mutter"), "{error}");
    }
}
