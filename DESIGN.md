# gnome-portal-bridge - design & contract (v0.1, experimental)

First-party GNOME/Wayland bridge for Claude Desktop's Computer Use on Linux. It
replaces the shell-out chain on GNOME Wayland sessions - `ydotool` (+`ydotoold`)
for input, and the `python3`+GStreamer portal script / `gnome-screenshot`+`convert`
/ `gdbus` cascade for screenshots - with one bundled binary, and ADDS best-effort
window enumeration (the JS executor has none on GNOME today).

It is the GNOME sibling of `kwin-portal-bridge` (KDE/Wayland) and `x11-bridge`
(X11 / XWayland) and speaks the **same JSON contract**, so the JS executor treats
all three interchangeably.

- **Invoked by:** `js/executor_linux.js` in `claude-desktop-bin`, via
  `execFile(bin, [subcommand, ...args])`, reading `JSON.parse(stdout.trim())`.
  The binary name resolves from `$GNOME_PORTAL_BRIDGE_BIN`, else
  `globalThis.__cuGnomeBridgeBin` (set in `cu_mode_preamble.js`), else the app
  `resourcesPath`, else `PATH` - the same pattern the x11 bridge uses with
  `X11_BRIDGE_BIN` / `globalThis.__cuX11BridgeBin`. (JS wiring is a separate
  workstream.)
- **Output:** exactly one JSON object/array on stdout. Errors: a human message on
  stderr and **exit code 1**. clap parse errors exit 2.

## Engine

Input and capture both flow through ONE combined XDG **RemoteDesktop +
ScreenCast** portal session (via the permissively-licensed `lamco-portal` /
`lamco-pipewire` crates - the same engine `kwin-portal-bridge` uses, but pulled
in directly rather than copied):

- **Input** = RemoteDesktop `notify_*` calls: `notify_pointer_motion_absolute`
  (stream-local coords), `notify_pointer_button` (evdev codes: left 272, right
  273, middle 274), `notify_pointer_axis` (scroll), and `notify_keyboard_keysym`
  (keys, incl. Unicode-plane keysyms for arbitrary characters).
- **Capture** = one PipeWire `VideoFrame` per screenshot from the session's
  ScreenCast stream, converted BGRx/BGRA/RGBx/RGBA -> RGB, downscaled to the CU
  caps and JPEG-encoded.

## Daemon vs ephemeral (dual mode)

`session-start` spawns a detached **daemon** holding one live portal session for
the whole tool-use lock, listening on a unix socket. Every other subcommand
connects and proxies through it, so the consent grant / PipeWire stream / input
capability are reused with no re-prompt. `session-end` stops the daemon.

If no daemon is running, a one-shot subcommand falls back to an **ephemeral**
session: open a portal session (dialog-free when a restore token exists), do the
action, tear down. Both modes work; the daemon just avoids repeated setup within
a lock.

**Socket:** `$XDG_RUNTIME_DIR/gnome-portal-bridge/session.sock`. Following the
kwin-portal-bridge precedent, it is **not** suffixed with `CLAUDE_PROFILE`: the
daemon is app-owned and spawned per-CU-session, with one active lock per running
app instance, so a fixed per-user path is correct. Profile isolation for
app-owned resources comes from per-profile state inherited via env, not from
suffixing this socket (per the claude-desktop-bin profile rule).

## Consent dialog & persistence

- **GNOME 46+**: the portal returns a **restore token**, persisted under
  `$XDG_STATE_HOME/gnome-portal-bridge/restore-token.json` (falling back to
  `~/.local/state/...`). After the first grant, later `session-start`s reuse it
  with **no dialog**.
- **GNOME 43-45**: the xdg-desktop-portal-gnome RemoteDesktop persistence backend
  (upstream MR #88, created April 2023) is not reliably present until GNOME 46, so
  a combined RemoteDesktop+ScreenCast session gets no restore token and a consent
  dialog appears **once per `session-start`** (accepted by design). Persistence
  rejection ("cannot persist" / "InvalidArgument") is detected and the session is
  retried without persistence.

**Token store is separate from the legacy JS path.** The executor's old inline
`python3`+GStreamer screenshot path persists its own token at
`~/.config/Claude/pipewire-restore-token` (`CLAUDE_PORTAL_TOKEN_PATH`). This
bridge deliberately uses its **own** store under `$XDG_STATE_HOME` and does not
read or write the legacy file - a clean separation, so enabling the bridge does
not disturb (or depend on) the old path's grant.

## Coordinate system

The JS sends **global (compositor logical) pixel** coordinates. The portal's
absolute pointer motion is **stream-local**, so a global point `(x, y)` is mapped
into the target screen's stream by:

```
local = ((global - screen.origin) / screen.logical_size) * stream.size
```

The target screen is the one whose geometry contains the point (else the default
screen). Screens are resolved on the client side (where the Mutter D-Bus
connection lives) and the resolved `Screen` is embedded in the request so the
daemon needs no D-Bus of its own.

## Subcommand -> JSON output contract

Field-name casing is **not uniform** and must be preserved exactly (this matches
the kwin/x11 bridges and the JS reader): screenshots/app-refs are camelCase;
screens/windows are snake_case.

| Subcommand | Output shape |
|---|---|
| `doctor` | `{xdg_current_desktop, xdg_session_type, portal_present, remote_desktop_version, screen_cast_version, gnome_shell_version, introspect_available, restore_token_present, restore_token_dir, daemon_running}` (human-facing; not parsed by JS) |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary}]` |
| `screenshot [--display <id>]` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY}` (camelCase) |
| `zoom --display <id> --x --y --w --h` | `{base64, width, height}` |
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
| `session-end` | `{ok:true, ended:true}` |

The scroll axis convention matches x11-bridge: **positive `dy` scrolls down**.
Drag interpolates ~20 steps at ~8ms each.

### `--display` selection

`--display <name>` matches a screen's `id`/`name`, which is the Mutter connector
name (`eDP-1`, `DP-2`). The JS passes `display._bridgeId` (the `id` field from
`screens`). Default (no flag): the active/primary screen, else the first.

## Screens & windows

- **Screens** come from `org.gnome.Mutter.DisplayConfig.GetCurrentState` (a public
  session-bus API). Logical geometry = current physical mode size / scale; scale
  can be fractional. Fallback when DisplayConfig is unavailable: derive screens
  from the portal ScreenCast streams (`id`/`name` = `stream-<node>`).
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

## glibc floor

Built as a **glibc-dynamic** binary (it links `libpipewire-0.3` via
`lamco-pipewire`), not static-musl like x11-bridge. The floor is **glibc 2.35**
(Ubuntu 22.04 jammy), the oldest base that ships a new-enough
`libpipewire-0.3-dev` (0.3.48 >= the 0.3.33 `lamco-pipewire` requires) while
still covering Debian 12 bookworm (2.36 / GNOME 43). CI builds inside a jammy
container and enforces the floor with `objdump -T | grep GLIBC_`.
