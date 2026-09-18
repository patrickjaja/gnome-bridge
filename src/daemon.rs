//! Session daemon and one-shot proxy.
//!
//! `session-start` spawns a detached daemon that holds ONE `LivePortalSession`
//! for the whole tool-use lock, so the consent dialog (or restore-token grant)
//! happens once. Every other subcommand connects to the daemon's unix socket
//! and proxies its request through, so the held session's PipeWire stream and
//! input capability are reused with no re-prompt.
//!
//! If no daemon is running, one-shot commands fall back to an EPHEMERAL session:
//! open a portal session (dialog-free when a restore token exists), perform the
//! action, tear down. Both modes therefore work; the daemon just avoids
//! repeated setup cost within a lock.
//!
//! Socket path: `$XDG_RUNTIME_DIR/gnome-portal-bridge/session.sock`. Following
//! the kwin-portal-bridge precedent, the socket is NOT suffixed with
//! `CLAUDE_PROFILE`: the daemon is spawned per-CU-session by the app that owns
//! it, and there is exactly one active tool-use lock per running app instance,
//! so a fixed per-user path is correct. (Per the claude-desktop-extra profile
//! rule, profile isolation for app-owned resources comes from per-profile state
//! inherited via env, not from suffixing this socket.)

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use crate::input::Button;
use crate::output::Screen;
use crate::portal::LivePortalSession;
use crate::screens::{self};

/// One request over the session socket. Screen resolution happens client-side
/// where the Mutter connection lives, so requests carry a resolved `Screen`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionRequest {
    /// Liveness probe; the daemon replies with `{ok:true}`.
    Ping,
    /// Stop the daemon.
    Shutdown,
    MovePointer {
        screen: Screen,
        x: i32,
        y: i32,
    },
    Click {
        screen: Screen,
        x: i32,
        y: i32,
        button: String,
        count: u32,
        modifiers: Vec<String>,
    },
    Scroll {
        screen: Screen,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    },
    Drag {
        from_screen: Screen,
        from_x: i32,
        from_y: i32,
        to_screen: Screen,
        to_x: i32,
        to_y: i32,
    },
    LeftMouseDown,
    LeftMouseUp,
    KeySequence {
        keys: String,
        repeat: Option<u32>,
    },
    Type {
        text: String,
        delay_ms: u64,
    },
    HoldKey {
        keys: Vec<String>,
        duration_ms: u64,
    },
    Screenshot {
        screen: Screen,
    },
    Zoom {
        screen: Screen,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Whether a session daemon is currently reachable.
pub async fn daemon_running() -> bool {
    let Ok(socket) = socket_path() else {
        return false;
    };
    if !socket.exists() {
        return false;
    }
    ping(&socket).await.is_ok()
}

async fn ping(socket: &Path) -> Result<()> {
    let _: Value = send(socket, &SessionRequest::Ping).await?;
    Ok(())
}

/// Start a detached daemon holding a live portal session, waiting until it is
/// ready. Errors if a daemon is already active.
pub async fn start_session_daemon() -> Result<Value> {
    if daemon_running().await {
        bail!("a portal session is already active");
    }
    let socket = socket_path()?;
    cleanup_stale_socket(&socket).await?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = Command::new(current_exe);
    command
        .arg("serve-session")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().context("failed to spawn session daemon")?;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if ping(&socket).await.is_ok() {
            return Ok(serde_json::json!({ "ok": true, "session": "daemon" }));
        }
        if Instant::now() >= deadline {
            bail!(
                "session daemon did not become ready in time (consent dialog declined or timed out?)"
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Stop the daemon if one is running (idempotent).
pub async fn stop_session_daemon() -> Result<Value> {
    let socket = socket_path()?;
    if !socket.exists() {
        return Ok(serde_json::json!({ "ok": true, "ended": true }));
    }
    // Best-effort: a missing/dead daemon is still "ended".
    let _: Result<Value> = send(&socket, &SessionRequest::Shutdown).await;
    Ok(serde_json::json!({ "ok": true, "ended": true }))
}

/// Run the daemon: bind the socket, open the portal session, serve requests
/// until Shutdown or SIGTERM.
pub async fn serve_session(socket: PathBuf) -> Result<()> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create session socket directory `{}`",
                parent.display()
            )
        })?;
    }
    if socket.exists() {
        std::fs::remove_file(&socket).ok();
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind session socket `{}`", socket.display()))?;

    let mut session = match LivePortalSession::open().await {
        Ok(session) => session,
        Err(error) => {
            std::fs::remove_file(&socket).ok();
            return Err(error);
        }
    };

    let serve_result = async {
        #[cfg(unix)]
        let mut terminate =
            signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
        #[cfg(unix)]
        let terminate_fut = async {
            terminate.recv().await;
        };
        #[cfg(not(unix))]
        let terminate_fut = std::future::pending::<()>();
        tokio::pin!(terminate_fut);

        loop {
            tokio::select! {
                _ = &mut terminate_fut => break,
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.context("failed to accept IPC client")?;
                    let request = read_request(&mut stream).await?;
                    let (response, should_shutdown) = handle_request(&mut session, request).await;
                    write_response(&mut stream, response).await?;
                    if should_shutdown {
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    session.shutdown().await.ok();
    std::fs::remove_file(&socket).ok();
    serve_result
}

async fn handle_request(
    session: &mut LivePortalSession,
    request: SessionRequest,
) -> (SessionResponse, bool) {
    let result = match request {
        SessionRequest::Ping => respond_ok(serde_json::json!({ "ok": true })),
        SessionRequest::Shutdown => {
            return (respond_ok(serde_json::json!({ "ok": true })), true);
        }
        SessionRequest::MovePointer { screen, x, y } => {
            respond(session.move_pointer(&screen, x, y).await)
        }
        SessionRequest::Click {
            screen,
            x,
            y,
            button,
            count,
            modifiers,
        } => match Button::parse(&button) {
            Ok(button) => respond(
                session
                    .click(&screen, x, y, button, count, &modifiers)
                    .await,
            ),
            Err(error) => respond_err(&error),
        },
        SessionRequest::Scroll {
            screen,
            x,
            y,
            dx,
            dy,
        } => respond(session.scroll(&screen, x, y, dx, dy).await),
        SessionRequest::Drag {
            from_screen,
            from_x,
            from_y,
            to_screen,
            to_x,
            to_y,
        } => respond(
            session
                .drag(&from_screen, from_x, from_y, &to_screen, to_x, to_y)
                .await,
        ),
        SessionRequest::LeftMouseDown => respond(session.left_mouse_down().await),
        SessionRequest::LeftMouseUp => respond(session.left_mouse_up().await),
        SessionRequest::KeySequence { keys, repeat } => {
            respond(session.key_sequence(&keys, repeat).await)
        }
        SessionRequest::Type { text, delay_ms } => {
            respond(session.type_text(&text, delay_ms).await)
        }
        SessionRequest::HoldKey { keys, duration_ms } => {
            respond(session.hold_key(&keys, duration_ms).await)
        }
        SessionRequest::Screenshot { screen } => respond(session.screenshot(&screen).await),
        SessionRequest::Zoom { screen, x, y, w, h } => {
            respond(session.zoom(&screen, x, y, w, h).await)
        }
    };
    (result, false)
}

/// Run one action against the daemon if it is up, else against an ephemeral
/// session. Screen resolution is done by the caller (the Mutter connection lives
/// on the client side); the resolved `Screen` is embedded in the request.
pub async fn run_action(request: SessionRequest) -> Result<Value> {
    let socket = socket_path()?;
    if socket.exists()
        && let Ok(value) = send(&socket, &request).await
    {
        return Ok(value);
    }
    // Ephemeral fallback: spin up a one-shot session, run, tear down.
    run_ephemeral(request).await
}

async fn run_ephemeral(request: SessionRequest) -> Result<Value> {
    let mut session = LivePortalSession::open()
        .await
        .context("failed to open an ephemeral portal session")?;
    let (response, _) = handle_request(&mut session, request).await;
    session.shutdown().await.ok();
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        bail!(
            "{}",
            response.error.unwrap_or_else(|| "action failed".to_owned())
        )
    }
}

/// Resolve a `Screen` by optional display selector, preferring the live Mutter
/// DisplayConfig and falling back to the portal streams via an ephemeral probe.
/// Used by capture/pointer commands to build their request.
pub async fn resolve_screen(display: Option<&str>) -> Result<Screen> {
    let screens = list_screens_best_effort().await?;
    pick_screen(&screens, display)
}

/// List screens the same way the top-level `screens` command does: Mutter first,
/// portal-stream fallback.
pub async fn list_screens_best_effort() -> Result<Vec<Screen>> {
    if let Ok(connection) = zbus::Connection::session().await
        && let Ok(screens) = screens::list_screens(&connection).await
        && !screens.is_empty()
    {
        return Ok(screens);
    }
    // Fallback: derive from the portal streams. If a daemon is up we cannot
    // cheaply query it for streams, so open a short ephemeral session.
    let session = LivePortalSession::open()
        .await
        .context("failed to open portal session for screen enumeration")?;
    let screens = session.screens_from_streams();
    session.shutdown().await.ok();
    if screens.is_empty() {
        bail!("no screens reported by Mutter or the portal");
    }
    Ok(screens)
}

/// Select a screen by id/name, else the active/primary/first.
pub fn pick_screen(screens: &[Screen], selector: Option<&str>) -> Result<Screen> {
    if screens.is_empty() {
        bail!("no screens available");
    }
    if let Some(selector) = selector {
        return screens
            .iter()
            .find(|s| s.id == selector || s.name == selector)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("display `{selector}` not found"));
    }
    Ok(screens
        .iter()
        .find(|s| s.is_active)
        .or_else(|| screens.iter().find(|s| s.is_primary))
        .or_else(|| screens.first())
        .cloned()
        .expect("screens is non-empty"))
}

// ---- socket IO ----

async fn send<T: DeserializeOwned>(socket: &Path, request: &SessionRequest) -> Result<T> {
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("failed to connect to session socket `{}`", socket.display()))?;
    let payload = serde_json::to_vec(request)?;
    stream
        .write_all(&payload)
        .await
        .context("failed to write session IPC request")?;
    stream
        .shutdown()
        .await
        .context("failed to finish writing session IPC request")?;

    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .context("failed to read session IPC response")?;
    let response: SessionResponse =
        serde_json::from_slice(&bytes).context("failed to decode session IPC response")?;

    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "session IPC request failed".to_owned())
        );
    }
    let value = response.result.unwrap_or(Value::Null);
    serde_json::from_value(value).context("failed to decode session IPC payload")
}

async fn read_request(stream: &mut UnixStream) -> Result<SessionRequest> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .context("failed to read session IPC request")?;
    serde_json::from_slice(&bytes).context("failed to decode session IPC request")
}

async fn write_response(stream: &mut UnixStream, response: SessionResponse) -> Result<()> {
    let bytes = serde_json::to_vec(&response)?;
    stream
        .write_all(&bytes)
        .await
        .context("failed to write session IPC response")
}

async fn cleanup_stale_socket(socket: &Path) -> Result<()> {
    if !socket.exists() {
        return Ok(());
    }
    if UnixStream::connect(socket).await.is_err() {
        std::fs::remove_file(socket).with_context(|| {
            format!(
                "failed to remove stale session socket `{}`",
                socket.display()
            )
        })?;
    }
    Ok(())
}

pub fn socket_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    Ok(base.join("gnome-portal-bridge").join("session.sock"))
}

fn respond_ok(value: Value) -> SessionResponse {
    SessionResponse {
        ok: true,
        result: Some(value),
        error: None,
    }
}

fn respond_err(error: &anyhow::Error) -> SessionResponse {
    SessionResponse {
        ok: false,
        result: None,
        error: Some(format!("{error:#}")),
    }
}

fn respond<T: Serialize>(result: Result<T>) -> SessionResponse {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(v) => respond_ok(v),
            Err(error) => SessionResponse {
                ok: false,
                result: None,
                error: Some(format!("failed to serialize response: {error}")),
            },
        },
        Err(error) => respond_err(&error),
    }
}
