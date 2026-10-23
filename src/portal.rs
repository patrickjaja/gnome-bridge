//! The portal + PipeWire engine.
//!
//! A `LivePortalSession` holds ONE combined XDG RemoteDesktop + ScreenCast
//! portal session (via `lamco-portal`) for the whole tool-use lock. Input goes
//! through the RemoteDesktop `notify_*` calls; screenshots grab one PipeWire
//! frame per capture through `lamco-pipewire`.
//!
//! Coordinate model: the JS sends GLOBAL (compositor logical) coordinates. The
//! portal's pointer-motion-absolute call is STREAM-LOCAL, so we map a global
//! point into a stream's pixel space using the target screen's geometry (offset
//! + logical size) against the stream's reported size - the same idea as the
//!   kwin bridge's `local_stream_point`.
//!
//! Session persistence: with RemoteDesktop portal v2 (GNOME 46+) the portal
//! returns a restore token which we persist under
//! `$XDG_STATE_HOME/gnome-portal-bridge/`, so subsequent sessions reuse the
//! grant with no consent dialog. Persistence is requested through
//! RemoteDesktop.SelectDevices ONLY - see the session-setup section below for
//! why. On RemoteDesktop v1 (GNOME <= 45) there is no persistence, so a dialog
//! appears once per session (accepted by design).

use std::collections::HashSet;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
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
use lamco_pipewire::{
    PipeWireThreadCommand, PipeWireThreadManager, PixelFormat, StreamConfig, VideoFrame,
};
use lamco_portal::{PortalConfig, PortalManager, PortalSessionHandle, SourceType};

use crate::capture;
use crate::input::keymap::{self, char_to_keysym, keysym_for_name, parse_spec};
use crate::input::{
    Button, ButtonStateResult, DragActionResult, KeyboardActionResult, PointerActionResult,
    TypeActionResult,
};
use crate::output::{Rect, Screen, ScreenshotCapture, ScreenshotResult};
use crate::token_store::TokenStore;

/// A single portal ScreenCast stream (one per monitor), as reported by the
/// session. `position`/`size` are in the compositor's global logical space.
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

/// A live combined RemoteDesktop + ScreenCast session that stays open for the
/// whole tool-use lock.
pub struct LivePortalSession {
    manager: PortalManager,
    session: PortalSessionHandle,
    streams: Vec<Stream>,
    pipewire: Option<PipeWire>,
    held_buttons: HashSet<i32>,
}

struct PipeWire {
    manager: PipeWireThreadManager,
    active_streams: HashSet<u32>,
}

impl LivePortalSession {
    /// Open a new portal session, reusing a persisted restore token if present.
    pub async fn open() -> Result<Self> {
        let (manager, session, _restore_token) = start_session().await?;
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
        })
    }

    /// Screens derived from the portal streams, used as a fallback when the
    /// Mutter DisplayConfig API is unavailable. Each monitor stream becomes a
    /// `Screen` in global logical coordinates; the first is treated as primary.
    pub fn screens_from_streams(&self) -> Vec<Screen> {
        let single = self.streams.len() == 1;
        self.streams
            .iter()
            .enumerate()
            .filter(|(_, s)| s.source_type == SourceType::Monitor || single)
            .map(|(idx, s)| {
                let is_primary = idx == 0 || single;
                Screen {
                    id: format!("stream-{}", s.node_id),
                    name: format!("stream-{}", s.node_id),
                    geometry: Rect {
                        x: s.position.0,
                        y: s.position.1,
                        width: s.size.0 as i32,
                        height: s.size.1 as i32,
                    },
                    scale: Some(1.0),
                    refresh_millihz: None,
                    is_active: is_primary,
                    is_primary,
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
        if let Some(mut pw) = self.pipewire.take() {
            pw.manager.shutdown().ok();
        }
        self.manager.cleanup().await.ok();
        drop(self.session);
        Ok(())
    }

    /// Find the stream whose geometry contains a global point, else the stream
    /// matching the given screen, else the first stream.
    fn stream_for_screen(&self, screen: &Screen) -> Result<&Stream> {
        // Exact position match first (screen origin == stream position).
        if let Some(stream) = self
            .streams
            .iter()
            .find(|s| s.position.0 == screen.geometry.x && s.position.1 == screen.geometry.y)
        {
            return Ok(stream);
        }
        self.streams
            .first()
            .ok_or_else(|| anyhow::anyhow!("portal session returned no streams"))
    }

    /// Map a global point onto a stream's pixel space.
    fn local_point(&self, screen: &Screen, stream: &Stream, x: i32, y: i32) -> (f64, f64) {
        let local_x = f64::from(x - screen.geometry.x);
        let local_y = f64::from(y - screen.geometry.y);
        let logical_w = f64::from(screen.geometry.width.max(1));
        let logical_h = f64::from(screen.geometry.height.max(1));
        let stream_w = f64::from(stream.size.0.max(1));
        let stream_h = f64::from(stream.size.1.max(1));
        (
            (local_x / logical_w) * stream_w,
            (local_y / logical_h) * stream_h,
        )
    }

    async fn motion_absolute(&self, screen: &Screen, x: i32, y: i32) -> Result<()> {
        let stream = self.stream_for_screen(screen)?;
        let (lx, ly) = self.local_point(screen, stream, x, y);
        self.manager
            .remote_desktop()
            .notify_pointer_motion_absolute(self.session.ashpd_session(), stream.node_id, lx, ly)
            .await
            .context("failed to send absolute pointer motion through the portal")
    }

    pub async fn move_pointer(
        &self,
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
        &self,
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

    /// Grab a single fresh PipeWire frame for the screen's stream.
    async fn capture_frame(&mut self, screen: &Screen) -> Result<VideoFrame> {
        let node_id = self.stream_for_screen(screen)?.node_id;
        let (width, height) = {
            let stream = self.stream_for_screen(screen)?;
            (stream.size.0, stream.size.1)
        };
        self.ensure_stream(node_id, width, height)?;
        let pw = self.pipewire.as_ref().expect("pipewire initialized");
        let frame = recv_frame_for_stream(&pw.manager, node_id, Duration::from_secs(10))?;
        Ok(frame)
    }

    fn ensure_stream(&mut self, node_id: u32, width: u32, height: u32) -> Result<()> {
        if self.pipewire.is_none() {
            let fd = dup_fd(self.session.pipewire_fd())?;
            self.pipewire = Some(PipeWire {
                manager: PipeWireThreadManager::new(fd.into_raw_fd())?,
                active_streams: HashSet::new(),
            });
        }
        let pw = self.pipewire.as_mut().expect("pipewire just initialized");
        if pw.active_streams.contains(&node_id) {
            return Ok(());
        }
        create_stream(&pw.manager, node_id, width, height)?;
        pw.active_streams.insert(node_id);
        Ok(())
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

async fn start_session() -> Result<(PortalManager, PortalSessionHandle, Option<String>)> {
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
) -> Result<(PortalSessionHandle, Option<String>)> {
    let session = remote_desktop
        .create_session(Default::default())
        .await
        .context("failed to create the RemoteDesktop portal session")?;

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
            Ok((handle, token))
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

fn create_stream(
    manager: &PipeWireThreadManager,
    node_id: u32,
    width: u32,
    height: u32,
) -> Result<()> {
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
    manager
        .send_command(PipeWireThreadCommand::CreateStream {
            stream_id: node_id,
            node_id,
            config,
            response_tx,
        })
        .context("failed to send PipeWire CreateStream command")?;
    response_rx
        .recv()
        .context("PipeWire create-stream response channel closed")?
        .context("PipeWire rejected stream creation")?;
    Ok(())
}

fn recv_frame_for_stream(
    manager: &PipeWireThreadManager,
    stream_id: u32,
    timeout: Duration,
) -> Result<VideoFrame> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if let Some(frame) = manager.recv_frame_timeout(remaining.min(Duration::from_millis(500)))
            && frame.frame_id == u64::from(stream_id)
        {
            return Ok(frame);
        }
    }

    let states = manager
        .drain_state_events()
        .into_iter()
        .map(|event| format!("stream {} -> {:?}", event.stream_id, event.state))
        .collect::<Vec<_>>();
    let suffix = if states.is_empty() {
        "no state events observed".to_owned()
    } else {
        format!("state events: {}", states.join(", "))
    };
    bail!("timed out waiting for PipeWire frame on stream {stream_id} ({suffix})");
}

fn dup_fd(raw_fd: i32) -> Result<OwnedFd> {
    let duplicated = unsafe { libc::dup(raw_fd) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to duplicate portal PipeWire file descriptor");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}
