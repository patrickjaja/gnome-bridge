# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
