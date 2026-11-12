mod ax;
mod capture;
mod cli;
mod daemon;
mod damage;
mod doctor;
mod error_kind;
mod frames;
mod input;
mod output;
mod overlay;
mod portal;
mod screens;
mod stopped;
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

/// Exit codes follow the Computer Use bridge CLI contract v1: 0 success,
/// 1 runtime error, 2 bad usage (clap), 3 monitor not shared in the portal
/// session, 4 no live portal session, 5 the user stopped screen sharing (see
/// `error_kind` and `stopped`).
fn main() -> ExitCode {
    // Bad usage exits 2 inside `parse`.
    let cli = Cli::parse();

    // overlay: before the tokio runtime. cu-overlay owns stdin / stdout
    // (protocol lines only), the parent-death watch and the exit code, and
    // blocks on its own X connection.
    if matches!(cli.command, Command::Overlay) {
        return overlay::run();
    }

    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async_main(cli)),
        Err(error) => {
            eprintln!("failed to start the tokio runtime: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main(cli: Cli) -> ExitCode {
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            if error.downcast_ref::<ax::UsageError>().is_some() {
                return ExitCode::from(2);
            }
            ExitCode::from(error_kind::exit_code(error_kind::kind_of(&error)))
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    // Contract v1 exit 5: after the user stopped screen sharing, input and
    // capture fail here, before a screen lookup or the daemon proxy could
    // open a portal session of their own (a remembered grant would restart
    // sharing without asking).
    if uses_portal(&cli.command) {
        stopped::gate()?;
    }

    match cli.command {
        Command::Overlay => unreachable!("handled in main"),
        Command::Doctor => {
            print_json(&doctor::doctor().await)?;
        }
        Command::Screens => {
            // Never opens a portal session (contract v1): without Mutter
            // and without a daemon this is an error, not a consent dialog.
            print_json(&daemon::list_screens_with_sharing(daemon::Ephemeral::Refused).await?)?;
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
            // Always null: Shell Introspect reports no window position (see
            // `windows::app_under_point`). The overlay's own windows are
            // override-redirect and never listed by Introspect either.
            print_json(&windows::app_under_point(x, y))?;
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
                stopped::clear();
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
        Command::ForgetSession => {
            // `stop_session_daemon` (inside) removes the stopped marker.
            print_json(&daemon::forget_session().await?)?;
        }
        Command::AxEnable { holder_pid } => ax::enable(holder_pid).await?,
        Command::AxRestore { holder_pid } => ax::restore(holder_pid).await?,
        Command::AxFind {
            window,
            app_id,
            title,
            ax,
        } => {
            ax::find(ax::FindArgs {
                window,
                app_id,
                title,
                flags: ax,
            })
            .await?;
        }
        Command::AxAction { ax } => ax::action(&ax).await?,
        Command::AxFeatures => println!("{}", cu_atspi::features_json()),
        Command::ServeSession { socket } => {
            let result = serve_session(std::path::PathBuf::from(socket)).await;
            exit_after_serve(result);
        }
    }

    Ok(())
}

/// Commands that talk to the portal session (input and capture): blocked
/// while the stopped-by-user marker exists. Listing, diagnostics and the
/// session lifecycle commands never open a portal session of their own.
fn uses_portal(command: &Command) -> bool {
    match command {
        Command::Screenshot { .. }
        | Command::Zoom { .. }
        | Command::PointerMove { .. }
        | Command::PointerClick { .. }
        | Command::PointerScroll { .. }
        | Command::PointerDrag { .. }
        | Command::LeftMouseDown
        | Command::LeftMouseUp
        | Command::KeySequence { .. }
        | Command::Type { .. }
        | Command::HoldKey { .. } => true,
        Command::Doctor
        | Command::Overlay
        | Command::Screens
        | Command::Windows
        | Command::CursorPosition
        | Command::FrontmostApp
        | Command::AppUnderPoint { .. }
        | Command::ActivateWindow { .. }
        | Command::SessionStart { .. }
        | Command::SessionEnd
        | Command::ForgetSession
        | Command::AxEnable { .. }
        | Command::AxRestore { .. }
        | Command::AxFind { .. }
        | Command::AxAction { .. }
        | Command::AxFeatures
        | Command::ServeSession { .. } => false,
    }
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
    // Outside every monitor: the default screen, preferring shared ones.
    resolve_screen(None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Command {
        let mut argv = vec!["gnome-portal-bridge"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap().command
    }

    #[test]
    fn input_and_capture_are_gated_listing_and_lifecycle_are_not() {
        for args in [
            &["screenshot"][..],
            &["zoom", "--x", "0", "--y", "0", "--w", "1", "--h", "1"],
            &["pointer-move", "--x", "1", "--y", "1"],
            &["pointer-click", "--x", "1", "--y", "1"],
            &["pointer-scroll", "--x", "1", "--y", "1"],
            &[
                "pointer-drag",
                "--from-x",
                "0",
                "--from-y",
                "0",
                "--to-x",
                "1",
                "--to-y",
                "1",
            ],
            &["left-mouse-down"],
            &["left-mouse-up"],
            &["key-sequence", "--keys", "a"],
            &["type", "--text", "a"],
            &["hold-key", "--key", "a", "--duration-ms", "1"],
        ] {
            assert!(uses_portal(&command(args)), "{args:?}");
        }
        for args in [
            &["doctor"][..],
            &["overlay"],
            &["screens"],
            &["windows"],
            &["frontmost-app"],
            &["app-under-point", "--x", "1", "--y", "1"],
            &["session-start"],
            &["session-end"],
            &["forget-session"],
            &["ax-enable", "--holder-pid", "1"],
            &["ax-restore", "--holder-pid", "1"],
            &["ax-find", "--window", "1"],
            &[
                "ax-action",
                "--scope",
                "s",
                "--ref",
                "r",
                "--action",
                "click",
            ],
        ] {
            assert!(!uses_portal(&command(args)), "{args:?}");
        }
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        let mut argv = vec!["gnome-portal-bridge"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap_err()
    }

    #[test]
    fn ax_find_flattened_flags_parse() {
        // Today's flag sets still parse, with or without --window.
        for args in [
            &["ax-find", "--window", "42"][..],
            &["ax-find", "--app-id", "org.gnome.Calculator"],
            &[
                "ax-find",
                "--app-id",
                "a",
                "--title",
                "t",
                "--all",
                "--max-depth",
                "3",
            ],
            &[
                "ax-find",
                "--window",
                "1",
                "--role",
                "AXButton",
                "--title-contains",
                "x",
            ],
            &[
                "ax-find",
                "--window",
                "1",
                "--window-ref",
                "r",
                "--deadline-ms",
                "300",
            ],
            &[
                "ax-find",
                "--window",
                "1",
                "--root-ref",
                "r",
                "--scope",
                "s",
            ],
            &["ax-find", "--window", "1", "--skeleton", "--max-depth", "2"],
        ] {
            assert!(matches!(command(args), Command::AxFind { .. }), "{args:?}");
        }
        let Command::AxFind {
            app_id, title, ax, ..
        } = command(&[
            "ax-find",
            "--app-id",
            "a",
            "--title",
            "t",
            "--include-hidden",
        ])
        else {
            panic!()
        };
        assert_eq!(
            (app_id.as_deref(), title.as_deref()),
            (Some("a"), Some("t"))
        );
        assert!(ax.include_hidden);
        // Usage errors (exit 2).
        for args in [
            &["ax-find"][..],
            &["ax-find", "--title", "x"],
            &["ax-find", "--window", "1", "--root-ref", "r"],
            &["ax-find", "--window", "1", "--scope", "s"],
            &[
                "ax-find",
                "--window",
                "1",
                "--skeleton",
                "--role",
                "AXButton",
            ],
        ] {
            assert_eq!(parse_err(args).exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn ax_action_flattened_flags_parse() {
        for action in [
            "click",
            "focus",
            "set-text",
            "ensure-checked",
            "ensure-unchecked",
            "ensure-expanded",
            "ensure-collapsed",
            "ensure-selected",
            "scroll-to",
            "set-value",
        ] {
            let command = command(&[
                "ax-action",
                "--scope",
                "s",
                "--ref",
                "r",
                "--action",
                action,
            ]);
            assert!(matches!(command, Command::AxAction { .. }), "{action}");
        }
        assert_eq!(
            parse_err(&[
                "ax-action",
                "--scope",
                "s",
                "--ref",
                "r",
                "--action",
                "type"
            ])
            .exit_code(),
            2
        );
        assert!(!uses_portal(&command(&["ax-features"])));
    }
}
