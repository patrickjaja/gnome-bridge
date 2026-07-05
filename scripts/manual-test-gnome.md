# Manual verification on a GNOME Wayland session

The portal runtime paths (consent dialog, input injection, PipeWire capture)
**cannot** be exercised on a non-GNOME machine. Run these steps on a real GNOME
Wayland session - the Ubuntu VirtualBox VM (GNOME) is the designated target.

Prerequisites in the guest:

- A GNOME **Wayland** login session (check: `echo $XDG_SESSION_TYPE` -> `wayland`,
  `echo $XDG_CURRENT_DESKTOP` -> contains `GNOME`).
- `xdg-desktop-portal` + `xdg-desktop-portal-gnome` and PipeWire running.
- The built binary copied in (e.g. `./dist/gnome-portal-bridge` from
  `scripts/pipeline-build-local.sh`, or a native `cargo build --release`).

```sh
BIN=./gnome-portal-bridge   # adjust path
```

## 1. Diagnostics (no consent needed)

```sh
"$BIN" doctor
```

Expect JSON with `xdg_current_desktop` containing `GNOME`, `xdg_session_type` =
`wayland`, `portal_present: true`, non-null `remote_desktop_version` and
`screen_cast_version`, a `gnome_shell_version`, and `daemon_running: false`.
`introspect_available` tells you whether `windows` will return data.

```sh
"$BIN" screens
```

Expect one entry per monitor with `geometry`, a `scale` (fractional allowed),
`is_primary`, and the connector `id`/`name` (e.g. `eDP-1`).

## 2. Start a session (consent dialog)

```sh
"$BIN" session-start
```

On the **first** run GNOME shows a screen-share / remote-control consent dialog.
Accept it. Expect `{"ok":true,"session":"daemon"}`. On GNOME 46+ a restore token
is now saved (`doctor` -> `restore_token_present: true`); re-running
`session-start` after `session-end` should NOT prompt again.

## 3. Screenshot

```sh
"$BIN" screenshot --display eDP-1 | python3 -c '
import sys, json, base64
d = json.load(sys.stdin)
img = base64.b64decode(d["base64"])
open("/tmp/gpb-shot.jpg","wb").write(img)
print("size", d["width"], "x", d["height"], "display", d["displayWidth"], d["displayHeight"])
'
xdg-open /tmp/gpb-shot.jpg   # eyeball it: should be the real screen contents
```

Also try `zoom --display eDP-1 --x 0 --y 0 --w 400 --h 300`.

## 4. Type into an app

Open **gedit** (or any text editor), click into it, then:

```sh
"$BIN" pointer-move --x 700 --y 400
"$BIN" type --text "hello from gnome-portal-bridge"
"$BIN" key-sequence --keys ctrl+a
"$BIN" key-sequence --keys Delete
"$BIN" type --text "unicode: café 😀"
```

The text should appear in the editor; the ctrl+a / Delete should select-and-clear.
Verify the emoji / accented characters render (Unicode-plane keysym path).

## 5. Pointer + scroll + drag

```sh
"$BIN" pointer-click --x 700 --y 400 --button left --count 1
"$BIN" pointer-scroll --x 700 --y 400 --dy 15     # positive dy scrolls DOWN
"$BIN" pointer-drag --from-x 700 --from-y 400 --to-x 900 --to-y 500
```

## 6. Windows (only if allowlisted)

```sh
"$BIN" windows
"$BIN" frontmost-app
```

If `doctor` reported `introspect_available: false`, `windows` returns `[]` and
`frontmost-app` returns `null` - that is expected (the API is allowlist-gated).
To exercise it: `gsettings set org.gnome.mutter unsafe-mode true` (temporarily),
re-run, then unset it.

## 7. End the session

```sh
"$BIN" session-end     # -> {"ok":true,"ended":true}
"$BIN" doctor          # daemon_running should be false again
```

## Not supported on GNOME (should error cleanly, exit 1)

```sh
"$BIN" cursor-position    # JS uses Electron's getCursorScreenPoint
"$BIN" activate-window --window 123   # Introspect exposes no activation
```
