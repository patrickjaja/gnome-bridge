# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 2026-10-23

### Fixed

- **`screens` never reached Mutter.** The `GetCurrentState` decoder was one
  field short of Mutter's `(siiddada{sv})` mode signature (the preferred-scale
  double), and zbus rejects the whole reply on a signature mismatch, so every
  call silently fell back to deriving screens from a fresh portal session - a
  consent dialog per `screens`, and because pointer and capture commands
  resolve their screen the same way, a dialog per click and per zoom as well,
  even with the session daemon up. Screens now come from Mutter in
  milliseconds with the connector name as `id` (`eDP-1`, not `stream-116`); a
  unit test pins the decoder to Mutter's published signature. The remaining
  fallbacks (the running daemon's streams, then an ephemeral session) announce
  themselves on stderr (claude-desktop-extra #293).
- **No restore token was ever stored.** Persistence of a combined
  RemoteDesktop + ScreenCast session may only be requested through
  `RemoteDesktop.SelectDevices`; the portal rejects `persist_mode` on
  `ScreenCast.SelectSources` for a remote-desktop session, and `lamco-portal`
  sends it to both. The rejection was being treated as "this GNOME cannot
  persist" and the session retried without persistence - on every GNOME. The
  bridge now builds the session itself with `ashpd` (persistence on
  `SelectDevices` only, RemoteDesktop portal v2+), stores the token the `Start`
  response returns, and rotates it on every session. Verified on GNOME 48: the
  second `session-start`, and every one-shot command, runs with no dialog.
- **`zoom` without `--display` takes global coordinates.** That is what the
  executor sends when it has no monitor list; the bridge used to apply them to
  the primary display and refuse the region on any monitor not at the origin.
  The display under the region's origin is used and the region is clamped to
  it; only a region entirely outside a display is an error.
- **`session-end` makes sure the daemon is gone.** The daemon records its pid
  next to the socket; `session-end` asks it to stop, waits, then sends SIGTERM
  and SIGKILL. The daemon also bounds its own teardown and hard-exits. A
  daemon that outlived `session-end` kept the portal session - and GNOME's
  screen-sharing indicator - alive.
- **A client that goes away mid-request no longer ends the daemon.** The
  executor kills a bridge subprocess on timeout; the half-written request that
  left behind used to error the serve loop and take the whole session down.
- `session-start` finding a daemon that is still waiting on its consent dialog
  waits for it instead of spawning a second daemon (and a second dialog), and
  gives up early when the daemon exits (dialog declined). Liveness probes are
  bounded, so `doctor`, `session-start` and `session-end` cannot hang behind
  an open dialog.

### Added

- The detached daemon's stderr is written to
  `$XDG_RUNTIME_DIR/gnome-portal-bridge/daemon.log` (truncated per start);
  `doctor` prints the path as `daemon_log`.

## 2026-07-06

### Changed

- Build base raised from `ubuntu:jammy` to `ubuntu:noble`; the floor is now
  **glibc 2.39 + PipeWire 1.0.5** (Ubuntu 24.04+, Fedora 40+, Debian 13+).
  `lamco-pipewire` 0.4.x declares a 0.3.33 pkg-config minimum but actually
  calls `pw_stream_get_nsec` / `pw_stream_get_time_n` and the extended
  `pw_time` fields (PipeWire >= 1.0.x), and `libspa` 0.9.x needs the
  post-0.3.65 `spa_video_info_raw` layout - so the previously documented
  jammy floor (Ubuntu 22.04 / Debian 12) never compiled and could never have
  run (Rust links BIND_NOW; the missing symbols abort startup). CI, the local
  pipeline script, README, and DESIGN were updated; manifest deps bumped to
  the locked `lamco-portal` 0.4.2 / `lamco-pipewire` 0.4.5.

### Added

- Initial v0.1.0 of `gnome-portal-bridge`, the first-party GNOME/Wayland backend
  for Claude Desktop's Computer Use on Linux. A single bundled binary that
  replaces the shell-out chain on GNOME (`ydotool` for input; the
  `python3`+GStreamer portal script / `gnome-screenshot`+`convert` / `gdbus`
  screenshot cascade) and ADDS best-effort window enumeration that GNOME
  otherwise lacks. It is the GNOME sibling of `kwin-portal-bridge` (KDE/Wayland)
  and `x11-bridge` (X11) and speaks the same kebab-case CLI + JSON contract, so
  the Claude Desktop executor can treat all three interchangeably.
- Input and capture through ONE combined XDG RemoteDesktop + ScreenCast portal
  session via the permissively-licensed `lamco-portal` / `lamco-pipewire` crates:
  pointer move/click/scroll/drag, `left-mouse-down`/`-up` held state,
  `key-sequence` / `type` / `hold-key` (keysym injection, incl. Unicode-plane
  keysyms), and PipeWire-frame screenshots/zoom (BGRx/BGRA/RGBx/RGBA -> RGB,
  downscaled to 1568 long edge / 1.15M px, JPEG quality 75 - matching
  `kwin-portal-bridge`).
- A session daemon (`session-start [--foreground]` / `session-end`) holding one
  portal grant for the whole tool-use lock over a unix socket in
  `$XDG_RUNTIME_DIR/gnome-portal-bridge/`, with all other subcommands proxying
  through it; plus an ephemeral one-shot fallback when no daemon is running.
- Restore-token persistence under `$XDG_STATE_HOME/gnome-portal-bridge/` so
  GNOME 46+ users see the consent dialog only once (GNOME 43-45 has no
  persistence -> dialog per `session-start`, by design).
- `screens` via `org.gnome.Mutter.DisplayConfig.GetCurrentState` (fractional
  scale, logical geometry), with a portal-stream fallback; `windows` /
  `frontmost-app` / `app-under-point` via `org.gnome.Shell.Introspect`, degrading
  to `[]` / `null` when the allowlist-restricted API is unavailable.
- `doctor` reporting the desktop environment, portal presence + RemoteDesktop /
  ScreenCast versions, GNOME Shell version, Introspect availability, restore-token
  presence, and whether a daemon is running.
- Documented not-supported subcommands for v0.1: `cursor-position` (JS uses
  Electron's `getCursorScreenPoint`) and `activate-window` (Introspect has no
  activation).
- Unit tests: key-spec parsing, char->keysym (incl. Unicode plane), screenshot
  downscale caps, `screens` mapping from canned DisplayConfig data, and
  window/app-ref bundle-id derivation.
- CI (`fmt` + `clippy -D warnings`; build + test on x86_64 and aarch64 inside a
  jammy container; a GLIBC-floor assertion via `objdump`) and a tag-driven draft
  GitHub Release publishing per-arch `.tar.gz` with `.sha256` and a combined
  `SHA256SUMS`. `scripts/pipeline-build-local.sh` reproduces the cross-build in
  one Docker run.

### Notes

- Built glibc-dynamic (links `libpipewire-0.3`), floor **glibc 2.35** (Ubuntu
  22.04 jammy) - the oldest base with a new-enough libpipewire that still covers
  Debian 12 bookworm / GNOME 43.
- Portal runtime paths (consent, input injection, capture) are **unverified** on
  the development machine (X11, no GNOME); see `scripts/manual-test-gnome.md` for
  the GNOME verification steps. Wiring the binary into
  `claude-desktop-extra`'s `js/executor_linux.js` is a separate follow-up.
