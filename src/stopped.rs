//! The stopped-by-user marker (Computer Use bridge CLI contract v1, exit 5).
//!
//! While Computer Use runs, GNOME shows a screen-sharing indicator, and the
//! user can stop sharing from it. Mutter then stops its RemoteDesktop /
//! ScreenCast session and the portal closes ours (`Session::Closed`). The
//! session daemon records that here, at
//! `${XDG_RUNTIME_DIR:-/tmp}/gnome-portal-bridge/stopped-by-user` (contents:
//! the Unix time it happened), and exits.
//!
//! While the marker exists, every input and capture command fails with
//! [`ErrorKind::StoppedByUser`] BEFORE anything talks to the portal: the
//! ephemeral one-shot fallback (see `daemon::run_action`) must not run, or a
//! remembered grant would silently restart sharing the user just stopped.
//! `session-start` (a new Computer Use task), `session-end` and
//! `forget-session` remove it. `windows` and `doctor` ignore it (`doctor`
//! reports it). `screens` lists from Mutter as usual; only without Mutter
//! and without a daemon does it reach the (gated) ephemeral session and exit
//! 5. The kind keeps its name, but Mutter also closes the session when a
//! shared monitor is unplugged or a stream fails, hence the neutral message.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::error_kind::{ErrorKind, kinded};

/// The stderr text for exit 5.
pub const MESSAGE: &str =
    "screen sharing was stopped (the portal session was closed by the user or the desktop)";

/// The marker path, next to the session socket.
pub fn marker_path() -> PathBuf {
    crate::daemon::runtime_dir().join("stopped-by-user")
}

/// The exit-5 error.
pub fn error() -> anyhow::Error {
    kinded(ErrorKind::StoppedByUser, MESSAGE)
}

/// Whether the marker exists (any file type; a dangling symlink counts).
pub fn is_set_at(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Whether the marker exists at its contract path.
pub fn is_set() -> bool {
    is_set_at(&marker_path())
}

/// Fail with exit 5 while the marker exists.
pub fn gate_at(path: &Path) -> Result<()> {
    if is_set_at(path) {
        Err(error())
    } else {
        Ok(())
    }
}

/// [`gate_at`] at the contract path. Every input and capture command runs
/// this before it resolves a screen or contacts the daemon or the portal.
pub fn gate() -> Result<()> {
    gate_at(&marker_path())
}

/// Write the marker (the daemon's runtime directory already exists).
pub fn record_at(path: &Path) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::write(path, format!("{now}\n"))
        .with_context(|| format!("failed to write `{}`", path.display()))
}

/// Remove the marker; true when there was one. Never creates anything.
pub fn clear_at(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

/// [`clear_at`] at the contract path.
pub fn clear() -> bool {
    clear_at(&marker_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_kind::{exit_code, kind_of};

    fn temp_marker(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gpb-stopped-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("stopped-by-user")
    }

    #[test]
    fn marker_round_trip() {
        let marker = temp_marker("round-trip");
        assert!(!is_set_at(&marker));
        assert!(gate_at(&marker).is_ok());
        assert!(!clear_at(&marker), "nothing to clear yet");

        record_at(&marker).unwrap();
        assert!(is_set_at(&marker));
        let contents = std::fs::read_to_string(&marker).unwrap();
        let line = contents.strip_suffix('\n').expect("one line");
        assert!(line.parse::<u64>().unwrap() > 1_700_000_000, "{line}");

        let error = gate_at(&marker).unwrap_err();
        assert_eq!(kind_of(&error), Some(ErrorKind::StoppedByUser));
        assert_eq!(exit_code(kind_of(&error)), 5);
        assert_eq!(format!("{error:#}"), MESSAGE);

        assert!(clear_at(&marker));
        assert!(!is_set_at(&marker));
        assert!(gate_at(&marker).is_ok());
        std::fs::remove_dir_all(marker.parent().unwrap()).ok();
    }

    #[test]
    fn marker_is_in_the_runtime_dir() {
        assert_eq!(
            marker_path().file_name().and_then(|n| n.to_str()),
            Some("stopped-by-user")
        );
        assert_eq!(
            marker_path().parent(),
            crate::daemon::socket_path().unwrap().parent()
        );
    }
}
