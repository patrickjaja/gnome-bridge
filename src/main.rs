mod capture;
mod cli;
mod daemon;
mod doctor;
mod input;
mod output;
mod portal;
mod screens;
mod token_store;
mod windows;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::daemon::{
    SessionRequest, resolve_screen, run_action, serve_session, start_session_daemon,
    stop_session_daemon,
};
use crate::output::print_json;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Doctor => {
            print_json(&doctor::doctor().await)?;
        }
        Command::Screens => {
            print_json(&daemon::list_screens_best_effort().await?)?;
        }
        Command::Windows => {
            // Introspect is allowlist-restricted; an unavailable API yields [].
            let connection = zbus::Connection::session().await?;
            print_json(&windows::list_windows(&connection).await)?;
        }
        Command::CursorPosition => {
            // No portable GNOME/Wayland global-cursor query; the JS uses
            // Electron's getCursorScreenPoint instead. Report not-supported.
            anyhow::bail!(
                "cursor-position is not supported on GNOME/Wayland; the executor uses \
                 Electron's getCursorScreenPoint instead"
            );
        }
        Command::FrontmostApp => {
            let connection = zbus::Connection::session().await?;
            match windows::frontmost_app(&connection).await {
                Some(app) => print_json(&app)?,
                None => print_json(&serde_json::Value::Null)?,
            }
        }
        Command::AppUnderPoint { x, y } => {
            let connection = zbus::Connection::session().await?;
            match windows::app_under_point(&connection, x, y).await {
                Some(app) => print_json(&app)?,
                None => print_json(&serde_json::Value::Null)?,
            }
        }
        Command::ActivateWindow { window } => {
            // GNOME Shell Introspect has no activate; the executor's open-app
            // path launches apps itself. Report a clean not-supported error.
            let _ = window;
            anyhow::bail!(
                "activate-window is not supported on GNOME/Wayland (Shell Introspect \
                 exposes no activation); the executor launches apps directly"
            );
        }

        Command::Screenshot { display } => {
            let screen = resolve_screen(display.as_deref()).await?;
            let value = run_action(SessionRequest::Screenshot { screen }).await?;
            print_json(&value)?;
        }
        Command::Zoom {
            display,
            x,
            y,
            w,
            h,
        } => {
            // With `--display` the region is display-relative (the x11/kwin
            // contract). Without it the caller only has GLOBAL coordinates
            // (the JS falls back to that when it has no monitor list), so
            // resolve the screen under the region's origin and translate.
            let (screen, x, y) = match display.as_deref() {
                Some(selector) => (resolve_screen(Some(selector)).await?, x, y),
                None => {
                    let screen = resolve_screen_for_point(x, y).await?;
                    (screen.clone(), x - screen.geometry.x, y - screen.geometry.y)
                }
            };
            let value = run_action(SessionRequest::Zoom { screen, x, y, w, h }).await?;
            print_json(&value)?;
        }

        Command::PointerMove { x, y } => {
            let screen = resolve_screen_for_point(x, y).await?;
            let value = run_action(SessionRequest::MovePointer { screen, x, y }).await?;
            print_json(&value)?;
        }
        Command::PointerClick {
            modifiers,
            x,
            y,
            button,
            count,
        } => {
            let screen = resolve_screen_for_point(x, y).await?;
            let value = run_action(SessionRequest::Click {
                screen,
                x,
                y,
                button,
                count,
                modifiers,
            })
            .await?;
            print_json(&value)?;
        }
        Command::PointerScroll { x, y, dx, dy } => {
            let screen = resolve_screen_for_point(x, y).await?;
            let value = run_action(SessionRequest::Scroll {
                screen,
                x,
                y,
                dx,
                dy,
            })
            .await?;
            print_json(&value)?;
        }
        Command::PointerDrag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            let from_screen = resolve_screen_for_point(from_x, from_y).await?;
            let to_screen = resolve_screen_for_point(to_x, to_y).await?;
            let value = run_action(SessionRequest::Drag {
                from_screen,
                from_x,
                from_y,
                to_screen,
                to_x,
                to_y,
            })
            .await?;
            print_json(&value)?;
        }
        Command::LeftMouseDown => {
            let value = run_action(SessionRequest::LeftMouseDown).await?;
            print_json(&value)?;
        }
        Command::LeftMouseUp => {
            let value = run_action(SessionRequest::LeftMouseUp).await?;
            print_json(&value)?;
        }

        Command::KeySequence { keys, repeat } => {
            let value = run_action(SessionRequest::KeySequence { keys, repeat }).await?;
            print_json(&value)?;
        }
        Command::Type { text, delay_ms } => {
            let value = run_action(SessionRequest::Type { text, delay_ms }).await?;
            print_json(&value)?;
        }
        Command::HoldKey { keys, duration_ms } => {
            let value = run_action(SessionRequest::HoldKey { keys, duration_ms }).await?;
            print_json(&value)?;
        }

        Command::SessionStart { foreground } => {
            if foreground {
                // Run the daemon in the foreground (blocks until SIGTERM). The
                // socket is prepared here so a subsequent connect works.
                let socket = daemon::socket_path()?;
                let result = serve_session(socket).await;
                exit_after_serve(result);
            } else {
                print_json(&start_session_daemon().await?)?;
            }
        }
        Command::SessionEnd => {
            print_json(&stop_session_daemon().await?)?;
        }
        Command::ServeSession { socket } => {
            let result = serve_session(std::path::PathBuf::from(socket)).await;
            exit_after_serve(result);
        }
    }

    Ok(())
}

/// The daemon must never outlive its session: a PipeWire or portal thread that
/// refuses to wind down would otherwise keep the process (and the compositor's
/// screen-sharing indicator) alive after `session-end`. Hard-exit instead of
/// relying on the runtime teardown.
fn exit_after_serve(result: Result<()>) -> ! {
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1)
        }
    }
}

/// Resolve the screen containing a global point, else the default screen. Point
/// actions carry a resolved `Screen` so the daemon can map global -> stream
/// coordinates without its own Mutter connection.
async fn resolve_screen_for_point(x: i32, y: i32) -> Result<output::Screen> {
    let screens = daemon::list_screens_best_effort().await?;
    if let Some(screen) = screens.iter().find(|s| {
        x >= s.geometry.x
            && x < s.geometry.x + s.geometry.width
            && y >= s.geometry.y
            && y < s.geometry.y + s.geometry.height
    }) {
        return Ok(screen.clone());
    }
    daemon::pick_screen(&screens, None)
}
