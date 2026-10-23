//! Computer Use bridge CLI contract v1, black-box against the built binary
//! (claude-desktop-extra `docs/computer-use-bridge-contract.md`).
//!
//! Every run gets a clean environment: a temp HOME / XDG_STATE_HOME /
//! XDG_RUNTIME_DIR, no display, and a session bus address that does not
//! exist, so nothing here can reach the real desktop, portal or daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_gnome-portal-bridge");
const BRIDGE: &str = "gnome-portal-bridge";

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "gpb-contract-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { root }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.state())
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
            .output()
            .expect("failed to run the bridge binary")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("terminated by a signal")
}

/// Exit 0 and exactly one JSON value on stdout.
fn json(output: &Output) -> Value {
    assert_eq!(
        code(output),
        0,
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(stdout.trim().lines().count(), 1, "stdout: {stdout}");
    serde_json::from_str(stdout.trim()).expect("stdout is not JSON")
}

fn is_string_or_null(value: &Value) -> bool {
    value.is_string() || value.is_null()
}

#[test]
fn version_is_bridge_and_semver() {
    let output = Sandbox::new("version").run(&["--version"]);
    assert_eq!(code(&output), 0);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (name, version) = stdout.trim().split_once(' ').expect("`<bridge> <semver>`");
    assert_eq!(name, BRIDGE);
    let parts: Vec<&str> = version.split('.').collect();
    assert_eq!(parts.len(), 3, "{version}");
    assert!(parts.iter().all(|p| p.parse::<u64>().is_ok()), "{version}");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn doctor_reports_every_required_key_without_a_bus() {
    let sandbox = Sandbox::new("doctor");
    let report = json(&sandbox.run(&["doctor"]));

    assert_eq!(report["contract"], 1);
    assert_eq!(report["bridge"], BRIDGE);
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["backend"], "gnome-portal");
    for key in [
        "session_match",
        "backend_reachable",
        "session_supported",
        "restore_token_present",
        "daemon_running",
    ] {
        assert!(report[key].is_boolean(), "{key}: {}", report[key]);
    }
    for key in ["backend_error", "restore_token_path", "session_socket"] {
        assert!(report.get(key).is_some_and(is_string_or_null), "{key}");
    }

    // No display, no bus: not this session, backend unreachable with a reason.
    assert_eq!(report["session_match"], false);
    assert_eq!(report["backend_reachable"], false);
    assert!(
        report["backend_error"]
            .as_str()
            .is_some_and(|e| !e.is_empty())
    );
    assert_eq!(report["session_supported"], true);
    assert_eq!(report["restore_token_present"], false);
    assert_eq!(report["daemon_running"], false);

    // Contract file layout.
    assert_eq!(
        Path::new(report["restore_token_path"].as_str().unwrap()),
        sandbox
            .state()
            .join("gnome-portal-bridge/restore-token.json")
    );
    assert_eq!(
        Path::new(report["session_socket"].as_str().unwrap()),
        sandbox.runtime().join("gnome-portal-bridge/session.sock")
    );

    // doctor creates nothing.
    assert!(!sandbox.state().exists());
    assert!(!sandbox.runtime().exists());
}

#[test]
fn doctor_sees_a_planted_token() {
    let sandbox = Sandbox::new("doctor-token");
    let dir = sandbox.state().join("gnome-portal-bridge");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("restore-token.json"), r#"{"token":"t"}"#).unwrap();
    let report = json(&sandbox.run(&["doctor"]));
    assert_eq!(report["restore_token_present"], true);
}

#[test]
fn session_end_without_a_daemon_ends_nothing() {
    let sandbox = Sandbox::new("end");
    let result = json(&sandbox.run(&["session-end"]));
    assert_eq!(result["ok"], true);
    assert_eq!(result["ended"], false);
    assert_eq!(result["daemon"], "none");
    assert!(!sandbox.runtime().exists());
}

#[test]
fn forget_session_creates_nothing_and_removes_only_the_token() {
    let sandbox = Sandbox::new("forget");
    let token_path = sandbox
        .state()
        .join("gnome-portal-bridge/restore-token.json");

    // Nothing to forget: idempotent, and no state directory appears.
    let result = json(&sandbox.run(&["forget-session"]));
    assert_eq!(result["ok"], true);
    assert_eq!(result["ended"], false);
    assert_eq!(result["daemon"], "none");
    assert_eq!(result["token_removed"], false);
    assert_eq!(
        Path::new(result["token_path"].as_str().unwrap()),
        token_path
    );
    assert!(
        !sandbox.state().exists(),
        "forget-session created the state dir"
    );
    assert!(!sandbox.runtime().exists());

    // A planted token goes; a sibling file stays.
    let dir = token_path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&token_path, r#"{"token":"t"}"#).unwrap();
    let sibling = dir.join("keep-me");
    std::fs::write(&sibling, "x").unwrap();
    let result = json(&sandbox.run(&["forget-session"]));
    assert_eq!(result["token_removed"], true);
    assert!(!token_path.exists());
    assert!(sibling.exists());

    // And again: nothing left to remove.
    let result = json(&sandbox.run(&["forget-session"]));
    assert_eq!(result["token_removed"], false);
}

#[test]
fn screens_without_mutter_or_daemon_fails_without_a_portal_session() {
    let sandbox = Sandbox::new("screens");
    let output = sandbox.run(&["screens"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(code(&output), 1, "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(
        stderr.contains("cannot list monitors without opening a portal session"),
        "{stderr}"
    );
    // No ephemeral session (it would create the token store) and no daemon.
    assert!(!sandbox.state().exists(), "a portal session was attempted");
    assert!(!sandbox.runtime().exists(), "a daemon was started");
}

#[test]
fn listing_and_session_commands_have_help() {
    let sandbox = Sandbox::new("help");
    for command in [
        "screens",
        "windows",
        "session-start",
        "session-end",
        "forget-session",
    ] {
        let output = sandbox.run(&[command, "--help"]);
        assert_eq!(code(&output), 0, "{command} --help");
    }
}

#[test]
fn coordinate_flags_accept_negative_values() {
    let sandbox = Sandbox::new("negative");
    let cases: &[&[&str]] = &[
        &["pointer-move", "--x", "-5", "--y", "-5"],
        &["pointer-click", "--x", "-5", "--y", "-5"],
        &["pointer-scroll", "--x", "-5", "--y", "-5", "--dy", "-3"],
        &[
            "pointer-drag",
            "--from-x",
            "-5",
            "--from-y",
            "-5",
            "--to-x",
            "-10",
            "--to-y",
            "-10",
        ],
        &["zoom", "--x", "-5", "--y", "-5", "--w", "10", "--h", "10"],
        &["app-under-point", "--x", "-5", "--y", "-5"],
    ];
    for args in cases {
        // Parsing succeeds; the command itself fails later (no bus).
        let output = sandbox.run(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(code(&output), 2, "{args:?}: {stderr}");
        assert!(
            !stderr.contains("unexpected argument"),
            "{args:?}: {stderr}"
        );
        // And `--help` after negative values parses too.
        let mut with_help = args.to_vec();
        with_help.push("--help");
        assert_eq!(code(&sandbox.run(&with_help)), 0, "{with_help:?}");
    }
}

#[test]
fn bad_usage_exits_2() {
    let sandbox = Sandbox::new("usage");
    assert_eq!(code(&sandbox.run(&["bogus-subcommand"])), 2);
    assert_eq!(code(&sandbox.run(&["doctor", "--bogus-flag"])), 2);
}
