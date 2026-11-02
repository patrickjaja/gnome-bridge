use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "gnome-portal-bridge",
    version,
    about = "GNOME/Wayland portal + PipeWire support bridge for Linux computer-use tooling."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Report desktop environment, portal, and GNOME Shell diagnostics.
    Doctor,
    /// Enumerate screens (Mutter logical monitors).
    Screens,
    /// Enumerate windows (best-effort via GNOME Shell Introspect).
    Windows,
    /// Report the current global cursor position. Not supported on GNOME/Wayland.
    CursorPosition,
    /// Report the currently active (frontmost) app.
    FrontmostApp,
    /// Report the topmost app at a global point.
    AppUnderPoint {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Activate (raise + focus) a window by id. Not supported on GNOME/Wayland.
    ActivateWindow {
        #[arg(long)]
        window: String,
    },
    /// Capture a full monitor and return an executor-style screenshot.
    Screenshot {
        #[arg(long)]
        display: Option<String>,
    },
    /// Capture a logical region within a display and return an executor-style zoom image.
    Zoom {
        #[arg(long)]
        display: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long)]
        w: i32,
        #[arg(long)]
        h: i32,
    },
    /// Move the pointer to a global point.
    PointerMove {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Click at a global point.
    PointerClick {
        #[arg(long = "modifier")]
        modifiers: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Scroll at a global point.
    PointerScroll {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dx: f64,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dy: f64,
    },
    /// Drag between two global points with the left button.
    PointerDrag {
        #[arg(long, allow_hyphen_values = true)]
        from_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        from_y: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_y: i32,
    },
    /// Press and hold the left mouse button until explicitly released.
    LeftMouseDown,
    /// Release the left mouse button if it is currently held.
    LeftMouseUp,
    /// Send an executor-style key sequence such as `ctrl+c`.
    KeySequence {
        #[arg(long)]
        keys: String,
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Type text through the portal as Unicode keysyms.
    Type {
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = 12)]
        delay_ms: u64,
    },
    /// Hold one or more keys for a fixed duration in milliseconds.
    HoldKey {
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        #[arg(long)]
        duration_ms: u64,
    },
    /// Start a long-lived portal session daemon for this tool-use lock.
    SessionStart {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// End the long-lived portal session daemon for this tool-use lock.
    SessionEnd,
    /// End any session daemon and delete the stored portal restore token, so
    /// the next session shows the consent dialog again (to share a different
    /// set of monitors).
    ForgetSession,
    /// Turn desktop accessibility (AT-SPI IsEnabled) on for a holder process.
    /// Never opens a portal session.
    AxEnable {
        /// The long-lived process that needs accessibility (Claude's main process).
        #[arg(long)]
        holder_pid: u32,
    },
    /// Drop a holder; turn accessibility back off when it was ours and no
    /// holder is left.
    AxRestore {
        #[arg(long)]
        holder_pid: u32,
    },
    /// List the accessible elements of a window (from `windows`, or by app id).
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(true).args(["window", "app_id"])))]
    AxFind {
        /// Window id from `windows` (GNOME Shell Introspect).
        #[arg(long)]
        window: Option<String>,
        /// App id (desktop file id) when Introspect does not list windows.
        #[arg(long)]
        app_id: Option<String>,
        /// Window title, with --app-id.
        #[arg(long)]
        title: Option<String>,
        /// Role filter: AX name ("AXButton") or AT-SPI name ("push button").
        #[arg(long)]
        role: Option<String>,
        /// Name or description substring, case-insensitive.
        #[arg(long)]
        title_contains: Option<String>,
        /// Not only interactive elements.
        #[arg(long, default_value_t = false)]
        all: bool,
        /// Also walk elements that are not showing.
        #[arg(long, default_value_t = false)]
        include_hidden: bool,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long)]
        max_depth: Option<u32>,
    },
    /// Act on an element from `ax-find`. The text for set-text is read from
    /// stdin (whole, UTF-8, untrimmed), never from argv.
    AxAction {
        /// The `scope` value `ax-find` returned.
        #[arg(long)]
        scope: String,
        /// The element's `ref` from `ax-find`.
        #[arg(long = "ref")]
        element_ref: String,
        #[arg(long, value_enum)]
        action: crate::ax::AxActionKind,
    },
    /// Internal: run the session daemon serving a bound unix socket.
    #[command(hide = true)]
    ServeSession {
        #[arg(long)]
        socket: String,
    },
}
