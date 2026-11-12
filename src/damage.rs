//! Make Mutter record a ScreenCast frame of one monitor NOW, without input.
//!
//! Mutter records a memory-buffer (SHM) monitor stream from an idle callback
//! (`maybe_record_frame_on_idle`, `G_PRIORITY_DEFAULT_IDLE`) that a stage
//! paint of the monitor's view schedules, and only on such a paint: a static
//! monitor produces no frame at all. A capture therefore damages the monitor
//! it captures and waits for the frame Mutter then records (see
//! `LivePortalSession::capture_frame`): that frame is painted after the
//! request, so it shows everything that changed before it.
//!
//! The damage comes from one 1x1 override-redirect XWayland window per
//! monitor, fully transparent (ARGB visual, pixel 0), with an empty input
//! shape, mapped at the monitor's top-left X pixel for the life of the
//! session. A poke clears that pixel (`ClearArea` with its own transparent
//! background): XWayland reports the damage, Mutter repaints that pixel of
//! the view - no visible change, no input event, no focus change, nothing in
//! Introspect's window list. Unlike the pointer nudge (`flush_after_input`)
//! it works on any monitor, with the pointer anywhere, and cannot move a
//! menu's selection.
//!
//! Mutter starts XWayland on the first X connection (180-270 ms once per
//! login). Without `$DISPLAY` or XWayland, `DamagePoker::connect` fails and
//! captures fall back to settling on whatever Mutter sends.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ClipOrdering, ColormapAlloc, ConnectionExt as _, CreateWindowAux, WindowClass,
};
use x11rb::rust_connection::RustConnection;

pub struct DamagePoker {
    conn: RustConnection,
    root: u32,
    depth32: Option<(u32, u32)>,
    /// Window per monitor name, with the X rect it was placed for.
    windows: HashMap<String, (u32, (i16, i16))>,
}

impl DamagePoker {
    /// Connect to the session's X server (XWayland under Mutter).
    pub fn connect() -> Result<Self> {
        if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
            bail!("no DISPLAY (XWayland not available)");
        }
        let (conn, screen_num) = RustConnection::connect(None).context("cannot connect to X")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth32 = screen
            .allowed_depths
            .iter()
            .find(|d| d.depth == 32)
            .and_then(|d| d.visuals.first())
            .map(|v| (v.visual_id, 32));
        // Probe the extensions now, so a missing one fails here, not later.
        conn.randr_get_monitors(root, true)
            .context("RandR GetMonitors")?
            .reply()
            .context("RandR GetMonitors")?;
        Ok(Self {
            conn,
            root,
            depth32,
            windows: HashMap::new(),
        })
    }

    /// Damage one pixel of the monitor named `name` (the Mutter connector
    /// name, which XWayland uses as the RandR monitor name).
    pub fn poke(&mut self, name: &str) -> Result<()> {
        let origin = self.monitor_origin(name)?;
        let window = match self.windows.get(name) {
            Some(&(window, at)) if at == origin => window,
            stale => {
                if let Some(&(window, _)) = stale {
                    let _ = self.conn.destroy_window(window);
                }
                let window = self.create_window(origin)?;
                self.windows.insert(name.to_owned(), (window, origin));
                window
            }
        };
        self.conn
            .clear_area(false, window, 0, 0, 1, 1)
            .context("ClearArea")?;
        // A round trip, so the request reached the server (and any error for
        // it came back) before the caller starts waiting for the frame.
        self.conn
            .get_input_focus()
            .context("X round trip")?
            .reply()
            .context("X round trip")?;
        Ok(())
    }

    /// Whether the X connection itself failed (the poker must be rebuilt).
    pub fn is_broken(&self) -> bool {
        self.conn
            .get_input_focus()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_none()
    }

    fn monitor_origin(&self, name: &str) -> Result<(i16, i16)> {
        let monitors = self
            .conn
            .randr_get_monitors(self.root, true)
            .context("RandR GetMonitors")?
            .reply()
            .context("RandR GetMonitors")?;
        for monitor in &monitors.monitors {
            let atom = self
                .conn
                .get_atom_name(monitor.name)
                .context("GetAtomName")?
                .reply()
                .context("GetAtomName")?;
            if atom.name == name.as_bytes() {
                return Ok((monitor.x, monitor.y));
            }
        }
        bail!("no X monitor named {name}")
    }

    fn create_window(&self, (x, y): (i16, i16)) -> Result<u32> {
        let window = self.conn.generate_id().context("X id")?;
        let mut aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0)
            .border_pixel(0);
        let (depth, visual) = match self.depth32 {
            Some((visual, depth)) => {
                let colormap = self.conn.generate_id().context("X id")?;
                self.conn
                    .create_colormap(ColormapAlloc::NONE, colormap, self.root, visual)
                    .context("CreateColormap")?;
                aux = aux.colormap(colormap);
                (depth as u8, visual)
            }
            // No ARGB visual: a black pixel is still better than no frame.
            None => (x11rb::COPY_DEPTH_FROM_PARENT, x11rb::COPY_FROM_PARENT),
        };
        self.conn
            .create_window(
                depth,
                window,
                self.root,
                x,
                y,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &aux,
            )
            .context("CreateWindow")?;
        // Click-through: an empty input shape.
        self.conn
            .shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )
            .context("Shape input")?;
        self.conn.map_window(window).context("MapWindow")?;
        self.conn.flush().context("X flush")?;
        Ok(window)
    }
}
