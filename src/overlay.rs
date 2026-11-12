//! `overlay`: Claude's Computer Use glow, drawn cursor and badge through the
//! shared cu-overlay crate (protocol 1, JSON lines on stdin / stdout), on its
//! XWayland backend: Mutter has no layer-shell, so the overlay is a set of
//! override-redirect ARGB X windows with an empty input shape.
//!
//! cu-overlay owns stdin, stdout (protocol lines only), the parent-death
//! watch (`CLAUDE_CU_PARENT_PID`) and the exit code. It runs on the main
//! thread before any long-lived async runtime exists; this module only adds
//! the bridge's one piece of knowledge the crate cannot have.
//!
//! Scale hint: from X, Mutter's physical layout mode (`layout-mode` = 2,
//! `scale-monitor-framebuffer` off, an integer scale > 1, the layout in
//! physical pixels) is indistinguishable from XWayland native scaling (the
//! layout in logical pixels, X pixels = logical x the factor): both announce
//! the factor through XSETTINGS and `Xft.dpi`. The crate would divide by it
//! in both. The bridge reads the layout mode from Mutter's DisplayConfig (the
//! same bounded read `screens` makes, no portal session) and, in the physical
//! mode, passes the factor 1, so `ready.outputs` is in the layout units
//! `screens` reports. In the logical mode the crate detects the factor itself.
//!
//! The glow stays static (the crate's x11 default): every pulse step makes
//! Mutter recomposite the whole output.
//!
//! Nothing can exclude the windows from the portal ScreenCast stream: the
//! caller hides them around each `screenshot` / `zoom` with the protocol's
//! `capture` command (scope `all`) and waits for its `ack`.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use cu_overlay::backend::x11::{Options, X11};

use crate::screens::LayoutMode;

/// Bound for the DisplayConfig read (session bus connect + GetCurrentState).
/// On a wedged bus the overlay starts with the crate's own detection.
const LAYOUT_READ_TIMEOUT: Duration = Duration::from_millis(1000);

/// `gnome-portal-bridge overlay`.
pub fn run() -> ExitCode {
    let want = std::env::var(cu_overlay::BACKEND_ENV).unwrap_or_default();
    let forced = match want.trim() {
        "" | "auto" => false,
        "x11" => true,
        // `layer-shell` (not in this build) or a typo: the crate's own
        // fatal error and exit code.
        _ => return cu_overlay::run(std::io::stdin(), std::io::stdout()),
    };
    let mut opts = Options::from_env(forced);
    let mode = read_layout_mode();
    opts.scale = scale_hint(opts.scale, mode);
    match X11::connect(opts) {
        Ok(backend) => cu_overlay::run_with(backend, std::io::stdin(), std::io::stdout()),
        Err(error) => fatal(&mut std::io::stdout(), &error),
    }
}

/// The X-to-logical factor to force: an explicit `CU_OVERLAY_X11_SCALE`
/// wins, Mutter's physical layout mode forces 1, otherwise (logical mode or
/// unknown) the crate detects it from X.
pub fn scale_hint(env_scale: Option<i32>, mode: Option<LayoutMode>) -> Option<i32> {
    match (env_scale, mode) {
        (Some(scale), _) => Some(scale),
        (None, Some(LayoutMode::Physical)) => Some(1),
        (None, Some(LayoutMode::Logical) | None) => None,
    }
}

/// Mutter's layout mode, `None` when DisplayConfig does not answer in time.
/// A short-lived current-thread runtime that is gone before the overlay
/// starts; a failure costs one stderr line, never a stdout line.
fn read_layout_mode() -> Option<LayoutMode> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("gnome-portal-bridge overlay: no runtime for the layout read: {error}");
            return None;
        }
    };
    let result = runtime.block_on(async {
        tokio::time::timeout(LAYOUT_READ_TIMEOUT, async {
            let connection = zbus::Connection::session().await?;
            crate::screens::layout_mode(&connection).await
        })
        .await
    });
    match result {
        Ok(Ok(mode)) => Some(mode),
        Ok(Err(error)) => {
            eprintln!(
                "gnome-portal-bridge overlay: Mutter layout mode unknown ({error:#}); \
                 the overlay detects the scale from X"
            );
            None
        }
        Err(_) => {
            eprintln!(
                "gnome-portal-bridge overlay: Mutter DisplayConfig did not answer within {} ms; \
                 the overlay detects the scale from X",
                LAYOUT_READ_TIMEOUT.as_millis()
            );
            None
        }
    }
}

/// What `cu_overlay::run` does when no backend fits: one fatal `error`
/// line, exit 2, nothing created.
fn fatal(out: &mut impl Write, error: &str) -> ExitCode {
    let line = cu_overlay::protocol::Event::error(error.to_owned(), true).line();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_layout_forces_factor_one_logical_lets_the_crate_detect() {
        assert_eq!(scale_hint(None, Some(LayoutMode::Physical)), Some(1));
        assert_eq!(scale_hint(None, Some(LayoutMode::Logical)), None);
        assert_eq!(scale_hint(None, None), None);
        // An explicit CU_OVERLAY_X11_SCALE always wins.
        assert_eq!(scale_hint(Some(2), Some(LayoutMode::Physical)), Some(2));
        assert_eq!(scale_hint(Some(2), Some(LayoutMode::Logical)), Some(2));
        assert_eq!(scale_hint(Some(1), None), Some(1));
    }

    #[test]
    fn fatal_is_one_protocol_error_line_and_exit_2() {
        let mut out = Vec::new();
        let code = fatal(&mut out, "no XWayland");
        assert_eq!(code, ExitCode::from(2));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["event"], "error");
        assert_eq!(value["fatal"], true);
        assert_eq!(value["message"], "no XWayland");
    }
}
