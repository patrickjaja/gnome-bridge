# gnome-portal-bridge - design & contract (v0.1, experimental)

First-party GNOME/Wayland bridge for Claude Desktop's Computer Use on Linux. It
replaces the shell-out chain on GNOME Wayland sessions - `ydotool` (+`ydotoold`)
for input, and the `python3`+GStreamer portal script / `gnome-screenshot`+`convert`
/ `gdbus` cascade for screenshots - with one bundled binary, and ADDS best-effort
window enumeration (the JS executor has none on GNOME today).

It is the GNOME sibling of `kwin-portal-bridge` (KDE/Wayland) and `x11-bridge`
(X11 / XWayland) and speaks the **same JSON contract**, so the JS executor treats
all three interchangeably.

- **Invoked by:** `js/cu_linux_executor.js` in `claude-desktop-extra`, via
  `execFile(bin, [subcommand, ...args])`, reading `JSON.parse(stdout.trim())`.
  The binary name resolves from `$GNOME_PORTAL_BRIDGE_BIN`, else
  `globalThis.__cuGnomeBridgeBin` (set in `cu_mode_preamble.js`), else the app
  `resourcesPath`, else `PATH` - the same pattern the x11 bridge uses with
  `X11_BRIDGE_BIN` / `globalThis.__cuX11BridgeBin`. (JS wiring is a separate
  workstream.)
- **Output:** exactly one JSON object/array on stdout. Errors: a human message on
  stderr and **exit code 1** (3: monitor not shared, 4: no live portal
  session, see [CLI contract v1](#cli-contract-v1)). clap parse errors exit 2.

## Engine

Input and capture both flow through ONE combined XDG **RemoteDesktop +
ScreenCast** portal session (via the permissively-licensed `lamco-portal` /
`lamco-pipewire` crates - the same engine `kwin-portal-bridge` uses, but pulled
in directly rather than copied):

- **Input** = RemoteDesktop `notify_*` calls: `notify_pointer_motion_absolute`
  (stream video pixels, see "Coordinate system"), `notify_pointer_button` (evdev codes: left 272, right
  273, middle 274), `notify_pointer_axis` (scroll), and `notify_keyboard_keysym`
  (keys, incl. Unicode-plane keysyms for arbitrary characters).
- **Capture** = the latest PipeWire `VideoFrame` of the session's ScreenCast
  stream, converted BGRx/BGRA/RGBx/RGBA -> RGB, downscaled to the CU caps and
  JPEG-encoded. `lamco-pipewire` queues frames in a bounded channel that drops
  the NEWEST frame when full, so popping one frame per screenshot returns an
  ever older frame under activity (and holds up to 256 full frames). A drainer
  thread therefore owns the PipeWire thread manager, empties that queue
  continuously and keeps only the latest frame per stream. A screenshot
  collects new frames until none arrives for 70 ms (or 300 ms passed) and uses
  the last one: the action's own cursor motion often produces a cursor-only
  frame before the application repainted, so the first new frame is not
  enough. Mutter emits frames on damage or cursor motion only; on a static
  screen none comes and the stored frame (already current) is used. Mutter
  also records a stream at most once per refresh interval: damage inside that
  interval is held back for a "follow-up" redraw of the monitor's view, and
  with an embedded cursor nothing else records the stream. That follow-up can
  miss the view (GNOME 51, second of two monitors), leaving the damage pending
  until the next change on that monitor - a click whose repaint came a few ms
  after its cursor frame then stays invisible. So the first capture of the
  screen the pointer was last sent to, after pointer input (motion or button),
  moves the pointer 1 px (toward the screen's middle) and back, which damages
  the view under the cursor, and settles again (at most 250 ms); the frame
  Mutter then records includes the pending damage. Keyboard input does not
  arm it (a pointer move over an open menu or popup list would move the
  selection to the item under the pointer), nor does a capture of another
  monitor; it never runs while a button is held or before the session placed
  the pointer. The frames themselves arrive 10-20 ms after Mutter
  queues them, in sequence: the PipeWire path drops nothing. A fresh
  stream waits up to 10 s for its first frame. The drainer ends with the
  session (`session-end`, daemon exit); if the PipeWire thread dies, the
  drainer notices (an early empty poll = disconnected channel), stops, and the
  next capture asks the portal for a new PipeWire remote on the live session
  (`OpenPipeWireRemote`; a dup of the old fd would share the dead
  connection). If that fails the capture errors and the daemon exits, so the
  next `session-start` opens a fresh session.

## Daemon vs ephemeral (dual mode)

`session-start` spawns a detached **daemon** holding one live portal session for
the whole tool-use lock, listening on a unix socket. Every other subcommand
connects and proxies through it, so the consent grant / PipeWire stream / input
capability are reused with no re-prompt. `session-end` stops the daemon: it
asks over the socket (1.5 s), then makes sure the recorded pid (`session.pid`
next to the socket) is gone - a daemon that answered gets 2 s to tear down,
else SIGKILL; one that did not answer gets SIGTERM at once, SIGKILL after
2 s. Worst case about 4 s, inside the JS exit hook's 5 s. A daemon that
outlives `session-end` keeps the portal session, and with it the compositor's
screen-sharing indicator, alive. The pid is validated by its cmdline and then
pinned with a pidfd (checked again after opening), so a reused pid is never
signalled; a zombie counts as gone. The daemon bounds its own teardown
(1.8 s) and every client read/write (10 s), so a stopped client cannot block
it.

The daemon serves one request at a time, so a request can wait behind a slow
one after its client gave up. Input requests (pointer, keys, typing; not
`left-mouse-up`, releasing is always safe) therefore carry the client's
deadline as a CLOCK_MONOTONIC timestamp, and the daemon refuses them,
unexecuted, once it passed or when the client hung up (`poll` POLLHUP; the
client's write half-close after its request is only RDHUP). A bare request
without a deadline (older client) is still served. On a client timeout the
error says an input action will not start anymore but may have run. A `session-start` that finds a daemon process
still coming up (its consent dialog open) waits for that one instead of
spawning a second daemon and a second dialog. `session-start` waits up to
120 s for a human to answer the dialog; on timeout the daemon keeps waiting
and the error says so (`still waiting for the portal consent dialog`), while
a daemon that died says `exited before becoming ready`. That error is only
for a daemon this `session-start` spawned (consent declined, portal failed):
a daemon that already existed and exits while being waited on was ending
anyway (its session was lost, or a `session-end` raced), so `session-start`
spawns a fresh one instead. A leaving daemon (Shutdown, lost session,
SIGTERM) removes its socket and pidfile before its reply and before the
teardown, and only while they are still its own (socket inode, pid in the
pidfile), so a `session-start` right after an exit 4 never waits on it.

If no daemon is running, a one-shot subcommand falls back to an **ephemeral**
session: open a portal session (dialog-free when a restore token exists), do the
action, tear down. Both modes work; the daemon just avoids repeated setup within
a lock. "No daemon" means no socket, or a stale socket nobody listens on
(connect refused). A daemon that accepts the connection but fails, reports an
error or does not answer in time (25 s, plus the sleep of `type`/`hold-key`;
below the JS executor's 30 s) returns that error, with the `daemon.log` path
for transport failures, and never falls back: a second portal session would
mean a dialog per action and may share a different monitor than the daemon's.

**Socket:** `$XDG_RUNTIME_DIR/gnome-portal-bridge/session.sock`. Following the
kwin-portal-bridge precedent, it is **not** suffixed with `CLAUDE_PROFILE`: the
daemon is app-owned and spawned per-CU-session, with one active lock per running
app instance, so a fixed per-user path is correct. Profile isolation for
app-owned resources comes from per-profile state inherited via env, not from
suffixing this socket (per the claude-desktop-extra profile rule).

## Consent dialog & persistence

Persistence of a COMBINED RemoteDesktop + ScreenCast session is owned by the
RemoteDesktop interface alone (xdg-desktop-portal, ScreenCast.SelectSources:
"Setting persist_mode is only allowed for screen cast sessions. Persistent
remote desktop screen cast sessions can only be handled via the Remote Desktop
interface."). The bridge therefore builds the session itself with `ashpd`:

1. `RemoteDesktop.CreateSession`
2. `RemoteDesktop.SelectDevices` with `persist_mode = 2` (until explicitly
   revoked) and the saved `restore_token`, if any - **RemoteDesktop portal
   version >= 2 only** (GNOME 46+); on v1 neither option is sent.
3. `ScreenCast.SelectSources` with **no** persistence options (the portal
   rejects them on a remote-desktop session with `InvalidArgument`).
4. `RemoteDesktop.Start` - the consent dialog, unless the token is accepted.
   Its response carries the new restore token.
5. `ScreenCast.OpenPipeWireRemote`.

`lamco-portal`'s own `create_session` sends the persistence options to BOTH
select calls, so step 3 fails and the only session it can produce is one
without persistence - which is why `PortalManager::create_session` is not used
(it still provides the `remote_desktop()` proxy the `notify_*` input calls go
through).

- **RemoteDesktop v2 (GNOME 46+)**: the token is persisted under
  `$XDG_STATE_HOME/gnome-portal-bridge/restore-token.json` (falling back to
  `~/.local/state/...`). After the first grant, later sessions start with
  **no dialog**. Each `Start` returns a fresh token (they are single-use) which
  replaces the stored one.
- **RemoteDesktop v1 (GNOME <= 45)**: no persistence API; a consent dialog
  appears **once per session** (accepted by design).
- A backend that advertises v2 but still refuses the persistent request is
  retried once without persistence, with the portal's reason on stderr.

**Token store is separate from the legacy JS path.** The executor's old inline
`python3`+GStreamer screenshot path persists its own token at
`~/.config/Claude/pipewire-restore-token` (`CLAUDE_PORTAL_TOKEN_PATH`). This
bridge deliberately uses its **own** store under `$XDG_STATE_HOME` and does not
read or write the legacy file - a clean separation, so enabling the bridge does
not disturb (or depend on) the old path's grant.

## Coordinate system

The JS sends **global coordinates in Mutter's layout space** (logical pixels in
the default logical layout mode). `NotifyPointerMotionAbsolute(stream, x, y)`
expects `x, y` in the stream's **video pixels**: Mutter maps it back as
`global = stream.position + (x, y) / scale` (scale 1 in physical layout mode).
The portal reports a stream's `position`/`size` in layout units, while its
PipeWire video is `round(size * scale)` pixels, so a global point is mapped by:

```
video_px = (global - screen.origin) * video_size / stream.size
```

For a screen that came from Mutter, `video_size / stream.size` is exactly
`Screen.scale`, the layout-unit -> stream-pixel factor: the logical monitor's
scale in logical layout mode, 1.0 in physical layout mode (`layout-mode` = 2
in `GetCurrentState`, where `geometry` is reported in physical pixels). That
is the factor Mutter's `transform_position` divides by, so the pointer path
uses it directly and never needs a capture stream. Only a stream-derived
screen (no Mutter; `source: stream` on the `Screen`) learns the factor from
the negotiated video size of its PipeWire stream, creating the consumer if no
screenshot did yet (no portal call, so no dialog). Example: eDP-1 at (0,240), 1920x1200 logical, scale 1.3333,
video 2560x1600; global (900,800) is sent as (1200, 746.67).

**Unreachable outer band.** xdg-desktop-portal (1.20, `check_position` in
`remote-desktop.c`) accepts an absolute position only if `0 <= x < size.w` and
`0 <= y < size.h`, with `size` the stream's portal `size` property, which Mutter
reports in LOGICAL units; Mutter then reads the same numbers as video pixels
and divides by the scale. On a scaled monitor every target whose video-pixel
point falls outside `size` is refused with `Invalid position` - on a 1.25
monitor (logical 2048x1280, video 2560x1600) that is local x >= 1638 or
y >= 1024. Such a target is reached in two ordered calls: absolute motion to
the clamped point `A_s = min(P, size - 1)` (Mutter puts the cursor at logical
`origin + A_s / factor`), then relative `NotifyPointerMotion` by the logical
remainder `target - A`. Mutter applies a virtual device's relative motion in
logical units without pointer acceleration. This applies to every pointer
action (move, click, scroll, each drag step). At scale 1 and in physical
layout mode the point is always inside `size`, so nothing is split. Related
upstream discussion of the coordinate-space ambiguity:
https://github.com/flatpak/xdg-desktop-portal/issues/1976.

The target screen is the one whose geometry contains the point (else the default
screen). Its stream is the one whose `position` equals the screen's origin.
There is **no fallback to another stream**: a monitor that was not shared in
the consent dialog fails with exit 3 and `monitor <id> (<x>,<y> <w>x<h>) is not
shared in the portal session (shared: <id> at <x>,<y> <w>x<h>, ...)`; the
remedy is the caller's to add. A plain
`session-end` + `session-start` would replay the stored restore token, i.e.
the same monitor selection; `forget-session` ends the daemon and deletes the
token so the next session shows the dialog. The one exception is a single-head setup (exactly one stream and
exactly one Mutter monitor), where the stream is used even if its origin
differs. The restore token is never cleared automatically.

Screens are resolved on the client side (where the Mutter D-Bus connection
lives) and the resolved `Screen` is embedded in the request; the daemon asks
Mutter itself only on a stream mismatch, to allow the single-head case and to
name the shared monitors in the error.

## Subcommand -> JSON output contract

Field-name casing is **not uniform** and must be preserved exactly (this matches
the kwin/x11 bridges and the JS reader): screenshots/app-refs are camelCase;
screens/windows are snake_case.

| Subcommand | Output shape |
|---|---|
| `doctor` | contract v1 keys `{contract:1, bridge, version, backend:"gnome-portal", session_match, backend_reachable, backend_error, session_supported:true, restore_token_present, restore_token_path, daemon_running, session_socket}`, then extras `{xdg_current_desktop, xdg_session_type, portal_present, remote_desktop_version, screen_cast_version, mutter_present, gnome_shell_version, introspect_available, restore_token_dir (deprecated), daemon_log}` |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary, is_shared?}]`; `is_shared` is present only while a session daemon answers: true iff the daemon's portal session has a stream at that screen's origin (not read by JS) |
| `screenshot [--display <id>]` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY}` (camelCase) |
| `zoom [--display <id>] --x --y --w --h` | `{base64, width, height}`; with `--display` the region is display-relative, without it the region is in GLOBAL coordinates and the display under its origin is used; the region is clamped to the display |
| `windows` | `[{id, title, geometry, pid, desktop_file_name, resource_class, resource_name, window_role, window_type, is_dock, is_desktop, is_visible, is_minimized, is_normal_window, is_dialog, transient, transient_for, output, stacking_order, is_active, exclude_from_capture, keep_above}]` (snake_case) - **best-effort, may be `[]`** |
| `frontmost-app` | `{bundleId, displayName}` (camelCase) or `null` |
| `app-under-point --x --y` | `{bundleId, displayName}` or `null` |
| `cursor-position` | **not supported** (exit 1) |
| `activate-window --window <id>` | **not supported** (exit 1) |
| `pointer-move --x --y` | `{action, x, y}` (body ignored by JS) |
| `pointer-click --x --y [--button] [--count] [--modifier ...]` | `{action, x, y}` |
| `pointer-scroll --x --y [--dx] [--dy]` | `{action, x, y}` |
| `pointer-drag --from-x --from-y --to-x --to-y` | `{action, fromX, fromY, toX, toY}` |
| `left-mouse-down` / `left-mouse-up` | `{action, button, isHeld}` |
| `key-sequence --keys <spec> [--repeat N]` | `{action, keys, repeat?}` |
| `type --text <t> [--delay-ms N]` | `{action, text, charCount}` |
| `hold-key --key <k>... --duration-ms N` | `{action, keys, durationMs}` |
| `session-start [--foreground]` | `{ok:true, session:"daemon"}` |
| `session-end` | `{ok:true, ended, daemon}`: `ended` only when a daemon was there; `daemon` is `exited`, `sigterm`, `sigkill`, `shutdown-requested` or `none` |
| `forget-session` | `{ok:true, ended, daemon, token_removed, token_path}`: ends any daemon and deletes the restore token, so the next session shows the consent dialog (to pick other monitors); creates no directory, calls no D-Bus |

The scroll axis convention matches x11-bridge: **positive `dy` scrolls down**.
Drag interpolates ~20 steps at ~8ms each.

### `--display` selection

`--display <name>` matches a screen's `id`/`name`, which is the Mutter connector
name (`eDP-1`, `DP-2`). The JS passes `display._bridgeId` (the `id` field from
`screens`). Default (no flag): while a session daemon runs, the shared
active/primary screen, else the first shared screen (the Mutter primary may be
a monitor the user did not share); without a daemon, or when nothing is
shared, the active/primary screen, else the first. An explicit `--display` is
always honored (and errors if that monitor is not shared).

## CLI contract v1

This bridge implements the Computer Use bridge CLI contract v1
(https://github.com/patrickjaja/claude-desktop-extra/blob/master/docs/computer-use-bridge-contract.md), the interface claude-desktop-extra's launcher and executors use for
every bridge:

- `doctor` leads with the contract keys (table above) and exits 0 even when
  the session bus is unreachable. `session_match` is the executor's own GNOME
  Wayland test (`XDG_SESSION_TYPE=wayland` or `WAYLAND_DISPLAY`, and
  `XDG_CURRENT_DESKTOP` containing `GNOME`); `backend_reachable` means the
  session bus, the desktop portal (a RemoteDesktop/ScreenCast `version`
  property read) and Mutter's DisplayConfig (`Peer.Ping`) answered, each probe
  bounded to 1.5 s. It never opens a portal session and creates nothing.
- Exit codes: 0 ok, 1 runtime error, 2 bad usage, 3 the monitor is not shared
  in the live portal session, 4 no live portal session (the message contains
  `no active portal session`). The kind is attached where the error starts
  (`src/error_kind.rs`) and crosses the daemon socket as `error_kind`
  (`not_shared`, `no_session`) next to the error text, so the client never
  parses text. A reply without `error_kind` (a daemon from before the upgrade
  still running) or with an unknown kind exits 1.
- Files: restore token `${XDG_STATE_HOME:-$HOME/.local/state}/gnome-portal-bridge/restore-token.json`,
  session socket `${XDG_RUNTIME_DIR:-/tmp}/gnome-portal-bridge/session.sock`
  (an empty variable counts as unset).
- `tests/contract.rs` checks the contract against the built binary in a clean
  environment.

Not covered by exit 4: with no daemon at all, action commands still fall back
to an ephemeral portal session (see "Daemon vs ephemeral"), and a portal
session the user stopped from the GNOME panel is not detected as such.

## Screens & windows

- **Screens** come from `org.gnome.Mutter.DisplayConfig.GetCurrentState` (a public
  session-bus API). Geometry is in layout units: in logical layout mode the
  current physical mode size / scale (scale can be fractional), in physical
  layout mode the mode size itself with `scale` 1.0. The decoder's Rust types are pinned to Mutter's published
  signature by a unit test: zbus refuses the whole reply on any mismatch, which
  does not look like a wrong field but like "DisplayConfig unavailable".
  Fallbacks when DisplayConfig really is unavailable, in order: the running
  session daemon's ScreenCast streams (no new portal session), else, for
  capture and pointer commands only, a short ephemeral portal session
  (`id`/`name` = `stream-<node>`). The `screens` command never opens a portal
  session (contract v1): without Mutter and without a daemon it exits 1
  (`cannot list monitors without opening a portal session`). Every fallback
  is announced on stderr.
- **Windows** come from `org.gnome.Shell.Introspect.GetWindows`. This API is
  **allowlist-restricted**: it works only when the caller is whitelisted or
  `org.gnome.mutter unsafe-mode` is on. When restricted it returns an error, and
  we degrade gracefully to `[]` / `null` - which is today's GNOME status quo (no
  window list), so it is never a regression. `doctor` reports
  `introspect_available` so you can tell which case you are in.

## Deviations from the x11/kwin contract (with rationale)

- **`cursor-position`: not supported.** There is no portable GNOME/Wayland global
  cursor query; the JS uses Electron's `getCursorScreenPoint` instead.
- **`activate-window`: not supported.** GNOME Shell Introspect exposes no
  activation. The executor's open-app path launches apps itself, so a clean
  not-supported error is correct for v0.1.
- **`windows` is best-effort** (see above) and window `geometry` often lacks a
  global origin on GNOME, so `app-under-point` can only resolve a hit when
  frame-rect data is present, else returns `null`.
- **No `--modifier` on scroll/drag** - matches the x11/kwin surface (only click
  takes modifiers).

## Out of scope for v0.1

Clipboard (`read-clipboard`/`write-clipboard`), app management
(`open-app`/`list-installed-apps`/`get-app-icon` - the JS resolves `.desktop`
entries in-process), teach overlays, and capture-exclusion / window-geometry
control. Generic wlroots-Wayland (`grim`+`ydotool`) is a future sibling using the
same contract; the portal+PipeWire engine here is reusable for it.

## Key-spec grammar

Accepted by `key-sequence` (and each `--key` of `hold-key`). Identical to the
x11-bridge grammar (extracted from `_mapKey` in `cu_linux_executor.js`):

```
spec     := token ("+" token)*
token    := modifier | keyname
modifier := ctrl | control | alt | shift | super | meta | cmd | command
keyname  := <CU key-name> | <single literal char>
```

- Case-insensitive; base key is the last non-modifier token.
- Modifier aliases: `cmd`/`command`/`meta` -> `super`; `control` -> `ctrl`.
- CU key-names (token -> X keysym name): `enter`/`return`->Return,
  `backspace`->BackSpace, `delete`->Delete, `escape`/`esc`->Escape, `tab`->Tab,
  `space`/` `->space, arrows, `home`/`end`, `pageup`/`page_up`->Prior,
  `pagedown`/`page_down`->Next, `capslock`->Caps_Lock. Anything else resolves as
  a keysym by char/name.
- On GNOME, keys are injected via the portal `notify_keyboard_keysym` (a keysym
  value), so there is no keycode lookup or spare-keycode remapping (unlike the
  X11 bridge). Arbitrary characters map to their keysym, using the Unicode-plane
  encoding (`0x01000000 | codepoint`) for glyphs with no named keysym.

## glibc + PipeWire floor

Built as a **glibc-dynamic** binary (it links `libpipewire-0.3` via
`lamco-pipewire`), not static-musl like x11-bridge. The floor is **glibc 2.39 +
PipeWire 1.0.5** (Ubuntu 24.04 noble). `lamco-pipewire` 0.4.x declares a
0.3.33 pkg-config minimum, but that metadata is wrong: it calls
`pw_stream_get_nsec` (PipeWire >= 1.0.x), `pw_stream_get_time_n` and the
extended `pw_time` fields (>= 0.3.50), and `libspa` 0.9.x needs the
`spa_video_info_raw` layout with `flags`/u64 `modifier` (>= 0.3.65). Older
bases fail at compile (bindgen sees the old structs) AND at runtime (Rust
links BIND_NOW; the missing `.so` symbols abort startup) - so Ubuntu 22.04
(0.3.48) and Debian 12 (0.3.65) cannot be supported with this dependency.
CI builds inside a noble container and enforces the floor with
`objdump -T | grep GLIBC_`.
