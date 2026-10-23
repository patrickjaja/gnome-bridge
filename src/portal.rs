//! The portal + PipeWire engine.
//!
//! A `LivePortalSession` holds ONE combined XDG RemoteDesktop + ScreenCast
//! portal session (via `lamco-portal`) for the whole tool-use lock. Input goes
//! through the RemoteDesktop `notify_*` calls; screenshots grab one PipeWire
//! frame per capture through `lamco-pipewire`.
//!
//! Coordinate model: the JS sends GLOBAL coordinates in Mutter's layout space
//! (logical pixels in the default logical layout mode). The portal's
//! `NotifyPointerMotionAbsolute` takes a point in the target stream's VIDEO
//! pixels; Mutter maps it back as `global = stream.position + point / scale`.
//! The portal's stream `position`/`size` are layout units, while the PipeWire
//! video is `round(size * scale)` pixels, so a global point is mapped as
//! `(global - screen.origin) * video_size / stream.size`, using the negotiated
//! video size of the stream (it is created for input too if no screenshot did
//! so yet). Without a video size the factor falls back to `Screen.scale` (the
//! Mutter scale in logical layout mode, 1.0 in physical layout mode).
//!
//! Stream selection: a screen maps to the stream at its origin. There is no
//! silent "first stream" fallback: a monitor that was not shared in the
//! consent dialog is an error naming the shared ones, except for a single-head
//! setup with exactly one stream (where a stale origin is harmless).
//!
//! Capture: frames are drained continuously into a latest-frame slot per
//! stream (`frames.rs`), so a screenshot is never older than the last frame
//! PipeWire delivered.
//!
//! Session persistence: with RemoteDesktop portal v2 (GNOME 46+) the portal
//! returns a restore token which we persist under
//! `$XDG_STATE_HOME/gnome-portal-bridge/`, so subsequent sessions reuse the
//! grant with no consent dialog. Persistence is requested through
//! RemoteDesktop.SelectDevices ONLY - see the session-setup section below for
//! why. On RemoteDesktop v1 (GNOME <= 45) there is no persistence, so a dialog
//! appears once per session (accepted by design).
//!
//! Stopped by the user: GNOME's screen-sharing indicator can stop the session
//! at any time. Mutter then stops its RemoteDesktop session and the portal
//! emits `org.freedesktop.portal.Session::Closed` (only when the session is
//! closed from the other side - our own `Close` emits nothing). Every session
//! subscribes to it right after `CreateSession` (see [`ClosedWatch`]); the
//! daemon turns it into exit 5 (see `stopped`).

use std::collections::HashSet;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{
    DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType as SourceTypeFlag,
};
use lamco_pipewire::{PipeWireThreadCommand, PixelFormat, StreamConfig, VideoFrame};
use lamco_portal::{PortalConfig, PortalManager, PortalSessionHandle, SourceType};

use crate::capture;
use crate::error_kind::{ErrorKind, kinded};
use crate::frames::FrameSource;
use crate::input::keymap::{self, char_to_keysym, keysym_for_name, parse_spec};
use crate::input::{
    Button, ButtonStateResult, DragActionResult, KeyboardActionResult, PointerActionResult,
    TypeActionResult,
};
use crate::output::{Rect, Screen, ScreenSource, ScreenshotCapture, ScreenshotResult};
use crate::screens;
use crate::token_store::TokenStore;

/// A single portal ScreenCast stream (one per monitor), as reported by the
/// session. `position`/`size` are in the compositor's global LAYOUT space
/// (logical pixels in logical layout mode), not video pixels.
#[derive(Debug, Clone)]
pub struct Stream {
    pub node_id: u32,
    pub position: (i32, i32),
    pub size: (u32, u32),
    pub source_type: SourceType,
}

/// The number of interpolated steps a drag is broken into (matches the kwin
/// bridge's ~20-step drag).
const DRAG_STEPS: u32 = 20;
const DRAG_STEP_DELAY_MS: u64 = 8;
/// How long a fresh stream may take to deliver its first frame (screenshot).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// A capture collects new frames until none arrives for `SETTLE_QUIET` (or
/// `SETTLE_DEADLINE` passed) and uses the last. Mutter sends frames on damage
/// or cursor motion only, so on a static screen none comes and the stored
/// frame (already current) is used after the deadline.
const SETTLE_QUIET: Duration = Duration::from_millis(70);
const SETTLE_DEADLINE: Duration = Duration::from_millis(300);
/// After input, the first capture nudges the pointer (1 px and back) and
/// settles again for at most this long; see `flush_after_input`.
const FLUSH_DEADLINE: Duration = Duration::from_millis(250);
/// How long pointer input waits for a fresh stream's first frame to learn the
/// video size before falling back to `Screen.scale`.
const INPUT_FIRST_FRAME_WAIT: Duration = Duration::from_secs(2);
/// Budget for the Mutter query on the stream-mismatch (error) path.
const MUTTER_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Budget for PipeWire teardown at session shutdown. Part of the daemon's
/// teardown budget, which `session-end` waits for before it signals.
const PIPEWIRE_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// A live combined RemoteDesktop + ScreenCast session that stays open for the
/// whole tool-use lock.
pub struct LivePortalSession {
    manager: PortalManager,
    session: PortalSessionHandle,
    streams: Vec<Stream>,
    pipewire: Option<PipeWire>,
    held_buttons: HashSet<i32>,
    /// Where pointer input last went; `None` until the session placed the
    /// pointer (then the bridge does not know where it is and never nudges).
    pointer_target: Option<PointerTarget>,
    /// Pointer input (motion or button) since the last capture of the
    /// pointer's screen; that capture flushes a repaint Mutter may still be
    /// holding back (see `flush_after_input`). Keyboard input does not arm it.
    pointer_input_pending: bool,
    /// Why the session can no longer capture (its PipeWire connection could
    /// not be rebuilt); the daemon ends itself after reporting it.
    dead: Option<String>,
    /// Fires when the portal closes the session from its side.
    closed: ClosedWatch,
}

/// The portal's `Session::Closed` signal for one session, received by a
/// background task on the connection that created the session (the portal
/// sends the signal to that connection only). Without a subscription (the
/// AddMatch failed) it never fires, which is the behavior from before.
pub struct ClosedWatch {
    rx: Option<tokio::sync::watch::Receiver<bool>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ClosedWatch {
    /// Subscribe to `Closed` on `session`. Must run before `Start`, so a
    /// session the user stops right away is not missed.
    async fn subscribe(
        remote_desktop: &RemoteDesktop,
        session: &ashpd::desktop::Session<RemoteDesktop>,
    ) -> Self {
        match closed_signal_stream(remote_desktop, session).await {
            Ok(mut stream) => {
                let (tx, rx) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    use std::pin::Pin;
                    use zbus::export::futures_core::Stream as _;
                    let next = std::future::poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await;
                    if next.is_some() {
                        tx.send_replace(true);
                    }
                    // Keep `tx` (and so `rx`'s value) until the receiver is
                    // gone; a dropped sender would make `wait` return early.
                    tx.closed().await;
                });
                Self {
                    rx: Some(rx),
                    task: Some(task),
                }
            }
            Err(error) => {
                eprintln!(
                    "gnome-portal-bridge: cannot watch the portal session's Closed signal \
                     ({error:#}); a stop from the screen-sharing indicator will not be detected"
                );
                Self {
                    rx: None,
                    task: None,
                }
            }
        }
    }

    /// A handle the daemon loop can wait on.
    pub fn signal(&self) -> ClosedSignal {
        ClosedSignal(self.rx.clone())
    }
}

impl Drop for ClosedWatch {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A cloneable view of a [`ClosedWatch`].
#[derive(Clone)]
pub struct ClosedSignal(Option<tokio::sync::watch::Receiver<bool>>);

impl ClosedSignal {
    /// A signal that never fires (tests, unsubscribed sessions).
    #[cfg(test)]
    pub fn never() -> Self {
        Self(None)
    }

    /// A signal driven by hand (tests).
    #[cfg(test)]
    pub fn manual() -> (tokio::sync::watch::Sender<bool>, Self) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (tx, Self(Some(rx)))
    }

    /// Whether the portal closed the session.
    pub fn is_closed(&self) -> bool {
        self.0.as_ref().is_some_and(|rx| *rx.borrow())
    }

    /// Resolve once the portal closed the session; never without a
    /// subscription.
    pub async fn wait(&self) {
        match self.0.clone() {
            Some(mut rx) => {
                if rx.wait_for(|closed| *closed).await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// The `Closed` signal stream of a portal session, on the connection that
/// created it.
async fn closed_signal_stream(
    remote_desktop: &RemoteDesktop,
    session: &ashpd::desktop::Session<RemoteDesktop>,
) -> Result<zbus::proxy::SignalStream<'static>> {
    // ashpd keeps the session's object path private; its Serialize writes it.
    let path = match serde_json::to_value(session).context("cannot read the session path")? {
        serde_json::Value::String(path) => path,
        other => bail!("unexpected session path encoding: {other}"),
    };
    let proxy = zbus::Proxy::new_owned(
        remote_desktop.connection().clone(),
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Session",
    )
    .await
    .context("cannot create a Session proxy")?;
    proxy
        .receive_signal("Closed")
        .await
        .context("cannot subscribe to Session::Closed")
}

/// The screen the pointer was last sent to, and the flush nudge's first
/// step there: toward the screen's middle, so a pointer at a screen edge is
/// not clamped on the way out and shifted on the way back.
struct PointerTarget {
    screen_id: String,
    nudge_dx: f64,
}

struct PipeWire {
    frames: FrameSource,
    active_streams: HashSet<u32>,
}

impl LivePortalSession {
    /// Open a new portal session, reusing a persisted restore token if present.
    pub async fn open() -> Result<Self> {
        let (manager, (session, closed), _restore_token) = start_session().await?;
        let streams = session
            .streams()
            .iter()
            .map(|s| Stream {
                node_id: s.node_id,
                position: s.position,
                size: s.size,
                source_type: s.source_type,
            })
            .collect();
        Ok(Self {
            manager,
            session,
            streams,
            pipewire: None,
            held_buttons: HashSet::new(),
            pointer_target: None,
            pointer_input_pending: false,
            dead: None,
            closed,
        })
    }

    /// The portal's `Session::Closed` for this session (the user stopped
    /// sharing from GNOME's indicator, or the compositor ended it).
    pub fn closed_signal(&self) -> ClosedSignal {
        self.closed.signal()
    }

    /// Screens derived from the portal streams, used as a fallback when the
    /// Mutter DisplayConfig API is unavailable (and by `screens` to tell which
    /// monitors are shared). Each monitor stream becomes a `Screen` in global
    /// layout coordinates; the first is treated as primary. `scale` is the
    /// negotiated video width over the stream's layout width once a frame was
    /// seen, else 1.0.
    pub fn screens_from_streams(&self) -> Vec<Screen> {
        let single = self.streams.len() == 1;
        self.streams
            .iter()
            .enumerate()
            .filter(|(_, s)| s.source_type == SourceType::Monitor || single)
            .map(|(idx, s)| {
                let is_primary = idx == 0 || single;
                let scale = match self.latest_video_size(s.node_id) {
                    Some((video_w, _)) if s.size.0 > 0 => f64::from(video_w) / f64::from(s.size.0),
                    _ => 1.0,
                };
                Screen {
                    id: format!("stream-{}", s.node_id),
                    name: format!("stream-{}", s.node_id),
                    geometry: Rect {
                        x: s.position.0,
                        y: s.position.1,
                        width: s.size.0 as i32,
                        height: s.size.1 as i32,
                    },
                    scale: Some(scale),
                    refresh_millihz: None,
                    is_active: is_primary,
                    is_primary,
                    is_shared: None,
                    source: ScreenSource::Stream,
                }
            })
            .collect()
    }

    /// Cleanly tear down: release any held buttons, stop PipeWire, close portal.
    pub async fn shutdown(mut self) -> Result<()> {
        for button in self.held_buttons.clone() {
            self.manager
                .remote_desktop()
                .notify_pointer_button(self.session.ashpd_session(), button, false)
                .await
                .ok();
        }
        if let Some(pw) = self.pipewire.take() {
            pw.frames.shutdown(PIPEWIRE_SHUTDOWN_GRACE).await;
        }
        self.manager.cleanup().await.ok();
        drop(self.session);
        Ok(())
    }

    /// The stream that captures `screen`: the one at the screen's origin.
    /// Only on a mismatch is Mutter asked for the monitor list - to allow the
    /// single-head case and to name the shared monitors in the error.
    async fn resolve_stream(&self, screen: &Screen) -> Result<Stream> {
        if let Some(stream) = stream_at_origin(&self.streams, screen) {
            return Ok(stream.clone());
        }
        let mutter = mutter_screens().await;
        select_stream(&self.streams, screen, mutter.as_deref()).cloned()
    }

    /// The negotiated video size of a stream, from its latest frame.
    fn latest_video_size(&self, node_id: u32) -> Option<(u32, u32)> {
        let (frame, _) = self.pipewire.as_ref()?.frames.slot(node_id)?.latest()?;
        (frame.width > 0 && frame.height > 0).then_some((frame.width, frame.height))
    }

    /// The video size of `stream`, creating its PipeWire consumer if no
    /// capture did yet. That only connects to the session's existing
    /// PipeWire remote; it makes no portal call, so it cannot show a dialog.
    async fn video_size_for_input(&mut self, stream: &Stream) -> Option<(u32, u32)> {
        if let Some(size) = self.latest_video_size(stream.node_id) {
            return Some(size);
        }
        if let Err(error) = self
            .ensure_stream(stream.node_id, stream.size.0, stream.size.1)
            .await
        {
            eprintln!(
                "gnome-portal-bridge: no PipeWire stream for input scaling ({error:#}); \
                 using the screen scale"
            );
            return None;
        }
        let slot = self.pipewire.as_ref()?.frames.slot(stream.node_id)?;
        let (frame, _) = match slot.latest() {
            Some(found) => found,
            None => slot.wait_newer(0, INPUT_FIRST_FRAME_WAIT).await?,
        };
        (frame.width > 0 && frame.height > 0).then_some((frame.width, frame.height))
    }

    /// Put the pointer on global point (x, y). Every pointer action (move,
    /// click, scroll, each drag step) goes through here, so the outer-band
    /// split (see `plan_pointer_motion`) applies to all of them.
    async fn motion_absolute(&mut self, screen: &Screen, x: i32, y: i32) -> Result<()> {
        let stream = self.resolve_stream(screen).await?;
        // A Mutter screen's `scale` is exactly the factor Mutter divides
        // absolute positions by (its transform_position uses the logical
        // monitor scale), so input needs no capture stream. Only a
        // stream-derived screen has to learn its factor from the video.
        let video = match screen.source {
            ScreenSource::Mutter => None,
            ScreenSource::Stream => self.video_size_for_input(&stream).await,
        };
        let plan = plan_pointer_motion(screen, &stream, video, x, y);
        let (ax, ay) = plan.absolute;
        self.pointer_target = Some(PointerTarget {
            screen_id: screen.id.clone(),
            nudge_dx: nudge_dx_toward_middle(screen, x),
        });
        self.pointer_input_pending = true;
        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(self.session.ashpd_session(), stream.node_id, ax, ay)
            .await
            .context("failed to send absolute pointer motion through the portal")?;
        if let Some((dx, dy)) = plan.relative {
            self.manager
                .remote_desktop()
                .notify_pointer_motion(self.session.ashpd_session(), dx, dy)
                .await
                .context("failed to send relative pointer motion through the portal")?;
        }
        Ok(())
    }

    pub async fn move_pointer(
        &mut self,
        screen: &Screen,
        x: i32,
        y: i32,
    ) -> Result<PointerActionResult> {
        self.motion_absolute(screen, x, y).await?;
        Ok(PointerActionResult {
            action: "mouse-move".to_owned(),
            x,
            y,
        })
    }

    pub async fn click(
        &mut self,
        screen: &Screen,
        x: i32,
        y: i32,
        button: Button,
        count: u32,
        modifiers: &[String],
    ) -> Result<PointerActionResult> {
        let code = button.evdev_code();
        if self.held_buttons.contains(&code) {
            bail!("button {code} is currently held by the session");
        }
        self.motion_absolute(screen, x, y).await?;

        let mod_keysyms = resolve_modifiers(modifiers)?;
        self.press_keysyms(&mod_keysyms).await?;

        let click_result = async {
            for _ in 0..count.max(1) {
                self.manager
                    .remote_desktop()
                    .notify_pointer_button(self.session.ashpd_session(), code, true)
                    .await
                    .context("failed to send pointer press through the portal")?;
                self.manager
                    .remote_desktop()
                    .notify_pointer_button(self.session.ashpd_session(), code, false)
                    .await
                    .context("failed to send pointer release through the portal")?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        let release_result = self.release_keysyms(&mod_keysyms).await;
        click_result?;
        release_result?;

        Ok(PointerActionResult {
            action: "click".to_owned(),
            x,
            y,
        })
    }

    pub async fn scroll(
        &mut self,
        screen: &Screen,
        x: i32,
        y: i32,
        dx: f64,
        dy: f64,
    ) -> Result<PointerActionResult> {
        self.motion_absolute(screen, x, y).await?;
        self.manager
            .remote_desktop()
            .notify_pointer_axis(self.session.ashpd_session(), dx, dy)
            .await
            .context("failed to send scroll event through the portal")?;
        Ok(PointerActionResult {
            action: "scroll".to_owned(),
            x,
            y,
        })
    }

    pub async fn drag(
        &mut self,
        from_screen: &Screen,
        from_x: i32,
        from_y: i32,
        to_screen: &Screen,
        to_x: i32,
        to_y: i32,
    ) -> Result<DragActionResult> {
        let code = Button::Left.evdev_code();
        if self.held_buttons.contains(&code) {
            bail!("left mouse button is currently held by the session");
        }

        self.motion_absolute(from_screen, from_x, from_y).await?;
        self.manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), code, true)
            .await
            .context("failed to press left button for drag")?;

        let drag_result = async {
            for step in 1..=DRAG_STEPS {
                let t = f64::from(step) / f64::from(DRAG_STEPS);
                let ix = from_x + ((f64::from(to_x - from_x) * t).round() as i32);
                let iy = from_y + ((f64::from(to_y - from_y) * t).round() as i32);
                // Interpolated points are addressed against the destination
                // screen; for same-screen drags this is exact, for cross-screen
                // the endpoints still land correctly.
                self.motion_absolute(to_screen, ix, iy).await?;
                tokio::time::sleep(Duration::from_millis(DRAG_STEP_DELAY_MS)).await;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;

        let release_result = self
            .manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), code, false)
            .await
            .context("failed to release left button after drag");

        drag_result?;
        release_result?;

        Ok(DragActionResult {
            action: "drag".to_owned(),
            from_x,
            from_y,
            to_x,
            to_y,
        })
    }

    pub async fn left_mouse_down(&mut self) -> Result<ButtonStateResult> {
        let code = Button::Left.evdev_code();
        if self.held_buttons.contains(&code) {
            bail!("left mouse button is already held");
        }
        self.pointer_input_pending = true;
        self.manager
            .remote_desktop()
            .notify_pointer_button(self.session.ashpd_session(), code, true)
            .await
            .context("failed to press left mouse button through the portal")?;
        self.held_buttons.insert(code);
        Ok(ButtonStateResult {
            action: "left-mouse-down".to_owned(),
            button: "left".to_owned(),
            is_held: true,
        })
    }

    pub async fn left_mouse_up(&mut self) -> Result<ButtonStateResult> {
        let code = Button::Left.evdev_code();
        let was_held = self.held_buttons.remove(&code);
        if was_held {
            self.pointer_input_pending = true;
            self.manager
                .remote_desktop()
                .notify_pointer_button(self.session.ashpd_session(), code, false)
                .await
                .context("failed to release left mouse button through the portal")?;
        }
        Ok(ButtonStateResult {
            action: "left-mouse-up".to_owned(),
            button: "left".to_owned(),
            is_held: false,
        })
    }

    async fn press_keysyms(&self, keysyms: &[u32]) -> Result<()> {
        for &keysym in keysyms {
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym as i32, true)
                .await
                .with_context(|| format!("failed to press keysym 0x{keysym:08X}"))?;
        }
        Ok(())
    }

    async fn release_keysyms(&self, keysyms: &[u32]) -> Result<()> {
        for &keysym in keysyms.iter().rev() {
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym as i32, false)
                .await
                .with_context(|| format!("failed to release keysym 0x{keysym:08X}"))?;
        }
        Ok(())
    }

    pub async fn key_sequence(
        &self,
        keys: &str,
        repeat: Option<u32>,
    ) -> Result<KeyboardActionResult> {
        let keysyms = resolve_key_spec(keys)?;
        for _ in 0..repeat.unwrap_or(1).max(1) {
            self.press_keysyms(&keysyms).await?;
            self.release_keysyms(&keysyms).await?;
        }
        Ok(KeyboardActionResult {
            action: "key-sequence".to_owned(),
            keys: keys.to_owned(),
            repeat,
            duration_ms: None,
        })
    }

    pub async fn hold_key(
        &self,
        keys: &[String],
        duration_ms: u64,
    ) -> Result<KeyboardActionResult> {
        // Each `--key` is its own spec; flatten to keysyms preserving order.
        let mut keysyms = Vec::new();
        for spec in keys {
            keysyms.extend(resolve_key_spec(spec)?);
        }
        self.press_keysyms(&keysyms).await?;
        tokio::time::sleep(Duration::from_millis(duration_ms)).await;
        self.release_keysyms(&keysyms).await?;
        Ok(KeyboardActionResult {
            action: "hold-key".to_owned(),
            keys: keys.join("+"),
            repeat: None,
            duration_ms: Some(duration_ms),
        })
    }

    pub async fn type_text(&self, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
        for ch in text.chars() {
            let keysym = char_to_keysym(ch)?;
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym as i32, true)
                .await
                .with_context(|| format!("failed to press keysym for {ch:?}"))?;
            self.manager
                .remote_desktop()
                .notify_keyboard_keysym(self.session.ashpd_session(), keysym as i32, false)
                .await
                .with_context(|| format!("failed to release keysym for {ch:?}"))?;
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
        Ok(TypeActionResult {
            action: "type".to_owned(),
            text: text.to_owned(),
            char_count: text.chars().count(),
        })
    }

    /// Capture one frame of the stream for `screen` and return an executor-style
    /// screenshot (downscaled JPEG + geometry metadata).
    pub async fn screenshot(&mut self, screen: &Screen) -> Result<ScreenshotResult> {
        let frame = self.capture_frame(screen).await?;
        capture::screenshot_result_from_frame(screen, &frame)
    }

    /// Capture one frame and crop a logical region for a zoom image.
    pub async fn zoom(
        &mut self,
        screen: &Screen,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<ScreenshotCapture> {
        let frame = self.capture_frame(screen).await?;
        capture::zoom_result_from_frame(screen, &frame, x, y, w, h)
    }

    /// Whether the session lost its capture path for good (see `dead`).
    pub fn is_dead(&self) -> bool {
        self.dead.is_some()
    }

    /// The latest PipeWire frame of the screen's stream: on a fresh stream the
    /// first frame (bounded wait), else the last frame once the stream settles
    /// (see `SETTLE_QUIET`), else the stored latest frame. The first capture
    /// of the pointer's screen after pointer input also flushes a repaint
    /// Mutter may be holding back.
    async fn capture_frame(&mut self, screen: &Screen) -> Result<Arc<VideoFrame>> {
        let stream = self.resolve_stream(screen).await?;
        let node_id = stream.node_id;
        self.ensure_stream(node_id, stream.size.0, stream.size.1)
            .await?;
        let pw = self.pipewire.as_ref().expect("pipewire initialized");
        let slot = pw
            .frames
            .slot(node_id)
            .context("PipeWire stream has no frame slot")?;
        // Only a capture of the screen the pointer is on can be flushed by
        // nudging it; a capture of another screen leaves the flag armed.
        let nudge = flush_nudge(
            self.pointer_target.as_ref(),
            self.pointer_input_pending,
            &screen.id,
        );
        if nudge.is_some() {
            self.pointer_input_pending = false;
        }
        let Some((stored, seq)) = slot.latest() else {
            // A fresh stream's first frame is rendered from the current
            // stage, so it needs no flush.
            return match slot.wait_newer(0, FIRST_FRAME_TIMEOUT).await {
                Some((frame, _)) => Ok(frame),
                None => bail!(
                    "timed out waiting for PipeWire frame on stream {node_id} ({})",
                    pw.frames.state_summary()
                ),
            };
        };
        let (frame, seq) = slot
            .settle(seq, SETTLE_QUIET, SETTLE_DEADLINE)
            .await
            .unwrap_or((stored, seq));
        let Some(nudge_dx) = nudge.filter(|_| self.held_buttons.is_empty()) else {
            return Ok(frame);
        };
        Ok(self
            .flush_after_input(&slot, seq, nudge_dx)
            .await
            .map_or(frame, |(fresh, _)| fresh))
    }

    /// Make Mutter record the repaint an input action caused, and wait for it.
    ///
    /// Mutter records a monitor stream at most once per refresh interval. A
    /// repaint that lands inside that interval (typically: the click's cursor
    /// frame was recorded, then the application repainted a few ms later) is
    /// skipped and left to a "follow-up" redraw of the monitor's view. With an
    /// embedded cursor nothing else records the stream, so when that follow-up
    /// does not reach the view (measured on GNOME 51 with two monitors: it
    /// repaints the other view), the repaint is never sent and every later
    /// screenshot shows the pre-click screen until something else changes on
    /// that monitor. Moving the pointer 1 px and back damages the view under
    /// the cursor; the next paint records a frame that includes the pending
    /// damage. Done once per batch of POINTER input and only on a capture of
    /// the screen the pointer was sent to: keyboard input does not arm it (a
    /// pointer move over an open menu or popup list would move its selection
    /// to the item under the pointer), nor does it run while a button is held
    /// (it would drag) or before the session placed the pointer at all.
    async fn flush_after_input(
        &self,
        slot: &crate::frames::LatestSlot<Arc<VideoFrame>>,
        seq: u64,
        nudge_dx: f64,
    ) -> Option<(Arc<VideoFrame>, u64)> {
        let remote = self.manager.remote_desktop();
        let session = self.session.ashpd_session();
        if let Err(error) = remote.notify_pointer_motion(session, nudge_dx, 0.0).await {
            eprintln!("gnome-portal-bridge: capture flush motion failed: {error:#}");
            return None;
        }
        // The way back must not be lost: one retry, then log the 1 px offset.
        let mut back = remote.notify_pointer_motion(session, -nudge_dx, 0.0).await;
        if back.is_err() {
            back = remote.notify_pointer_motion(session, -nudge_dx, 0.0).await;
        }
        if let Err(error) = back {
            eprintln!(
                "gnome-portal-bridge: capture flush could not move the pointer back \
                 (left 1 px off): {error:#}"
            );
        }
        slot.settle(seq, SETTLE_QUIET, FLUSH_DEADLINE).await
    }

    async fn ensure_stream(&mut self, node_id: u32, width: u32, height: u32) -> Result<()> {
        if let Some(reason) = &self.dead {
            return Err(kinded(ErrorKind::NoSession, reason.clone()));
        }
        // A source whose PipeWire thread died never delivers again. Its fd
        // cannot be reused (a dup shares the old connection, whose objects
        // the server still holds, and a restarted PipeWire left it dead), so
        // the portal is asked for a new PipeWire remote on the live session.
        let mut reconnect = false;
        if let Some(pw) = self.pipewire.take_if(|pw| !pw.frames.is_alive()) {
            eprintln!(
                "gnome-portal-bridge: PipeWire thread exited ({}); reconnecting",
                pw.frames.state_summary()
            );
            pw.frames.shutdown(Duration::ZERO).await;
            reconnect = true;
        }
        if self.pipewire.is_none() {
            let fd = if reconnect {
                match self.open_pipewire_remote().await {
                    Ok(fd) => fd,
                    Err(error) => {
                        // Contract v1 exit 4: the message names "no active
                        // portal session" for consumers below contract 1.
                        let reason = format!(
                            "no active portal session: the screen capture connection was lost \
                             and the portal session could not reopen it ({error:#}); the session \
                             daemon ends now - run session-start again"
                        );
                        self.dead = Some(reason.clone());
                        return Err(kinded(ErrorKind::NoSession, reason));
                    }
                }
            } else {
                dup_fd(self.session.pipewire_fd())?
            };
            self.pipewire = Some(PipeWire {
                frames: FrameSource::start(fd)?,
                active_streams: HashSet::new(),
            });
        }
        let pw = self.pipewire.as_mut().expect("pipewire just initialized");
        if pw.active_streams.contains(&node_id) {
            return Ok(());
        }
        create_stream(&pw.frames, node_id, width, height).await?;
        pw.active_streams.insert(node_id);
        Ok(())
    }

    /// A fresh PipeWire remote for this portal session (`OpenPipeWireRemote`
    /// may be called repeatedly on a started session).
    async fn open_pipewire_remote(&self) -> Result<OwnedFd> {
        let screencast = Screencast::new()
            .await
            .context("failed to open the ScreenCast portal proxy")?;
        tokio::time::timeout(
            MUTTER_QUERY_TIMEOUT,
            screencast.open_pipe_wire_remote(self.session.ashpd_session(), Default::default()),
        )
        .await
        .context("ScreenCast.OpenPipeWireRemote timed out")?
        .context("ScreenCast.OpenPipeWireRemote failed")
    }
}

// ---- stream selection + coordinate mapping (pure, unit-tested) ----

/// The flush nudge for a capture of `screen_id`: only after pointer input,
/// only on the screen the pointer was sent to, never before the session
/// placed the pointer (see `flush_after_input`).
fn flush_nudge(target: Option<&PointerTarget>, pending: bool, screen_id: &str) -> Option<f64> {
    target
        .filter(|target| pending && target.screen_id == screen_id)
        .map(|target| target.nudge_dx)
}

/// The flush nudge's first step: toward the middle of `screen` from global x,
/// so neither step is clamped at a screen edge (see `flush_after_input`).
fn nudge_dx_toward_middle(screen: &Screen, x: i32) -> f64 {
    if x >= screen.geometry.x + screen.geometry.width / 2 {
        -1.0
    } else {
        1.0
    }
}

fn stream_at_origin<'a>(streams: &'a [Stream], screen: &Screen) -> Option<&'a Stream> {
    streams
        .iter()
        .find(|s| s.position.0 == screen.geometry.x && s.position.1 == screen.geometry.y)
}

/// Pick the stream for `screen`: the one at its origin; else, only when there
/// is exactly one stream AND Mutter reports exactly one monitor, that stream
/// (single-head, the origins can differ by a stale layout). Anything else is
/// a monitor the user did not share, and silently driving another monitor
/// instead would click and capture the wrong screen.
fn select_stream<'a>(
    streams: &'a [Stream],
    screen: &Screen,
    mutter: Option<&[Screen]>,
) -> Result<&'a Stream> {
    if streams.is_empty() {
        bail!("portal session returned no streams");
    }
    if let Some(stream) = stream_at_origin(streams, screen) {
        return Ok(stream);
    }
    if streams.len() == 1 && mutter.is_some_and(|m| m.len() == 1) {
        return Ok(&streams[0]);
    }
    let shared = streams
        .iter()
        .map(|s| {
            let name = mutter
                .and_then(|m| {
                    m.iter()
                        .find(|ms| (ms.geometry.x, ms.geometry.y) == s.position)
                })
                .map_or_else(|| format!("stream-{}", s.node_id), |ms| ms.id.clone());
            format!(
                "{name} at {},{} {}x{}",
                s.position.0, s.position.1, s.size.0, s.size.1
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    // Contract v1 wording (exit 3). The remedy is the consumer's to add: the
    // user of claude-desktop-extra resets the grant through the launcher, not
    // through this binary.
    let g = &screen.geometry;
    Err(kinded(
        ErrorKind::NotShared,
        format!(
            "monitor {} ({},{} {}x{}) is not shared in the portal session (shared: {shared})",
            screen.id, g.x, g.y, g.width, g.height
        ),
    ))
}

/// The Mutter monitor list, bounded; `None` when Mutter is unavailable.
async fn mutter_screens() -> Option<Vec<Screen>> {
    tokio::time::timeout(MUTTER_QUERY_TIMEOUT, async {
        let connection = zbus::Connection::session().await.ok()?;
        screens::list_screens(&connection).await.ok()
    })
    .await
    .ok()
    .flatten()
}

/// Layout-unit -> video-pixel factors for a screen's stream: the negotiated
/// video size over the stream's layout size when known, else `Screen.scale`.
fn pixel_factors(screen: &Screen, stream: &Stream, video: Option<(u32, u32)>) -> (f64, f64) {
    let layout_w = if stream.size.0 > 0 {
        f64::from(stream.size.0)
    } else {
        f64::from(screen.geometry.width.max(1))
    };
    let layout_h = if stream.size.1 > 0 {
        f64::from(stream.size.1)
    } else {
        f64::from(screen.geometry.height.max(1))
    };
    match video {
        Some((video_w, video_h)) if video_w > 0 && video_h > 0 => {
            (f64::from(video_w) / layout_w, f64::from(video_h) / layout_h)
        }
        _ => {
            let scale = screen
                .scale
                .filter(|s| s.is_finite() && *s > 0.0)
                .unwrap_or(1.0);
            (scale, scale)
        }
    }
}

/// Map a global layout point onto `stream`'s video pixels (what
/// `NotifyPointerMotionAbsolute` expects).
fn stream_pixel_point(
    screen: &Screen,
    stream: &Stream,
    video: Option<(u32, u32)>,
    x: i32,
    y: i32,
) -> (f64, f64) {
    let (factor_x, factor_y) = pixel_factors(screen, stream, video);
    (
        f64::from(x - screen.geometry.x) * factor_x,
        f64::from(y - screen.geometry.y) * factor_y,
    )
}

/// How a pointer target is reached: one absolute motion, optionally followed
/// by a relative motion for the part the portal will not accept absolutely.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PointerPlan {
    /// Stream video pixels for `NotifyPointerMotionAbsolute`.
    absolute: (f64, f64),
    /// Layout (logical) units for `NotifyPointerMotion`, if needed.
    relative: Option<(f64, f64)>,
}

/// Plan the motion to global point (x, y).
///
/// xdg-desktop-portal's `check_position` (remote-desktop.c, 1.20) refuses an
/// absolute position unless `0 <= x < size.w && 0 <= y < size.h`, with `size`
/// the stream's portal `size` property - LOGICAL units - while Mutter reads
/// the same numbers as VIDEO pixels and divides by the scale. On a scaled
/// monitor the outer band beyond `size / scale` logical (e.g. x >= 1638 of
/// 2048 at scale 1.25) is therefore unreachable by absolute motion alone
/// ("Invalid position"). For such a point we move absolutely to the clamped
/// stream point `A_s = min(P, size - 1)` - Mutter puts the cursor at logical
/// `origin + A_s / factor` - and send the rest as relative motion, which
/// Mutter applies in logical units without acceleration (virtual device).
/// At scale 1 (or in physical layout mode) `P` is the logical offset itself,
/// which is always inside `size`, so this never splits.
fn plan_pointer_motion(
    screen: &Screen,
    stream: &Stream,
    video: Option<(u32, u32)>,
    x: i32,
    y: i32,
) -> PointerPlan {
    let point = stream_pixel_point(screen, stream, video, x, y);
    let (factor_x, factor_y) = pixel_factors(screen, stream, video);
    let local_x = f64::from(x - screen.geometry.x);
    let local_y = f64::from(y - screen.geometry.y);
    let (size_w, size_h) = (f64::from(stream.size.0), f64::from(stream.size.1));
    // An unknown (0) size cannot be clamped against; send as is.
    if size_w < 1.0 || size_h < 1.0 || (point.0 < size_w && point.1 < size_h) {
        return PointerPlan {
            absolute: point,
            relative: None,
        };
    }
    let clamped = (point.0.min(size_w - 1.0), point.1.min(size_h - 1.0));
    PointerPlan {
        absolute: clamped,
        relative: Some((
            local_x - clamped.0 / factor_x,
            local_y - clamped.1 / factor_y,
        )),
    }
}

/// Resolve a list of modifier tokens (`ctrl`, `shift`, ...) to keysym values.
fn resolve_modifiers(modifiers: &[String]) -> Result<Vec<u32>> {
    modifiers
        .iter()
        .map(|m| keysym_for_name(m))
        .collect::<Result<Vec<_>>>()
}

/// Resolve a full key spec (`ctrl+shift+a`) to the ordered keysyms to press:
/// modifiers first, then the base key. Release order is the reverse.
fn resolve_key_spec(spec: &str) -> Result<Vec<u32>> {
    let parsed = parse_spec(spec)?;
    let mut keysyms = Vec::with_capacity(parsed.modifiers.len() + 1);
    for m in &parsed.modifiers {
        keysyms.push(keymap::keysym_for_name(m)?);
    }
    keysyms.push(keymap::keysym_for_name(&parsed.base)?);
    Ok(keysyms)
}

// ---- portal session setup ----
//
// The session is built here with ashpd directly instead of through
// `PortalManager::create_session`, because of how persistence works for a
// COMBINED RemoteDesktop + ScreenCast session (xdg-desktop-portal docs,
// ScreenCast.SelectSources): "Setting persist_mode is only allowed for screen
// cast sessions. Persistent remote desktop screen cast sessions can only be
// handled via the Remote Desktop interface." lamco-portal passes persist_mode +
// restore_token to BOTH SelectDevices and SelectSources, the portal rejects the
// SelectSources call with InvalidArgument, and the only way to get a session at
// all was to retry with persistence off - which is why no restore token was
// ever stored and a consent dialog appeared on every session
// (claude-desktop-extra issue #293). The correct flow: persist_mode and the
// saved token go to RemoteDesktop.SelectDevices ONLY (RemoteDesktop portal
// version >= 2, i.e. GNOME 46+), SelectSources carries no persistence options,
// and the new token comes back in RemoteDesktop.Start's response.
//
// `PortalManager` is still used for the input path (`remote_desktop()` holds
// the proxy the `notify_*` calls go through); only session creation is ours.

async fn start_session() -> Result<(
    PortalManager,
    (PortalSessionHandle, ClosedWatch),
    Option<String>,
)> {
    let token_store = TokenStore::new()?;
    let saved_token = token_store.load().unwrap_or(None);

    let config = default_config();
    let manager = PortalManager::new(config.clone())
        .await
        .context("failed to connect to the XDG desktop portal")?;
    let remote_desktop = RemoteDesktop::new()
        .await
        .context("failed to open the RemoteDesktop portal proxy")?;
    let persistence = if remote_desktop.version() >= 2 {
        Persistence::Requested
    } else {
        // RemoteDesktop portal v1 (GNOME <= 45): no persistence API at all; a
        // consent dialog per session is the best that backend can do.
        Persistence::Unsupported
    };

    match try_start_session(&remote_desktop, &config, saved_token.clone(), persistence).await {
        Ok((session, restore_token)) => {
            if let Some(ref token) = restore_token {
                token_store.save(token).ok();
            }
            Ok((manager, session, restore_token))
        }
        Err(error) if persistence == Persistence::Requested && is_persistence_rejection(&error) => {
            // A backend that advertises v2 but refuses to persist (or a token
            // it no longer accepts): one more try with no persistence at all
            // so the session still comes up; the dialog is unavoidable then.
            eprintln!(
                "gnome-portal-bridge: portal rejected a persistent session ({error:#}); \
                 retrying without persistence (a consent dialog will appear each session)"
            );
            let (session, restore_token) =
                try_start_session(&remote_desktop, &config, None, Persistence::Unsupported)
                    .await
                    .context("failed to create session after persistence fallback")?;
            Ok((manager, session, restore_token))
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Persistence {
    /// Ask the portal to remember the grant (RemoteDesktop portal v2+).
    Requested,
    /// Do not ask; the backend cannot persist remote desktop sessions.
    Unsupported,
}

async fn try_start_session(
    remote_desktop: &RemoteDesktop,
    config: &PortalConfig,
    restore_token: Option<String>,
    persistence: Persistence,
) -> Result<((PortalSessionHandle, ClosedWatch), Option<String>)> {
    let session = remote_desktop
        .create_session(Default::default())
        .await
        .context("failed to create the RemoteDesktop portal session")?;
    let closed = ClosedWatch::subscribe(remote_desktop, &session).await;

    let result =
        select_and_start(remote_desktop, config, &session, restore_token, persistence).await;
    match result {
        Ok((fd, streams, token)) => {
            let session_id = generate_session_id()?;
            let handle = PortalSessionHandle::new(
                session_id.clone(),
                fd.into_raw_fd(),
                streams,
                Some(session_id),
                session,
            );
            Ok(((handle, closed), token))
        }
        Err(error) => {
            // Close the half-built session so the portal does not keep stale
            // state around (a retry on the same connection would otherwise
            // fail with "Invalid state").
            session.close().await.ok();
            Err(error)
        }
    }
}

async fn select_and_start(
    remote_desktop: &RemoteDesktop,
    config: &PortalConfig,
    session: &ashpd::desktop::Session<RemoteDesktop>,
    restore_token: Option<String>,
    persistence: Persistence,
) -> Result<(OwnedFd, Vec<lamco_portal::StreamInfo>, Option<String>)> {
    let mut devices = SelectDevicesOptions::default().set_devices(config.devices);
    if persistence == Persistence::Requested {
        devices = devices
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
    }
    remote_desktop
        .select_devices(session, devices)
        .await
        .context("RemoteDesktop.SelectDevices failed")?
        .response()
        .context("RemoteDesktop.SelectDevices was refused")?;

    // No persist_mode / restore_token here, by portal contract (see above).
    let screencast = Screencast::new()
        .await
        .context("failed to open the ScreenCast portal proxy")?;
    let sources = SelectSourcesOptions::default()
        .set_cursor_mode(config.cursor_mode)
        .set_sources(config.source_type)
        .set_multiple(config.allow_multiple);
    screencast
        .select_sources(session, sources)
        .await
        .context("ScreenCast.SelectSources failed")?
        .response()
        .context("ScreenCast.SelectSources was refused")?;

    // Start: shows the consent dialog unless the restore token is accepted.
    let selected = remote_desktop
        .start(session, None, StartOptions::default())
        .await
        .context("RemoteDesktop.Start failed")?
        .response()
        .context("RemoteDesktop.Start was refused (consent dialog declined or closed?)")?;
    let restore_token = selected.restore_token().map(str::to_owned);

    let streams: Vec<lamco_portal::StreamInfo> = selected
        .streams()
        .iter()
        .map(|stream| {
            let size = stream.size().unwrap_or((0, 0));
            lamco_portal::StreamInfo {
                node_id: stream.pipe_wire_node_id(),
                position: stream.position().unwrap_or((0, 0)),
                size: (
                    size.0.max(0).try_into().unwrap_or(0),
                    size.1.max(0).try_into().unwrap_or(0),
                ),
                source_type: SourceType::Monitor,
            }
        })
        .collect();
    if streams.is_empty() {
        bail!("the portal session started with no screen streams");
    }

    let fd = screencast
        .open_pipe_wire_remote(session, Default::default())
        .await
        .context("ScreenCast.OpenPipeWireRemote failed")?;

    Ok((fd, streams, restore_token))
}

fn default_config() -> PortalConfig {
    PortalConfig {
        cursor_mode: CursorMode::Embedded,
        source_type: SourceTypeFlag::Monitor.into(),
        devices: DeviceType::Keyboard | DeviceType::Pointer,
        allow_multiple: true,
        ..Default::default()
    }
}

fn generate_session_id() -> Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis();
    Ok(format!("gnome-portal-bridge-{timestamp}"))
}

fn is_persistence_rejection(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("persist") || message.contains("InvalidArgument")
}

// ---- PipeWire helpers ----

/// How long PipeWire may take to confirm a stream creation.
const CREATE_STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask the PipeWire thread for a stream and wait for its answer WITHOUT
/// blocking a tokio worker (a blocked worker delays the daemon's SIGTERM
/// handling past `session-end`'s grace and forces a SIGKILL).
async fn create_stream(frames: &FrameSource, node_id: u32, width: u32, height: u32) -> Result<()> {
    let (response_tx, response_rx) = std_mpsc::sync_channel(1);
    let config = StreamConfig {
        name: format!("monitor-{node_id}"),
        width,
        height,
        framerate: 60,
        use_dmabuf: false,
        buffer_count: 3,
        preferred_format: Some(PixelFormat::BGRx),
        dmabuf_passthrough: false,
    };
    frames
        .send_stream_command(
            node_id,
            PipeWireThreadCommand::CreateStream {
                stream_id: node_id,
                node_id,
                config,
                response_tx,
            },
        )
        .context("failed to send PipeWire CreateStream command")?;
    let deadline = tokio::time::Instant::now() + CREATE_STREAM_TIMEOUT;
    loop {
        match response_rx.try_recv() {
            Ok(result) => return result.context("PipeWire rejected stream creation"),
            Err(std_mpsc::TryRecvError::Disconnected) => {
                bail!(
                    "no PipeWire create-stream response (the PipeWire thread exited; see daemon.log)"
                )
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "no PipeWire create-stream response within {}s; see daemon.log",
                CREATE_STREAM_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn dup_fd(raw_fd: i32) -> Result<OwnedFd> {
    let duplicated = unsafe { libc::dup(raw_fd) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to duplicate portal PipeWire file descriptor");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, x: i32, y: i32, w: i32, h: i32, scale: f64) -> Screen {
        Screen {
            id: id.to_owned(),
            name: id.to_owned(),
            geometry: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            scale: Some(scale),
            refresh_millihz: None,
            is_active: false,
            is_primary: false,
            is_shared: None,
            source: ScreenSource::Mutter,
        }
    }

    fn stream(node_id: u32, x: i32, y: i32, w: u32, h: u32) -> Stream {
        Stream {
            node_id,
            position: (x, y),
            size: (w, h),
            source_type: SourceType::Monitor,
        }
    }

    /// Mutter reports fractional scales as the float nearest to n/d.
    const SCALE_4_3: f64 = 1.333_333_373_069_763_2;

    fn edp() -> Screen {
        screen("eDP-1", 0, 240, 1920, 1200, SCALE_4_3)
    }

    fn hdmi() -> Screen {
        screen("HDMI-1", 1920, 0, 3440, 1440, 1.0)
    }

    fn assert_close(got: (f64, f64), want: (f64, f64)) {
        assert!(
            (got.0 - want.0).abs() < 0.01 && (got.1 - want.1).abs() < 0.01,
            "got {got:?}, want {want:?}"
        );
    }

    /// claude-desktop-extra #293: global (900,800) on the 1.3333-scaled eDP-1
    /// at (0,240) must be sent as stream VIDEO pixels (Mutter divides by the
    /// scale again), not as the logical offset (which landed at (675,660)).
    #[test]
    fn scaled_monitor_maps_to_video_pixels_from_negotiated_size() {
        let s = stream(40, 0, 240, 1920, 1200);
        let got = stream_pixel_point(&edp(), &s, Some((2560, 1600)), 900, 800);
        assert_close(got, (1200.0, 746.67));
        // Mutter's inverse: origin + point / scale lands on the target.
        assert_close(
            (got.0 / SCALE_4_3, 240.0 + got.1 / SCALE_4_3),
            (900.0, 800.0),
        );
    }

    #[test]
    fn scaled_monitor_falls_back_to_screen_scale_without_video_size() {
        let s = stream(40, 0, 240, 1920, 1200);
        assert_close(
            stream_pixel_point(&edp(), &s, None, 900, 800),
            (1200.0, 746.67),
        );
    }

    #[test]
    fn flush_nudge_only_after_pointer_input_on_the_pointer_screen() {
        let target = PointerTarget {
            screen_id: "HDMI-1".to_owned(),
            nudge_dx: -1.0,
        };
        assert_eq!(flush_nudge(Some(&target), true, "HDMI-1"), Some(-1.0));
        // Keyboard-only input does not arm it.
        assert_eq!(flush_nudge(Some(&target), false, "HDMI-1"), None);
        // A capture of another monitor would only pay the wait.
        assert_eq!(flush_nudge(Some(&target), true, "eDP-1"), None);
        // The pointer was never placed: its position is unknown.
        assert_eq!(flush_nudge(None, true, "HDMI-1"), None);
    }

    #[test]
    fn flush_nudge_steps_away_from_the_nearest_edge() {
        let screen = hdmi(); // x 1920..5360
        assert_eq!(nudge_dx_toward_middle(&screen, 1920), 1.0);
        assert_eq!(nudge_dx_toward_middle(&screen, 3000), 1.0);
        assert_eq!(nudge_dx_toward_middle(&screen, 3640), -1.0);
        assert_eq!(nudge_dx_toward_middle(&screen, 5359), -1.0);
    }

    #[test]
    fn scale_one_monitor_is_the_identity() {
        let s = stream(41, 1920, 0, 3440, 1440);
        assert_close(
            stream_pixel_point(&hdmi(), &s, Some((3440, 1440)), 2500, 700),
            (580.0, 700.0),
        );
        assert_close(
            stream_pixel_point(&hdmi(), &s, None, 2500, 700),
            (580.0, 700.0),
        );
    }

    /// Physical layout mode: the Mutter fixture yields physical geometry and
    /// scale 1, the portal stream is physical too, so nothing is divided or
    /// multiplied.
    #[test]
    fn physical_layout_mode_maps_without_scaling() {
        let screens = screens::screens_from_reply(&screens::tests::physical_mode_reply());
        let screen = &screens[0];
        assert_eq!(screen.scale, Some(1.0));
        let s = stream(40, 0, 0, 2560, 1600);
        for video in [Some((2560, 1600)), None] {
            assert_close(
                stream_pixel_point(screen, &s, video, 1200, 900),
                (1200.0, 900.0),
            );
        }
    }

    /// A portrait (90-degree) scale-2 monitor: layout 720x1280, the portal
    /// stream and its video are rotated the same way, so both the video ratio
    /// and the scale fallback map onto the rotated video.
    #[test]
    fn rotated_monitor_maps_with_rotated_video() {
        let portrait = screen("DP-1", 0, 0, 720, 1280, 2.0);
        let s = stream(40, 0, 0, 720, 1280);
        for video in [Some((1440, 2560)), None] {
            assert_close(
                stream_pixel_point(&portrait, &s, video, 700, 1200),
                (1400.0, 2400.0),
            );
        }
    }

    /// A 1.25 monitor: logical 2048x1280, video 2560x1600.
    fn scaled_125() -> (Screen, Stream) {
        (
            screen("eDP-1", 0, 0, 2048, 1280, 1.25),
            stream(40, 0, 0, 2048, 1280),
        )
    }

    #[test]
    fn plan_inside_the_accepted_area_is_one_absolute_motion() {
        let (screen, s) = scaled_125();
        let plan = plan_pointer_motion(&screen, &s, Some((2560, 1600)), 1000, 800);
        assert_eq!(plan.relative, None);
        assert_close(plan.absolute, (1250.0, 1000.0));
    }

    #[test]
    fn plan_in_the_outer_band_clamps_and_moves_the_remainder() {
        let (screen, s) = scaled_125();
        // x = 1800 -> P.x = 2250 >= 2048 (band); y = 1100 -> P.y = 1375 >= 1280.
        let plan = plan_pointer_motion(&screen, &s, Some((2560, 1600)), 1800, 1100);
        assert_close(plan.absolute, (2047.0, 1279.0));
        let (dx, dy) = plan.relative.expect("band point needs relative motion");
        // Mutter lands the absolute part at A_s / 1.25 = (1637.6, 1023.2).
        assert_close((dx, dy), (1800.0 - 1637.6, 1100.0 - 1023.2));
        assert_close((2047.0 / 1.25 + dx, 1279.0 / 1.25 + dy), (1800.0, 1100.0));
        // Only one axis in the band: the other is exact and its remainder 0.
        let plan = plan_pointer_motion(&screen, &s, Some((2560, 1600)), 2000, 100);
        assert_close(plan.absolute, (2047.0, 125.0));
        assert_close(plan.relative.unwrap(), (2000.0 - 1637.6, 0.0));
    }

    #[test]
    fn plan_never_splits_at_scale_one() {
        let s = stream(41, 1920, 0, 3440, 1440);
        for (x, y) in [(1920, 0), (5359, 1439), (4000, 700)] {
            for video in [Some((3440, 1440)), None] {
                let plan = plan_pointer_motion(&hdmi(), &s, video, x, y);
                assert_eq!(plan.relative, None, "({x},{y})");
            }
        }
    }

    #[test]
    fn select_stream_matches_by_origin() {
        let streams = [
            stream(40, 0, 240, 1920, 1200),
            stream(41, 1920, 0, 3440, 1440),
        ];
        assert_eq!(select_stream(&streams, &hdmi(), None).unwrap().node_id, 41);
        assert_eq!(select_stream(&streams, &edp(), None).unwrap().node_id, 40);
    }

    #[test]
    fn select_stream_refuses_an_unshared_monitor() {
        let streams = [stream(41, 1920, 0, 3440, 1440)];
        let mutter = [edp(), hdmi()];
        let error = select_stream(&streams, &edp(), Some(&mutter)).unwrap_err();
        assert_eq!(
            crate::error_kind::kind_of(&error),
            Some(ErrorKind::NotShared)
        );
        let error = error.to_string();
        assert_eq!(
            error,
            "monitor eDP-1 (0,240 1920x1200) is not shared in the portal session \
             (shared: HDMI-1 at 1920,0 3440x1440)"
        );
        // Without Mutter the shared stream is named by its node.
        let error = select_stream(&streams, &edp(), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("shared: stream-41 at 1920,0 3440x1440"),
            "{error}"
        );
    }

    #[test]
    fn select_stream_single_head_tolerates_a_stale_origin() {
        let streams = [stream(40, 0, 0, 1920, 1200)];
        let only = screen("eDP-1", 0, 240, 1920, 1200, SCALE_4_3);
        let single = [only.clone()];
        assert_eq!(
            select_stream(&streams, &only, Some(&single))
                .unwrap()
                .node_id,
            40
        );
        // One stream but two monitors: not single-head, so no fallback.
        let two = [only.clone(), hdmi()];
        assert!(select_stream(&streams, &only, Some(&two)).is_err());
        // Mutter unknown: no fallback either.
        assert!(select_stream(&streams, &only, None).is_err());
    }
}
