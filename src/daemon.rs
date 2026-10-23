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
//! repeated setup cost within a lock. Only an ABSENT daemon falls back; a
//! daemon that is there but fails returns its error (see `run_action`).
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

use crate::error_kind::{ErrorKind, kind_of, kinded};
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

impl SessionRequest {
    /// Requests that act on the desktop. These are refused instead of run
    /// when their client is no longer waiting (deadline passed, or hung up):
    /// the daemon serves one request at a time, so a click queued behind a
    /// slow request could otherwise land long after the caller gave up.
    /// `LeftMouseUp` is exempt - releasing a held button is always safe.
    fn is_input(&self) -> bool {
        matches!(
            self,
            Self::MovePointer { .. }
                | Self::Click { .. }
                | Self::Scroll { .. }
                | Self::Drag { .. }
                | Self::LeftMouseDown
                | Self::KeySequence { .. }
                | Self::Type { .. }
                | Self::HoldKey { .. }
        )
    }
}

/// A request plus the instant its client stops waiting, as CLOCK_MONOTONIC
/// nanoseconds (client and daemon share the machine, so the clock is shared).
#[derive(Debug, Serialize, Deserialize)]
struct RequestEnvelope {
    request: SessionRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline_mono_ns: Option<u64>,
}

/// What the daemon accepts on the wire: the envelope, or a bare request as
/// older clients send it (no deadline = no deadline check). Clients send the
/// bare form for everything but input, so those requests still work against
/// an older daemon too.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WireRequest {
    Envelope(RequestEnvelope),
    Bare(SessionRequest),
}

impl WireRequest {
    fn into_parts(self) -> (SessionRequest, Option<u64>) {
        match self {
            Self::Envelope(envelope) => (envelope.request, envelope.deadline_mono_ns),
            Self::Bare(request) => (request, None),
        }
    }
}

/// CLOCK_MONOTONIC now, in nanoseconds.
fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec we pass.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// Why an input request must not run, if it must not.
fn input_refusal(
    deadline_mono_ns: Option<u64>,
    now_mono_ns: u64,
    peer_closed: bool,
) -> Option<&'static str> {
    if deadline_mono_ns.is_some_and(|deadline| now_mono_ns >= deadline) {
        Some(
            "the client stopped waiting before the daemon got to this action (deadline passed); it was not executed",
        )
    } else if peer_closed {
        Some("the client hung up before the daemon got to this action; it was not executed")
    } else {
        None
    }
}

/// Whether the peer closed its end completely. A client half-closes (shuts
/// down writing) after its request and still waits for the reply: that is
/// only RDHUP. A full close (process killed, timed out and dropped) is HUP.
#[cfg(unix)]
fn peer_closed(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, zero timeout.
    let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
    ready > 0 && pfd.revents & libc::POLLHUP != 0
}

#[cfg(not(unix))]
fn peer_closed(_stream: &UnixStream) -> bool {
    false
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The failure's `ErrorKind` wire name (`not_shared`, `no_session`), so
    /// the client exits with the contract code without parsing `error`.
    /// Absent from older daemons and on generic failures: exit 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error_kind: Option<String>,
}

impl SessionResponse {
    /// The client-side error for a failed (`ok: false`) response: the
    /// daemon's text verbatim, classified by its `error_kind` when this
    /// binary knows the kind.
    fn into_error(self) -> anyhow::Error {
        let message = self
            .error
            .unwrap_or_else(|| "session IPC request failed".to_owned());
        match self.error_kind.as_deref().and_then(ErrorKind::from_wire) {
            Some(kind) => kinded(kind, message),
            None => anyhow::anyhow!("{message}"),
        }
    }
}

/// How long a liveness probe may wait for the daemon's reply. The daemon
/// binds its socket BEFORE the portal session is up, so while the consent
/// dialog is open a connect succeeds but no reply ever comes; without a bound
/// every `doctor` / `session-start` / `session-end` would hang on it.
const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for reading a request from / writing a reply to one client.
/// Clients are served one at a time, so without it a stopped client would
/// freeze the daemon (SIGTERM handling included).
const IPC_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `session-end` waits for the Shutdown reply. A daemon that does
/// not answer within this (stopped, wedged, behind its consent dialog, busy)
/// is signalled right away.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_millis(1500);
/// Grace period after a Shutdown reply for the daemon to tear down and exit;
/// above `TEARDOWN_LIMIT`, so a daemon that answered is not killed inside its
/// own (bounded) teardown. With the request timeout and the SIGKILL wait,
/// `session-end` stays under the JS exit hook's 5 s budget.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// The daemon's own teardown budget (held buttons, PipeWire, portal session).
const TEARDOWN_LIMIT: Duration = Duration::from_millis(1800);
/// How long to wait for the process to disappear after SIGKILL.
const KILL_WAIT: Duration = Duration::from_millis(500);
/// Margin between the client's deadline and its own timeout, so the daemon
/// never starts an action in the instant the client gives up.
const DEADLINE_MARGIN: Duration = Duration::from_millis(250);
/// How long `session-start` waits for the daemon to become ready. Covers a
/// human reading and answering the consent dialog (measured: ~50 s), with
/// margin; the JS executor's async session-start budget sits above this.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
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
    let pid = read_pidfile()?;
    (is_daemon_process(pid) && pid_alive(pid)).then_some(pid)
}

fn read_pidfile() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_path().ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (pid > 1).then_some(pid)
}

fn is_daemon_process(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| is_daemon_cmdline(&cmdline))
}

fn is_daemon_cmdline(cmdline: &[u8]) -> bool {
    cmdline
        .split(|b| *b == 0)
        .any(|part| part == b"serve-session")
}

/// Whether `pid` is a live (non-zombie) process.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) only probes for existence/permission.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // A zombie still "exists" to kill(2) until its parent reaps it.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => proc_stat_state(&stat) != Some('Z'),
        Err(_) => false,
    }
}

/// The state letter of a `/proc/<pid>/stat` line (after the `(comm)` field,
/// which may itself contain spaces and parentheses).
fn proc_stat_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// The daemon process, held by a pidfd where the kernel has them (Linux
/// 5.3+), so a signal can never reach a different process that got the pid
/// after the daemon exited.
struct DaemonProcess {
    pid: i32,
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl DaemonProcess {
    /// Pin `pid`, which was validated as our daemon. The cmdline is checked
    /// again AFTER the pidfd is open: the pidfd refers to whatever process
    /// had the pid at that moment, so this closes the reuse window.
    fn open(pid: i32) -> Option<Self> {
        // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let pidfd = if fd >= 0 {
            // SAFETY: a fresh fd we own.
            Some(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd as i32) })
        } else {
            None
        };
        is_daemon_process(pid).then_some(Self { pid, pidfd })
    }

    fn alive(&self) -> bool {
        match &self.pidfd {
            Some(fd) => {
                use std::os::fd::AsRawFd;
                // A pidfd turns readable when the process exits (zombie or not).
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd, zero timeout.
                let ready = unsafe { libc::poll(&mut pfd, 1, 0) };
                ready == 0
            }
            None => pid_alive(self.pid),
        }
    }

    fn signal(&self, signal: i32) {
        match &self.pidfd {
            Some(fd) => {
                use std::os::fd::AsRawFd;
                // SAFETY: pidfd_send_signal on our pidfd, no siginfo.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
            }
            // SAFETY: plain signal delivery to a pid verified to be our daemon.
            None => unsafe {
                libc::kill(self.pid, signal);
            },
        }
    }

    /// Wait up to `grace` for the process to exit; true when it did.
    async fn wait_for_exit(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !self.alive() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !self.alive()
    }
}

/// Make sure the daemon process is gone and say how it ended. A daemon that
/// answered Shutdown gets `SHUTDOWN_GRACE` to tear down, else SIGKILL; one
/// that did not answer gets SIGTERM at once, then SIGKILL. Worst case with
/// the Shutdown request: 1.5 + 2 + 0.5 s. A `session-end` that leaves
/// `serve-session` behind also leaves the GNOME screen-sharing indicator on
/// (the portal session stays open), so this is not optional cleanup
/// (claude-desktop-extra issue #293).
async fn reap_daemon(daemon: &DaemonProcess, answered: bool) -> &'static str {
    if answered && daemon.wait_for_exit(SHUTDOWN_GRACE).await {
        return "exited";
    }
    if !daemon.alive() {
        return "exited";
    }
    if !answered {
        daemon.signal(libc::SIGTERM);
        if daemon.wait_for_exit(TERM_GRACE).await {
            return "sigterm";
        }
    }
    daemon.signal(libc::SIGKILL);
    daemon.wait_for_exit(KILL_WAIT).await;
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
            let outcome = wait_until_ready(&socket, || pid_alive(pid)).await;
            match after_wait(outcome, Spawner::Earlier) {
                AfterWait::Done(result) => return result,
                // It was on its way out (e.g. its session was lost and it was
                // tearing down): the caller asked for a session, so start one.
                AfterWait::SpawnFresh => {
                    eprintln!(
                        "gnome-portal-bridge: the session daemon {pid} exited while starting; \
                         spawning a new one"
                    );
                }
            }
        }
        DaemonState::Down => {}
    }
    spawn_session_daemon(&socket).await
}

/// Spawn a new detached daemon and wait until it is ready.
async fn spawn_session_daemon(socket: &Path) -> Result<Value> {
    cleanup_stale_socket(socket).await?;

    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    // The daemon's stderr goes to a log next to the socket (truncated per
    // start) so a portal or PipeWire failure inside the session can be read
    // after the fact; nothing else ever sees that stream.
    let log_dir = runtime_dir();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create `{}`", log_dir.display()))?;
    let log = std::fs::File::create(log_path()?)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut command = Command::new(current_exe);
    command
        .arg("serve-session")
        .arg("--socket")
        .arg(socket)
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
    let outcome = wait_until_ready(socket, || {
        child.try_wait().map(|s| s.is_none()).unwrap_or(true)
    })
    .await;
    match after_wait(outcome, Spawner::ThisCall) {
        AfterWait::Done(result) => result,
        AfterWait::SpawnFresh => unreachable!("a daemon this call spawned is never respawned"),
    }
}

/// How waiting for a daemon to become ready ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Ready,
    /// The daemon process went away before it answered.
    Exited,
    /// Still not answering after `READY_TIMEOUT` (consent dialog open).
    TimedOut,
}

/// Who started the daemon `session-start` waited on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spawner {
    /// It existed before this `session-start`.
    Earlier,
    /// This `session-start` spawned it.
    ThisCall,
}

/// What `session-start` does next.
#[derive(Debug)]
enum AfterWait {
    Done(Result<Value>),
    SpawnFresh,
}

/// A daemon this call spawned that exits means the consent dialog was
/// declined or the portal failed: an error. A daemon that predates the call
/// and exits while we wait was ending anyway (session lost, or a
/// `session-end` racing us); failing then would latch Computer Use down in
/// the executor although nothing is wrong, so a fresh daemon is spawned.
fn after_wait(outcome: WaitOutcome, spawner: Spawner) -> AfterWait {
    match (outcome, spawner) {
        (WaitOutcome::Ready, _) => {
            AfterWait::Done(Ok(serde_json::json!({ "ok": true, "session": "daemon" })))
        }
        (WaitOutcome::Exited, Spawner::Earlier) => AfterWait::SpawnFresh,
        (WaitOutcome::Exited, Spawner::ThisCall) => AfterWait::Done(Err(anyhow::anyhow!(
            "session daemon exited before becoming ready (consent dialog declined or \
             closed, or the portal failed){}",
            log_hint()
        ))),
        (WaitOutcome::TimedOut, _) => AfterWait::Done(Err(anyhow::anyhow!(
            "still waiting for the portal consent dialog after {}s; the session daemon \
             keeps waiting - answer the dialog, then run session-start again{}",
            READY_TIMEOUT.as_secs(),
            log_hint()
        ))),
    }
}

/// Poll the socket until the daemon answers. Gives up early when the daemon
/// process dies (consent declined -> `serve_session` exits), else after
/// `READY_TIMEOUT`, leaving the daemon to wait on: a later `session-start`
/// picks it up instead of opening a second dialog.
async fn wait_until_ready(socket: &Path, mut alive: impl FnMut() -> bool) -> WaitOutcome {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if tokio::time::timeout(PING_TIMEOUT, ping(socket))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return WaitOutcome::Ready;
        }
        if !alive() {
            return WaitOutcome::Exited;
        }
        if Instant::now() >= deadline {
            return WaitOutcome::TimedOut;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Stop the daemon if one is running (idempotent): ask it to shut down over
/// the socket, then make sure the process is actually gone. Bounded: a daemon
/// that is stopped, wedged or behind its consent dialog is signalled instead
/// of waited on, so an app quit can never hang here.
pub async fn stop_session_daemon() -> Result<Value> {
    let socket = socket_path()?;
    let daemon = live_daemon_pid().and_then(DaemonProcess::open);
    let answered = socket.exists()
        && matches!(
            request_daemon::<Value>(&socket, &SessionRequest::Shutdown, SHUTDOWN_REQUEST_TIMEOUT)
                .await,
            DaemonReply::Reply(Ok(_))
        );
    // Contract v1: `ended` only when a daemon was actually there (a process,
    // or at least something that answered the Shutdown request).
    let outcome = match daemon {
        Some(daemon) => {
            let outcome = reap_daemon(&daemon, answered).await;
            // A daemon that had to be killed left its files behind.
            std::fs::remove_file(&socket).ok();
            if read_pidfile() == Some(daemon.pid)
                && let Ok(pidfile) = pid_path()
            {
                std::fs::remove_file(pidfile).ok();
            }
            outcome
        }
        None if answered => "shutdown-requested",
        None => "none",
    };
    Ok(serde_json::json!({
        "ok": true,
        "ended": outcome != "none",
        "daemon": outcome,
    }))
}

/// `forget-session`: end any running daemon, then delete the restore token.
/// Without this, `session-end` + `session-start` silently replays the stored
/// grant - the same monitor selection - so "re-share" changes nothing. Never
/// creates a directory and never calls D-Bus (contract v1).
pub async fn forget_session() -> Result<Value> {
    let store = crate::token_store::TokenStore::locate()?;
    let mut result = stop_session_daemon().await?;
    result["token_removed"] = Value::Bool(store.remove()?);
    result["token_path"] = Value::String(store.path().display().to_string());
    Ok(result)
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
    let mut files = DaemonFiles::new(socket.clone(), pidfile);

    let mut session = match LivePortalSession::open().await {
        Ok(session) => session,
        Err(error) => {
            files.release();
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
                    let (request, deadline) =
                        match tokio::time::timeout(IPC_IO_TIMEOUT, read_request(&mut stream)).await {
                            Ok(Ok(parts)) => parts,
                            Ok(Err(error)) => {
                                eprintln!("gnome-portal-bridge: dropped a client request: {error:#}");
                                continue;
                            }
                            Err(_) => {
                                eprintln!("gnome-portal-bridge: a client timed out sending its request");
                                continue;
                            }
                        };
                    // `session-start` polls with pings that time out while the
                    // consent dialog is open; the listener queues them and they
                    // are answered after their client left. Expected, not logged.
                    let is_ping = matches!(request, SessionRequest::Ping);
                    let refusal = if request.is_input() {
                        input_refusal(deadline, monotonic_now_ns(), peer_closed(&stream))
                    } else {
                        None
                    };
                    let (response, should_shutdown) = match refusal {
                        Some(reason) => {
                            eprintln!("gnome-portal-bridge: refused a stale input request: {reason}");
                            (respond_err(&anyhow::anyhow!("{reason}")), false)
                        }
                        None => handle_request(&mut session, request).await,
                    };
                    // A session that lost its capture path for good ends, so
                    // the next session-start opens a fresh one. Leaving
                    // (Shutdown, or a lost session) gives up the socket and
                    // pidfile BEFORE the reply: a session-start the reply
                    // triggers must find no daemon, not this one dying.
                    let leaving = should_shutdown || session.is_dead();
                    if leaving {
                        files.release();
                    }
                    let written = tokio::time::timeout(IPC_IO_TIMEOUT, write_response(&mut stream, response)).await;
                    match written {
                        Ok(Ok(())) => {}
                        _ if is_ping || refusal.is_some() => {}
                        Ok(Err(error)) => {
                            eprintln!("gnome-portal-bridge: client went away before the reply: {error:#}");
                        }
                        Err(_) => {
                            eprintln!("gnome-portal-bridge: a client timed out reading the reply");
                        }
                    }
                    if leaving {
                        if !should_shutdown {
                            eprintln!("gnome-portal-bridge: the portal session is unusable; exiting");
                        }
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Gone from the socket path before the (up to TEARDOWN_LIMIT) teardown,
    // so a session-start meanwhile spawns a new daemon instead of waiting on
    // this one (SIGTERM and errors reach here without the release above).
    drop(listener);
    files.release();
    // Bounded: a portal/PipeWire teardown that never completes must not keep
    // the daemon (and the compositor's screen-sharing indicator) alive.
    let _ = tokio::time::timeout(TEARDOWN_LIMIT, session.shutdown()).await;
    serve_result
}

/// The daemon's socket and pidfile. Released once, and only while they are
/// still this daemon's: after the release a new daemon may already own both
/// paths, so a late removal would orphan it.
struct DaemonFiles {
    socket: PathBuf,
    /// (device, inode) of the socket this daemon bound.
    socket_id: Option<(u64, u64)>,
    pidfile: PathBuf,
    released: bool,
}

impl DaemonFiles {
    fn new(socket: PathBuf, pidfile: PathBuf) -> Self {
        let socket_id = file_id(&socket);
        Self {
            socket,
            socket_id,
            pidfile,
            released: false,
        }
    }

    fn release(&mut self) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        if self.socket_id.is_some() && file_id(&self.socket) == self.socket_id {
            std::fs::remove_file(&self.socket).ok();
        }
        let ours = std::fs::read_to_string(&self.pidfile)
            .is_ok_and(|pid| pid.trim() == std::process::id().to_string());
        if ours {
            std::fs::remove_file(&self.pidfile).ok();
        }
    }
}

fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()))
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

/// Run one action against the daemon if one is running, else against an
/// ephemeral session. Screen resolution is done by the caller (the Mutter
/// connection lives on the client side); the resolved `Screen` is embedded in
/// the request.
///
/// Only an ABSENT daemon (no socket, or a stale socket nobody listens on)
/// falls back to an ephemeral session. A daemon that is there but fails,
/// errors or times out returns that error: a second portal session per
/// action is a consent dialog per action, and it may share a different
/// monitor than the daemon's session - a click on the wrong monitor with no
/// error (claude-desktop-extra #293).
pub async fn run_action(request: SessionRequest) -> Result<Value> {
    let socket = socket_path()?;
    match request_daemon(&socket, &request, request_timeout(&request)).await {
        DaemonReply::Reply(result) => result,
        DaemonReply::NotRunning => run_ephemeral(request).await,
    }
}

/// Client-side budget for one daemon request. It must cover the daemon's own
/// worst case - a fresh stream (`CreateStream` <= 10 s, first frame <= 10 s for
/// a screenshot or <= 2 s for pointer input) after a Mutter query on the
/// mismatch path (<= 2 s) - and stay below the JS executor's 30 s execFile
/// timeout for portal commands, so a stuck daemon surfaces as our error (with
/// the log path) instead of a killed process. Requests that sleep by design
/// get their sleep on top.
fn request_timeout(request: &SessionRequest) -> Duration {
    const BASE: Duration = Duration::from_secs(25);
    let extra_ms = match request {
        SessionRequest::HoldKey { duration_ms, .. } => *duration_ms,
        SessionRequest::Type { text, delay_ms } => {
            delay_ms.saturating_mul(text.chars().count() as u64)
        }
        _ => 0,
    };
    BASE.saturating_add(Duration::from_millis(extra_ms))
}

/// What a request to the session daemon found.
enum DaemonReply<T> {
    /// No daemon: no socket, or a stale socket nobody listens on.
    NotRunning,
    /// A daemon accepted the connection; this is its answer or the failure.
    Reply(Result<T>),
}

/// Whether a failed connect means "no daemon" (fall back) rather than "a
/// daemon that is broken" (report).
fn daemon_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Send one request to the daemon, bounded by `timeout` end to end.
async fn request_daemon<T: DeserializeOwned>(
    socket: &Path,
    request: &SessionRequest,
    timeout: Duration,
) -> DaemonReply<T> {
    if !socket.exists() {
        return DaemonReply::NotRunning;
    }
    let stream = match tokio::time::timeout(PING_TIMEOUT, UnixStream::connect(socket)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if daemon_absent(&error) => return DaemonReply::NotRunning,
        Ok(Err(error)) => {
            return DaemonReply::Reply(Err(anyhow::Error::new(error).context(format!(
                "failed to connect to the session daemon at `{}`{}",
                socket.display(),
                log_hint()
            ))));
        }
        Err(_) => {
            return DaemonReply::Reply(Err(anyhow::anyhow!(
                "the session daemon at `{}` did not accept the connection within {}s{}",
                socket.display(),
                PING_TIMEOUT.as_secs(),
                log_hint()
            )));
        }
    };
    // Input carries the instant this client stops waiting; the daemon will
    // not START it after that (see `SessionRequest::is_input`).
    let deadline = request.is_input().then(|| {
        let budget = timeout.saturating_sub(DEADLINE_MARGIN);
        monotonic_now_ns().saturating_add(u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX))
    });
    match tokio::time::timeout(timeout, exchange(stream, request, deadline)).await {
        Ok(result) => DaemonReply::Reply(result),
        Err(_) => DaemonReply::Reply(Err(anyhow::anyhow!(
            "the session daemon did not answer within {}s{}{}",
            timeout.as_secs(),
            if request.is_input() {
                "; the action will not start anymore, but it may have run (or be finishing)"
            } else {
                ""
            },
            log_hint()
        ))),
    }
}

/// "; see <daemon.log>" for transport-level daemon failures.
fn log_hint() -> String {
    log_path()
        .map(|path| format!("; see {}", path.display()))
        .unwrap_or_default()
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
        Err(response.into_error())
    }
}

/// Resolve a `Screen` by optional display selector, preferring the live Mutter
/// DisplayConfig and falling back to the portal streams via an ephemeral probe.
/// Used by capture/pointer commands to build their request.
pub async fn resolve_screen(display: Option<&str>) -> Result<Screen> {
    let screens = match display {
        Some(_) => list_screens_best_effort().await?,
        // The default screen should be one the daemon can act on, so ask
        // which monitors its session shares (see `pick_screen`).
        None => list_screens_with_sharing(Ephemeral::Allowed).await?,
    };
    pick_screen(&screens, display)
}

/// Whether a screen listing may open an ephemeral portal session when
/// neither Mutter nor a daemon can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ephemeral {
    /// Pointer/capture commands, which open a session anyway.
    Allowed,
    /// The `screens` command: contract v1 says it never opens a portal
    /// session (nor shows a consent dialog).
    Refused,
}

/// List screens for pointer/capture commands: Mutter first, portal-stream
/// fallback.
pub async fn list_screens_best_effort() -> Result<Vec<Screen>> {
    if let Some(screens) = mutter_screens().await {
        return Ok(screens);
    }
    Ok(stream_screens_without_mutter(Ephemeral::Allowed).await?.0)
}

/// Screens when Mutter is unavailable, and whether they came from a running
/// daemon (true) or an ephemeral session (false).
async fn stream_screens_without_mutter(ephemeral: Ephemeral) -> Result<(Vec<Screen>, bool)> {
    // Fallback 1: the running daemon already holds a portal session - ask it
    // for its streams instead of opening (and prompting for) a second one.
    // A daemon that is there but fails is an error, not a reason for a
    // second session (see `run_action`).
    match daemon_stream_screens_reply().await {
        DaemonReply::Reply(Ok(screens)) if !screens.is_empty() => {
            eprintln!(
                "gnome-portal-bridge: screens derived from the running portal session's streams"
            );
            Ok((screens, true))
        }
        DaemonReply::Reply(Ok(_)) => bail!("the session daemon reported no screens"),
        DaemonReply::Reply(Err(error)) => Err(error),
        DaemonReply::NotRunning if ephemeral == Ephemeral::Refused => bail!(
            "Mutter DisplayConfig unavailable and no session running; cannot list monitors \
             without opening a portal session"
        ),
        DaemonReply::NotRunning => Ok((ephemeral_stream_screens().await?, false)),
    }
}

/// The `screens` command: like `list_screens_best_effort`, plus `is_shared`
/// on every screen while a session daemon answers - true iff the daemon's
/// portal session has a stream at that screen's origin. Without a daemon the
/// field is omitted (asking would need a portal session of its own). With
/// neither Mutter nor a daemon, `Ephemeral::Refused` (the `screens` command)
/// fails instead of opening an ephemeral session; the default-screen lookup
/// of capture commands passes `Allowed`.
pub async fn list_screens_with_sharing(ephemeral: Ephemeral) -> Result<Vec<Screen>> {
    if let Some(mut screens) = mutter_screens().await {
        if let Some(streams) = daemon_stream_screens().await {
            mark_shared(&mut screens, &streams);
        }
        return Ok(screens);
    }
    let (mut screens, from_daemon) = stream_screens_without_mutter(ephemeral).await?;
    // Without Mutter, a daemon's screens ARE its streams; an ephemeral
    // session's are not "shared" in the daemon sense, so leave them unset.
    if from_daemon {
        for screen in &mut screens {
            screen.is_shared = Some(true);
        }
    }
    Ok(screens)
}

/// Set `is_shared` on each screen by origin against the daemon's streams.
fn mark_shared(screens: &mut [Screen], streams: &[Screen]) {
    for screen in screens {
        let origin = (screen.geometry.x, screen.geometry.y);
        screen.is_shared = Some(
            streams
                .iter()
                .any(|s| (s.geometry.x, s.geometry.y) == origin),
        );
    }
}

async fn mutter_screens() -> Option<Vec<Screen>> {
    match zbus::Connection::session().await {
        Ok(connection) => match screens::list_screens(&connection).await {
            Ok(screens) if !screens.is_empty() => return Some(screens),
            Ok(_) => eprintln!("gnome-portal-bridge: Mutter DisplayConfig reported no monitors"),
            Err(error) => {
                eprintln!("gnome-portal-bridge: Mutter DisplayConfig unavailable: {error:#}")
            }
        },
        Err(error) => eprintln!("gnome-portal-bridge: no session bus: {error:#}"),
    }
    None
}

/// The running daemon's stream-derived screens, if a daemon answers within
/// the ping budget. No socket = no daemon = returns at once.
async fn daemon_stream_screens() -> Option<Vec<Screen>> {
    match daemon_stream_screens_reply().await {
        DaemonReply::Reply(Ok(screens)) if !screens.is_empty() => Some(screens),
        _ => None,
    }
}

async fn daemon_stream_screens_reply() -> DaemonReply<Vec<Screen>> {
    match socket_path() {
        Ok(socket) => request_daemon(&socket, &SessionRequest::Screens, PING_TIMEOUT).await,
        Err(_) => DaemonReply::NotRunning,
    }
}

/// Fallback 2: no daemon - open a short ephemeral session (consent dialog
/// unless a restore token is accepted).
async fn ephemeral_stream_screens() -> Result<Vec<Screen>> {
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

/// Select a screen by id/name. Without a selector: when sharing is known (a
/// daemon answered) and some screen is shared, the shared active/primary
/// screen, else the first shared one - the Mutter primary may be a monitor
/// the user did not share, which would only produce a "not shared" error.
/// Otherwise the active/primary/first screen.
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
    let shared: Vec<&Screen> = screens
        .iter()
        .filter(|s| s.is_shared == Some(true))
        .collect();
    let pool: Vec<&Screen> = if shared.is_empty() {
        screens.iter().collect()
    } else {
        shared
    };
    Ok(pool
        .iter()
        .find(|s| s.is_active)
        .or_else(|| pool.iter().find(|s| s.is_primary))
        .or_else(|| pool.first())
        .map(|s| (*s).clone())
        .expect("screens is non-empty"))
}

// ---- socket IO ----

async fn send<T: DeserializeOwned>(socket: &Path, request: &SessionRequest) -> Result<T> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("failed to connect to session socket `{}`", socket.display()))?;
    exchange(stream, request, None).await
}

/// Write one request on a connected stream and read the reply. A daemon-side
/// failure (`ok: false`) is returned verbatim; transport failures carry the
/// daemon log path.
async fn exchange<T: DeserializeOwned>(
    mut stream: UnixStream,
    request: &SessionRequest,
    deadline_mono_ns: Option<u64>,
) -> Result<T> {
    // Without a deadline the bare request goes out, as older daemons expect.
    let payload = match deadline_mono_ns {
        Some(deadline) => serde_json::to_vec(&serde_json::json!({
            "request": request,
            "deadline_mono_ns": deadline,
        }))?,
        None => serde_json::to_vec(request)?,
    };
    stream
        .write_all(&payload)
        .await
        .with_context(|| format!("failed to write session IPC request{}", log_hint()))?;
    stream
        .shutdown()
        .await
        .with_context(|| format!("failed to finish writing session IPC request{}", log_hint()))?;

    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("failed to read session IPC response{}", log_hint()))?;
    let response: SessionResponse = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to decode session IPC response (daemon died mid-request?){}",
            log_hint()
        )
    })?;

    if !response.ok {
        return Err(response.into_error());
    }
    let value = response.result.unwrap_or(Value::Null);
    serde_json::from_value(value).context("failed to decode session IPC payload")
}

async fn read_request(stream: &mut UnixStream) -> Result<(SessionRequest, Option<u64>)> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .context("failed to read session IPC request")?;
    decode_request(&bytes)
}

fn decode_request(bytes: &[u8]) -> Result<(SessionRequest, Option<u64>)> {
    serde_json::from_slice::<WireRequest>(bytes)
        .map(WireRequest::into_parts)
        .context("failed to decode session IPC request")
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

/// The detached daemon's stderr log, next to the socket. Only the path:
/// `session-start` creates the directory, `doctor` and error hints must not.
pub fn log_path() -> Result<PathBuf> {
    Ok(runtime_dir().join("daemon.log"))
}

/// `${XDG_RUNTIME_DIR:-/tmp}/gnome-portal-bridge` (contract v1): an empty
/// `XDG_RUNTIME_DIR` counts as unset, as with the shell's `:-`.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("gnome-portal-bridge")
}

fn respond_ok(value: Value) -> SessionResponse {
    SessionResponse {
        ok: true,
        result: Some(value),
        error: None,
        error_kind: None,
    }
}

fn respond_err(error: &anyhow::Error) -> SessionResponse {
    SessionResponse {
        ok: false,
        result: None,
        error: Some(format!("{error:#}")),
        error_kind: kind_of(error).map(|kind| kind.as_wire().to_owned()),
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
                error_kind: None,
            },
        },
        Err(error) => respond_err(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Rect;

    fn screen(id: &str, x: i32, y: i32, w: i32, h: i32) -> Screen {
        Screen {
            id: id.to_owned(),
            name: id.to_owned(),
            geometry: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            scale: Some(1.0),
            refresh_millihz: None,
            is_active: false,
            is_primary: false,
            is_shared: None,
            source: crate::output::ScreenSource::Mutter,
        }
    }

    #[test]
    fn only_a_missing_listener_counts_as_no_daemon() {
        use std::io::{Error, ErrorKind};
        assert!(daemon_absent(&Error::from(ErrorKind::NotFound)));
        assert!(daemon_absent(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!daemon_absent(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!daemon_absent(&Error::from(ErrorKind::TimedOut)));
    }

    fn temp_socket(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gpb-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("session.sock")
    }

    #[tokio::test]
    async fn no_socket_and_stale_socket_fall_back() {
        let socket = temp_socket("stale");
        let reply: DaemonReply<Value> =
            request_daemon(&socket, &SessionRequest::Ping, Duration::from_secs(1)).await;
        assert!(matches!(reply, DaemonReply::NotRunning));
        // A socket file whose listener is gone (daemon crashed) = no daemon.
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let reply: DaemonReply<Value> =
            request_daemon(&socket, &SessionRequest::Ping, Duration::from_secs(1)).await;
        assert!(matches!(reply, DaemonReply::NotRunning));
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_live_daemon_that_hangs_is_an_error_not_a_fallback() {
        let socket = temp_socket("hang");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept and never answer.
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let reply: DaemonReply<Value> =
            request_daemon(&socket, &SessionRequest::Ping, Duration::from_millis(200)).await;
        match reply {
            DaemonReply::Reply(Err(error)) => {
                let message = format!("{error:#}");
                assert!(message.contains("did not answer within"), "{message}");
                assert!(message.contains("daemon.log"), "{message}");
            }
            _ => panic!("expected an error reply"),
        }
        server.abort();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_daemon_side_error_is_returned_verbatim() {
        let socket = temp_socket("err");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await.unwrap();
            let response = respond_err(&anyhow::anyhow!("monitor eDP-1 is not shared"));
            write_response(&mut stream, response).await.unwrap();
        });
        let reply: DaemonReply<Value> =
            request_daemon(&socket, &SessionRequest::Ping, Duration::from_secs(2)).await;
        match reply {
            DaemonReply::Reply(Err(error)) => {
                assert_eq!(error.to_string(), "monitor eDP-1 is not shared")
            }
            _ => panic!("expected the daemon's error"),
        }
        server.await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    /// Serve one connection with raw reply bytes, as any daemon version might.
    async fn reply_once(tag: &str, reply: Vec<u8>) -> anyhow::Error {
        let socket = temp_socket(tag);
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await.unwrap();
            stream.write_all(&reply).await.unwrap();
        });
        let reply: DaemonReply<Value> =
            request_daemon(&socket, &SessionRequest::Ping, Duration::from_secs(2)).await;
        server.await.unwrap();
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
        match reply {
            DaemonReply::Reply(Err(error)) => error,
            _ => panic!("expected the daemon's error"),
        }
    }

    #[tokio::test]
    async fn error_kind_crosses_the_socket() {
        for kind in [ErrorKind::NotShared, ErrorKind::NoSession] {
            let response = respond_err(&kinded(kind, "the daemon's text").context("outer"));
            assert_eq!(response.error_kind.as_deref(), Some(kind.as_wire()));
            let error = reply_once("kind", serde_json::to_vec(&response).unwrap()).await;
            assert_eq!(kind_of(&error), Some(kind));
            assert_eq!(error.to_string(), "outer: the daemon's text");
        }
        // Generic failures carry no kind on the wire at all.
        let bytes = serde_json::to_vec(&respond_err(&anyhow::anyhow!("plain"))).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("error_kind"));
    }

    #[tokio::test]
    async fn an_old_or_newer_daemon_reply_is_a_generic_failure() {
        // A daemon from before contract v1 (still running after an upgrade):
        // no `error_kind`, the not-shared text with its old remedy tail.
        let old = br#"{"ok":false,"error":"monitor eDP-1 (0,0 1920x1080) is not shared in the portal session (shared: HDMI-1 at 1920,0 2560x1440); run `gnome-portal-bridge forget-session`, then start Computer Use again and select all monitors in the dialog"}"#;
        let error = reply_once("old", old.to_vec()).await;
        assert_eq!(kind_of(&error), None);
        assert!(error.to_string().starts_with("monitor eDP-1"));
        // A kind this binary does not know.
        let newer = br#"{"ok":false,"error":"x","error_kind":"from_the_future"}"#;
        let error = reply_once("newer", newer.to_vec()).await;
        assert_eq!(kind_of(&error), None);
        assert_eq!(error.to_string(), "x");
    }

    #[test]
    fn request_timeout_covers_sleeps_and_stays_under_the_js_budget() {
        assert!(request_timeout(&SessionRequest::LeftMouseUp) < Duration::from_secs(30));
        assert!(request_timeout(&SessionRequest::LeftMouseUp) > Duration::from_secs(22));
        let hold = SessionRequest::HoldKey {
            keys: vec!["a".to_owned()],
            duration_ms: 4000,
        };
        assert_eq!(request_timeout(&hold), Duration::from_secs(29));
    }

    #[tokio::test]
    async fn ready_wait_reports_a_dead_daemon_as_exited() {
        let socket = temp_socket("ready");
        assert_eq!(
            wait_until_ready(&socket, || false).await,
            WaitOutcome::Exited
        );
        std::fs::remove_dir_all(socket.parent().unwrap()).ok();
    }

    #[test]
    fn only_an_earlier_daemon_that_exits_is_replaced() {
        // Ours exited: consent declined or the portal failed - an error.
        match after_wait(WaitOutcome::Exited, Spawner::ThisCall) {
            AfterWait::Done(Err(error)) => assert!(
                error
                    .to_string()
                    .starts_with("session daemon exited before becoming ready"),
                "{error}"
            ),
            other => panic!("expected an error, got {other:?}"),
        }
        // One that predates the call was ending anyway: start a fresh one.
        assert!(matches!(
            after_wait(WaitOutcome::Exited, Spawner::Earlier),
            AfterWait::SpawnFresh
        ));
        for spawner in [Spawner::Earlier, Spawner::ThisCall] {
            match after_wait(WaitOutcome::Ready, spawner) {
                AfterWait::Done(Ok(value)) => {
                    assert_eq!(value, serde_json::json!({"ok": true, "session": "daemon"}))
                }
                other => panic!("expected ready, got {other:?}"),
            }
            // A dialog still open is never a reason for a second daemon.
            assert!(matches!(
                after_wait(WaitOutcome::TimedOut, spawner),
                AfterWait::Done(Err(_))
            ));
        }
    }

    #[tokio::test]
    async fn released_files_are_removed_only_while_still_ours() {
        let socket = temp_socket("files");
        let dir = socket.parent().unwrap().to_path_buf();
        let pidfile = dir.join("session.pid");

        // Ours: both go.
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::write(&pidfile, std::process::id().to_string()).unwrap();
        let mut files = DaemonFiles::new(socket.clone(), pidfile.clone());
        files.release();
        assert!(!socket.exists() && !pidfile.exists());
        drop(listener);

        // A new daemon took both paths before our (late) release: both stay.
        let _ours = UnixListener::bind(&socket).unwrap();
        std::fs::write(&pidfile, std::process::id().to_string()).unwrap();
        let mut files = DaemonFiles::new(socket.clone(), pidfile.clone());
        std::fs::remove_file(&socket).unwrap();
        let _theirs = UnixListener::bind(&socket).unwrap();
        std::fs::write(&pidfile, "999999").unwrap();
        files.release();
        assert!(socket.exists());
        assert_eq!(std::fs::read_to_string(&pidfile).unwrap(), "999999");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn default_screen_prefers_shared_monitors() {
        let mut edp = screen("eDP-1", 0, 240, 1920, 1200);
        edp.is_primary = true;
        edp.is_active = true;
        let hdmi = screen("HDMI-1", 1920, 0, 3440, 1440);
        let dp = screen("DP-1", 5360, 0, 1920, 1080);

        // No daemon (sharing unknown): the Mutter primary.
        let screens = vec![edp.clone(), hdmi.clone(), dp.clone()];
        assert_eq!(pick_screen(&screens, None).unwrap().id, "eDP-1");

        // Primary not shared: the first shared screen.
        let mut screens = screens;
        mark_shared(&mut screens, &[dp.clone(), hdmi.clone()]);
        assert_eq!(pick_screen(&screens, None).unwrap().id, "HDMI-1");

        // A shared primary wins.
        mark_shared(&mut screens, &[dp.clone(), edp.clone()]);
        assert_eq!(pick_screen(&screens, None).unwrap().id, "eDP-1");

        // Nothing shared at all: the old rule (the daemon then reports it).
        mark_shared(&mut screens, &[]);
        assert_eq!(pick_screen(&screens, None).unwrap().id, "eDP-1");

        // An explicit selector is honored even if not shared.
        mark_shared(&mut screens, &[hdmi]);
        assert_eq!(pick_screen(&screens, Some("eDP-1")).unwrap().id, "eDP-1");
    }

    #[test]
    fn mark_shared_matches_streams_by_origin() {
        let mut screens = vec![
            screen("eDP-1", 0, 240, 1920, 1200),
            screen("HDMI-1", 1920, 0, 3440, 1440),
        ];
        let streams = vec![screen("stream-51", 1920, 0, 3440, 1440)];
        mark_shared(&mut screens, &streams);
        assert_eq!(screens[0].is_shared, Some(false));
        assert_eq!(screens[1].is_shared, Some(true));
    }

    #[test]
    fn is_shared_is_omitted_from_json_when_unknown() {
        let mut s = screen("eDP-1", 0, 0, 1920, 1080);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("is_shared"), "{json}");
        assert!(
            json.ends_with(r#""is_active":false,"is_primary":false}"#),
            "{json}"
        );
        s.is_shared = Some(true);
        s.source = crate::output::ScreenSource::Stream;
        assert!(
            serde_json::to_string(&s)
                .unwrap()
                .ends_with(r#""is_shared":true,"source":"stream"}"#)
        );
        // Requests carry the provenance; a missing field means Mutter.
        let round: Screen = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.source, crate::output::ScreenSource::Stream);
        let old: Screen = serde_json::from_str(&json).unwrap();
        assert_eq!(old.source, crate::output::ScreenSource::Mutter);
        s.source = crate::output::ScreenSource::Mutter;
        assert!(
            serde_json::to_string(&s)
                .unwrap()
                .ends_with(r#""is_primary":false,"is_shared":true}"#)
        );
    }

    // ---- stale input refusal ----

    #[test]
    fn expired_deadline_refuses_input() {
        assert!(input_refusal(Some(100), 100, false).is_some());
        assert!(input_refusal(Some(100), 200, false).is_some());
        assert!(input_refusal(Some(100), 99, false).is_none());
        // No deadline (older client): only a hang-up refuses.
        assert!(input_refusal(None, u64::MAX, false).is_none());
        assert!(input_refusal(None, 0, true).is_some());
    }

    #[test]
    fn deadline_is_on_the_monotonic_clock() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a > 0 && b >= a);
    }

    #[test]
    fn input_classification() {
        let s = screen("A", 0, 0, 100, 100);
        assert!(
            SessionRequest::Click {
                screen: s.clone(),
                x: 1,
                y: 1,
                button: "left".into(),
                count: 1,
                modifiers: vec![],
            }
            .is_input()
        );
        assert!(
            SessionRequest::Type {
                text: "x".into(),
                delay_ms: 0
            }
            .is_input()
        );
        assert!(!SessionRequest::Screenshot { screen: s }.is_input());
        assert!(!SessionRequest::Ping.is_input());
        assert!(!SessionRequest::LeftMouseUp.is_input());
    }

    #[test]
    fn wire_accepts_bare_requests_and_envelopes() {
        // Older clients send the bare request.
        let (request, deadline) = decode_request(br#""Ping""#).unwrap();
        assert!(matches!(request, SessionRequest::Ping) && deadline.is_none());
        let bare = serde_json::to_vec(&SessionRequest::KeySequence {
            keys: "ctrl+c".into(),
            repeat: None,
        })
        .unwrap();
        let (request, deadline) = decode_request(&bare).unwrap();
        assert!(matches!(request, SessionRequest::KeySequence { .. }) && deadline.is_none());
        let envelope = serde_json::to_vec(&serde_json::json!({
            "request": SessionRequest::LeftMouseDown,
            "deadline_mono_ns": 42u64,
        }))
        .unwrap();
        let (request, deadline) = decode_request(&envelope).unwrap();
        assert!(matches!(request, SessionRequest::LeftMouseDown));
        assert_eq!(deadline, Some(42));
        assert!(decode_request(b"{\"nonsense\":1}").is_err());
    }

    #[tokio::test]
    async fn fully_closed_peer_is_detected() {
        let (server, client) = UnixStream::pair().unwrap();
        assert!(!peer_closed(&server));
        drop(client);
        assert!(peer_closed(&server));
    }

    #[tokio::test]
    async fn half_closed_peer_still_waits_for_the_reply() {
        // `exchange` shuts down writing after the request and then reads the
        // reply: that client must still be served.
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(b"\"Ping\"").await.unwrap();
        client.shutdown().await.unwrap();
        let (request, _) = read_request(&mut server).await.unwrap();
        assert!(matches!(request, SessionRequest::Ping));
        assert!(!peer_closed(&server));
        write_response(&mut server, respond_ok(serde_json::json!({"ok": true})))
            .await
            .unwrap();
        drop(server);
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert!(!reply.is_empty());
    }

    #[tokio::test]
    async fn exchange_sends_a_deadline_the_daemon_decodes() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let reader = tokio::spawn(async move {
            let parts = read_request(&mut server).await.unwrap();
            write_response(&mut server, respond_ok(serde_json::json!(true)))
                .await
                .unwrap();
            parts
        });
        let reply: Value = exchange(client, &SessionRequest::LeftMouseDown, Some(7))
            .await
            .unwrap();
        assert_eq!(reply, Value::Bool(true));
        let (request, deadline) = reader.await.unwrap();
        assert!(matches!(request, SessionRequest::LeftMouseDown));
        assert_eq!(deadline, Some(7));
    }

    // ---- reaping ----

    #[test]
    fn proc_stat_state_survives_odd_comm_names() {
        assert_eq!(proc_stat_state("42 (sleep) S 1 42 42"), Some('S'));
        assert_eq!(proc_stat_state("42 (a) b) (c) Z 1 42 42"), Some('Z'));
        assert_eq!(proc_stat_state("garbage"), None);
    }

    #[test]
    fn daemon_cmdline_is_matched_by_argument() {
        assert!(is_daemon_cmdline(
            b"/usr/bin/gnome-portal-bridge\0serve-session\0--socket\0/run/s.sock\0"
        ));
        assert!(!is_daemon_cmdline(b"vim\0serve-session.txt\0"));
    }

    static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spawn(command: &mut std::process::Command) -> std::process::Child {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        command.spawn().unwrap()
    }

    /// A test child pinned like the real daemon (pidfd when available).
    fn pinned(child: &std::process::Child) -> DaemonProcess {
        let pid = child.id() as i32;
        // SAFETY: as in `DaemonProcess::open`, minus the cmdline check.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        DaemonProcess {
            pid,
            pidfd: (fd >= 0).then(|| unsafe { std::os::fd::FromRawFd::from_raw_fd(fd as i32) }),
        }
    }

    #[tokio::test]
    async fn a_silent_daemon_gets_sigterm_at_once() {
        let mut child = spawn(std::process::Command::new("sleep").arg("30"));
        let daemon = pinned(&child);
        let started = Instant::now();
        assert_eq!(reap_daemon(&daemon, false).await, "sigterm");
        assert!(started.elapsed() < Duration::from_secs(1));
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn a_stopped_daemon_is_killed_within_bounds() {
        // The real daemon handles SIGTERM itself, so while stopped the signal
        // stays pending; model that with SIGTERM ignored.
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        // SAFETY: async-signal-safe call between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                Ok(())
            });
        }
        let mut child = spawn(&mut command);
        let daemon = pinned(&child);
        // SAFETY: signal to our own child.
        unsafe { libc::kill(daemon.pid, libc::SIGSTOP) };
        let started = Instant::now();
        assert_eq!(reap_daemon(&daemon, false).await, "sigkill");
        let took = started.elapsed();
        assert!(
            took >= TERM_GRACE && took < TERM_GRACE + Duration::from_secs(1),
            "{took:?}"
        );
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn an_answered_daemon_that_hangs_in_teardown_is_killed_after_the_grace() {
        let mut child = spawn(std::process::Command::new("sleep").arg("30"));
        let daemon = pinned(&child);
        let started = Instant::now();
        assert_eq!(reap_daemon(&daemon, true).await, "sigkill");
        let took = started.elapsed();
        assert!(
            took >= SHUTDOWN_GRACE
                && took < SHUTDOWN_GRACE + KILL_WAIT + Duration::from_millis(500),
            "{took:?}"
        );
        // Worst case of session-end stays inside the JS exit hook's 5 s.
        assert!(
            SHUTDOWN_REQUEST_TIMEOUT + SHUTDOWN_GRACE + KILL_WAIT <= Duration::from_millis(4500)
        );
        assert!(SHUTDOWN_REQUEST_TIMEOUT + TERM_GRACE + KILL_WAIT <= Duration::from_millis(4500));
        assert!(TEARDOWN_LIMIT < SHUTDOWN_GRACE);
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn a_daemon_that_exits_after_answering_is_not_signalled() {
        let mut child = spawn(std::process::Command::new("sleep").arg("0.2"));
        let daemon = pinned(&child);
        assert_eq!(reap_daemon(&daemon, true).await, "exited");
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn an_exited_unreaped_daemon_counts_as_gone() {
        let mut child = spawn(std::process::Command::new("true").arg(""));
        let daemon = pinned(&child);
        // Not waited for: a zombie until `child.wait()`.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!daemon.alive());
        assert!(!pid_alive(daemon.pid));
        child.wait().unwrap();
    }
}
