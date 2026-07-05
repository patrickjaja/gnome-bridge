//! Environment / capability diagnostics (human-facing; the JS never parses it).
//!
//! Reports the desktop environment, XDG portal presence and the RemoteDesktop /
//! ScreenCast interface versions, the GNOME Shell version, whether the Shell
//! Introspect API is callable (drives whether `windows` returns data), whether
//! a persisted restore token exists, and whether a session daemon is running.

use serde::Serialize;
use zbus::Connection;

use crate::daemon;
use crate::token_store::TokenStore;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub xdg_current_desktop: String,
    pub xdg_session_type: String,
    pub portal_present: bool,
    pub remote_desktop_version: Option<u32>,
    pub screen_cast_version: Option<u32>,
    pub gnome_shell_version: Option<String>,
    pub introspect_available: bool,
    pub restore_token_present: bool,
    pub restore_token_dir: String,
    pub daemon_running: bool,
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
async fn introspect_available(connection: &Connection) -> bool {
    connection
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell/Introspect",
            Some("org.gnome.Shell.Introspect"),
            "GetWindows",
            &(),
        )
        .await
        .is_ok()
}

pub async fn doctor() -> DoctorReport {
    let xdg_current_desktop =
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "<unset>".to_owned());
    let xdg_session_type =
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "<unset>".to_owned());

    let connection = Connection::session().await.ok();

    let (
        portal_present,
        remote_desktop_version,
        screen_cast_version,
        gnome_shell_version,
        introspect,
    ) = if let Some(conn) = connection.as_ref() {
        let rd = portal_version(conn, "org.freedesktop.portal.RemoteDesktop").await;
        let sc = portal_version(conn, "org.freedesktop.portal.ScreenCast").await;
        let shell = gnome_shell_version(conn).await;
        let introspect = introspect_available(conn).await;
        (rd.is_some() || sc.is_some(), rd, sc, shell, introspect)
    } else {
        (false, None, None, None, false)
    };

    let restore_token_present = TokenStore::new().map(|s| s.exists()).unwrap_or(false);
    let restore_token_dir = TokenStore::dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_owned());

    DoctorReport {
        xdg_current_desktop,
        xdg_session_type,
        portal_present,
        remote_desktop_version,
        screen_cast_version,
        gnome_shell_version,
        introspect_available: introspect,
        restore_token_present,
        restore_token_dir,
        daemon_running: daemon::daemon_running().await,
    }
}
