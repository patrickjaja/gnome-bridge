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
    /// The screens the daemon's portal session covers (derived from its
    /// ScreenCast streams). Lets the Mutter-less fallback reuse the live
    /// session instead of opening a second portal session (= a second
    /// consent dialog) while the daemon is up.
    Screens,
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

/// How long a liveness probe may wait for the daemon's reply. The daemon
/// binds its socket BEFORE the portal session is up, so while the consent
/// dialog is open a connect succeeds but no reply ever comes; without a bound
/// every `doctor` / `session-start` / `session-end` would hang on it.
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Grace period after a Shutdown reply before the daemon is signalled.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// Grace period after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// What a daemon probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// No daemon: no socket, or a socket nobody answers on and no live pid.
    Down,
    /// A daemon process exists but does not answer yet (consent dialog open,
    /// or busy with a long request).
    Starting(i32),
    /// The daemon answers.
    Up,
}

/// Probe the daemon: socket reachable and answering, else a live pid from the
/// pidfile, else down.
pub async fn daemon_state() -> DaemonState {
    let Ok(socket) = socket_path() else {
        return DaemonState::Down;
    };
    if socket.exists()
        && tokio::time::timeout(PING_TIMEOUT, ping(&socket))
            .await
            .is_ok_and(|r| r.is_ok())
    {
        return DaemonState::Up;
    }
    match live_daemon_pid() {
        Some(pid) => DaemonState::Starting(pid),
        None => DaemonState::Down,
    }
}

/// Whether a session daemon is currently reachable (answers a ping).
pub async fn daemon_running() -> bool {
    daemon_state().await == DaemonState::Up
}

async fn ping(socket: &Path) -> Result<()> {
    let _: Value = send(socket, &SessionRequest::Ping).await?;
    Ok(())
}

/// The pid recorded by a running `serve-session`, if that process is still
/// alive AND still a `gnome-portal-bridge serve-session` (pids get reused).
fn live_daemon_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path().ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if pid <= 1 {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if !cmdline.windows(13).any(|w| w == b"serve-session") {
        return None;
    }
    Some(pid)
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) only probes for existence/permission.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait up to `grace` for `pid` to exit; true when it did.
async fn wait_for_exit(pid: i32, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !pid_alive(pid)
}

/// Make sure a daemon process is gone: Shutdown reply or not, after the grace
/// period it gets SIGTERM, then SIGKILL. Returns the signal that was needed.
/// A `session-end` that leaves `serve-session` behind also leaves the GNOME
/// screen-sharing indicator on (the portal session stays open), so this is
/// not optional cleanup (claude-desktop-extra issue #293).
async fn reap_daemon(pid: i32) -> &'static str {
    if wait_for_exit(pid, SHUTDOWN_GRACE).await {
        return "exited";
    }
    // SAFETY: plain signal delivery to a pid we verified is our daemon.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    if wait_for_exit(pid, TERM_GRACE).await {
        return "sigterm";
    }
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_for_exit(pid, Duration::from_secs(1)).await;
    "sigkill"
}

/// Start a detached daemon holding a live portal session, waiting until it is
/// ready. Errors if a daemon is already active.
pub async fn start_session_daemon() -> Result<Value> {
    let socket = socket_path()?;
    match daemon_state().await {
        DaemonState::Up => bail!("a portal session is already active"),
        DaemonState::Starting(pid) => {
            // A daemon is already coming up (its consent dialog is probably
            // open). Spawning another would stack a second dialog; wait for
            // this one instead.
            return wait_until_ready(&socket, || pid_alive(pid)).await;
        }
        DaemonState::Down => {}
    }
    cleanup_stale_socket(&socket).await?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    // The daemon's stderr goes to a log next to the socket (truncated per
    // start) so a portal or PipeWire failure inside the session can be read
    // after the fact; nothing else ever sees that stream.
    let log = std::fs::File::create(log_path()?)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut command = Command::new(current_exe);
    command
        .arg("serve-session")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().context("failed to spawn session daemon")?;
    // `try_wait` rather than kill(pid, 0): an exited-but-unreaped child is a
    // zombie, which still "exists" to kill(2).
    wait_until_ready(&socket, || {
        child.try_wait().map(|s| s.is_none()).unwrap_or(true)
    })
    .await
}

/// Poll the socket until the daemon answers. Gives up early when the daemon
/// process dies (consent declined -> `serve_session` exits), else after 30 s.
async fn wait_until_ready(socket: &Path, mut alive: impl FnMut() -> bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::timeout(PING_TIMEOUT, ping(socket))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Ok(serde_json::json!({ "ok": true, "session": "daemon" }));
        }
        if !alive() {
            bail!("session daemon exited before becoming ready (consent dialog declined?)");
        }
        if Instant::now() >= deadline {
            bail!(
                "session daemon did not become ready in time (consent dialog declined or timed out?)"
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Stop the daemon if one is running (idempotent). Asks it to shut down over
/// the socket, then makes sure the process is actually gone.
pub async fn stop_session_daemon() -> Result<Value> {
    let socket = socket_path()?;
    let pid = live_daemon_pid();
    if socket.exists() {
        // Best-effort: a missing/dead daemon is still "ended". Bounded, so a
        // daemon stuck behind its consent dialog cannot hang us.
        let _ = tokio::time::timeout(PING_TIMEOUT, async {
            let _: Result<Value> = send(&socket, &SessionRequest::Shutdown).await;
        })
        .await;
    }
    let mut ended = serde_json::json!({ "ok": true, "ended": true });
    if let Some(pid) = pid {
        ended["daemon"] = Value::String(reap_daemon(pid).await.to_owned());
        std::fs::remove_file(&socket).ok();
        if let Ok(pidfile) = pid_path() {
            std::fs::remove_file(pidfile).ok();
        }
    }
    Ok(ended)
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
    let pidfile = pid_path()?;
    std::fs::write(&pidfile, std::process::id().to_string())
        .with_context(|| format!("failed to write pidfile `{}`", pidfile.display()))?;

    let mut session = match LivePortalSession::open().await {
        Ok(session) => session,
        Err(error) => {
            std::fs::remove_file(&socket).ok();
            std::fs::remove_file(&pidfile).ok();
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
                    // One client's failure must not end the session: the JS
                    // executor kills a bridge subprocess on timeout, and the
                    // half-written request that leaves behind used to take the
                    // whole daemon down with it (claude-desktop-extra #293).
                    let (mut stream, _) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            eprintln!("gnome-portal-bridge: accept failed: {error:#}");
                            continue;
                        }
                    };
                    let request = match read_request(&mut stream).await {
                        Ok(request) => request,
                        Err(error) => {
                            eprintln!("gnome-portal-bridge: dropped a client request: {error:#}");
                            continue;
                        }
                    };
                    let (response, should_shutdown) = handle_request(&mut session, request).await;
                    if let Err(error) = write_response(&mut stream, response).await {
                        eprintln!("gnome-portal-bridge: client went away before the reply: {error:#}");
                    }
                    if should_shutdown {
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Bounded: a portal/PipeWire teardown that never completes must not keep
    // the daemon (and the compositor's screen-sharing indicator) alive.
    let _ = tokio::time::timeout(Duration::from_secs(5), session.shutdown()).await;
    std::fs::remove_file(&socket).ok();
    std::fs::remove_file(&pidfile).ok();
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
        SessionRequest::Screens => respond(Ok(session.screens_from_streams())),
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
    match zbus::Connection::session().await {
        Ok(connection) => match screens::list_screens(&connection).await {
            Ok(screens) if !screens.is_empty() => return Ok(screens),
            Ok(_) => eprintln!("gnome-portal-bridge: Mutter DisplayConfig reported no monitors"),
            Err(error) => {
                eprintln!("gnome-portal-bridge: Mutter DisplayConfig unavailable: {error:#}")
            }
        },
        Err(error) => eprintln!("gnome-portal-bridge: no session bus: {error:#}"),
    }
    // Fallback 1: the running daemon already holds a portal session - ask it
    // for its streams instead of opening (and prompting for) a second one.
    let socket = socket_path()?;
    if socket.exists()
        && let Ok(Ok(screens)) = tokio::time::timeout(
            PING_TIMEOUT,
            send::<Vec<Screen>>(&socket, &SessionRequest::Screens),
        )
        .await
        && !screens.is_empty()
    {
        eprintln!("gnome-portal-bridge: screens derived from the running portal session's streams");
        return Ok(screens);
    }
    // Fallback 2: no daemon - open a short ephemeral session (consent dialog
    // unless a restore token is accepted).
    eprintln!("gnome-portal-bridge: screens derived from an ephemeral portal session");
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
    Ok(runtime_dir().join("session.sock"))
}

/// The daemon's pidfile, next to the socket.
pub fn pid_path() -> Result<PathBuf> {
    Ok(runtime_dir().join("session.pid"))
}

/// The detached daemon's stderr log, next to the socket.
pub fn log_path() -> Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create `{}`", dir.display()))?;
    Ok(dir.join("daemon.log"))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("gnome-portal-bridge")
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
