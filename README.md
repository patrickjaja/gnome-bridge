# gnome-portal-bridge

A first-party Rust bridge that makes Anthropic's Computer Use tooling work on
**GNOME under Wayland**. Paired with `claude-desktop-extra`, it replaces the
fragile shell-out chain the JS executor uses on GNOME today with a single
bundled binary, and adds best-effort window enumeration that GNOME otherwise
lacks.

It is the GNOME sibling of [`kwin-portal-bridge`](https://github.com/mosi0815/kwin-portal-bridge)
(KDE/Wayland) and [`x11-bridge`](https://github.com/patrickjaja/x11-bridge)
(X11 / XWayland), and speaks the **same kebab-case CLI + JSON contract**, so the
Claude Desktop executor can pick a backend per session without changing how it
parses results.

## Status: experimental

This talks to GNOME through the XDG desktop portal (**RemoteDesktop +
ScreenCast** via PipeWire) for input and screen capture, plus GNOME's public
D-Bus APIs (`org.gnome.Mutter.DisplayConfig` for monitors,
`org.gnome.Shell.Introspect` for windows). The portal runtime paths cannot be
exercised on a non-GNOME machine, so treat the capture/input paths as unverified
until run on a real GNOME Wayland session (see `scripts/manual-test-gnome.md`).

## What it replaces

| Concern | Old (shell-out) | New (this bridge) |
|---|---|---|
| Input | `ydotool` (+`ydotoold`, else x11-bridge over XWayland) | portal RemoteDesktop `notify_*` |
| Screenshot | 4-tier cascade (below) | one portal ScreenCast PipeWire frame -> JPEG |
| Windows | *(none on GNOME)* | `org.gnome.Shell.Introspect` (best-effort) |
| Screens | *(Electron only)* | `org.gnome.Mutter.DisplayConfig` |

The screenshot cascade in `js/cu_linux_executor.js` this replaces, in order:
(1) an inline `python3`+GStreamer script driving the ScreenCast portal
(persisting a token at `~/.config/Claude/pipewire-restore-token`) with a
`pipewiresrc ! videoconvert ! pngenc` pipeline; (2) `gnome-screenshot` +
ImageMagick `convert -crop`; (3) `gdbus` -> `org.gnome.Shell.Screenshot`;
(4) the portal path again on first run without a token. This bridge collapses
all four into a single persisted-token portal capture. It keeps its **own**
restore-token store (`$XDG_STATE_HOME/gnome-portal-bridge/`) separate from the
legacy python path's token file.

## Requirements

- A **GNOME Wayland** session (GNOME 42+; persistence needs 46+).
- `xdg-desktop-portal` + `xdg-desktop-portal-gnome` and **PipeWire** running.
- For window enumeration: the caller must be on GNOME Shell's Introspect
  allowlist or `org.gnome.mutter unsafe-mode` on. Otherwise `windows` returns
  `[]` (no regression - GNOME has no window list today).

## Consent dialog

The first session shows the portal's "Remote Desktop" consent dialog; leave
"Remember This Selection" on and turn on "Allow Remote Interaction". With
RemoteDesktop portal version 2 (**GNOME 46+**) the grant comes back as a
restore token, persisted under `$XDG_STATE_HOME/gnome-portal-bridge/`, so every
later session - daemon or one-shot - is **dialog-free**. `doctor` reports
`restore_token_present`. On RemoteDesktop v1 (**GNOME <= 45**, no portal
persistence) the dialog appears once per session, which is accepted by design.
Persistence is requested through `RemoteDesktop.SelectDevices` only; the portal
refuses it on `ScreenCast.SelectSources` for a remote-desktop session (see
DESIGN.md).

## Usage

```sh
# Diagnostics: DE, portal versions, GNOME Shell version, Introspect availability.
gnome-portal-bridge doctor

# Start a session daemon for a tool-use lock (one consent), then act, then end.
gnome-portal-bridge session-start
gnome-portal-bridge screenshot --display eDP-1
gnome-portal-bridge type --text "hello"
gnome-portal-bridge key-sequence --keys ctrl+s
gnome-portal-bridge pointer-click --x 400 --y 300 --button left --count 1
gnome-portal-bridge session-end
```

Every subcommand is short-lived and prints one JSON object/array on stdout.
Without a running daemon, one-shot commands open an ephemeral portal session,
act, and tear down. See [DESIGN.md](DESIGN.md) for the full CLI/JSON contract.

## Binary resolution (for the JS executor)

The executor resolves the binary from, in order:
`$GNOME_PORTAL_BRIDGE_BIN` -> `globalThis.__cuGnomeBridgeBin` -> the app
`resourcesPath` -> `PATH` (mirroring the x11 bridge's
`X11_BRIDGE_BIN` / `globalThis.__cuX11BridgeBin`).

## Siblings

| Backend | Session | Repo |
|---|---|---|
| `x11-bridge` | X11 / XWayland | pure Rust, static musl |
| `kwin-portal-bridge` | KDE Plasma / Wayland | portal + PipeWire |
| **`gnome-portal-bridge`** | **GNOME / Wayland** | **portal + PipeWire (this repo)** |
| `wlroots-bridge` | Sway / Hyprland / Niri | *in development* (`grim` + `ydotool`) |

## Building

```sh
# Native (needs clang + libpipewire-0.3-dev + pkg-config).
cargo build --release

# Reproducible cross-build (x86_64 + aarch64) mirroring CI, in one Docker run.
./scripts/pipeline-build-local.sh
```

The binary links `libpipewire-0.3` dynamically (glibc-dynamic, not static). The
floor is **glibc 2.39 + PipeWire 1.0.5** (Ubuntu 24.04 noble): `lamco-pipewire`
calls `pw_stream_get_nsec` / `pw_stream_get_time_n` and the extended `pw_time`
fields, which older PipeWire (Ubuntu 22.04's 0.3.48, Debian 12's 0.3.65) neither
compiles against nor provides at runtime - Rust links BIND_NOW, so the binary
fails at startup there. Supported: Ubuntu 24.04+, Fedora 40+, Debian 13+, Arch.
CI builds inside noble and verifies the floor via `objdump`.

## License

MIT. See [LICENSE](LICENSE). The portal/PipeWire engine is the
permissively-licensed (`MIT OR Apache-2.0`) `lamco-portal` and `lamco-pipewire`
crates, used directly.
