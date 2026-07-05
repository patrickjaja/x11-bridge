# x11-bridge - design & contract (v0.1, experimental)

First-party X11 bridge for Claude Desktop's Computer Use on Linux. It replaces
the shell-out chain (`xdotool` / `scrot` / ImageMagick `import` /
`gnome-screenshot` / `wmctrl`) on **X11 sessions** with a single pure-Rust,
statically-linkable binary. It is the X11 sibling of `kwin-portal-bridge`
(KDE/Wayland) and speaks the **same JSON contract** so the JS executor treats
them interchangeably.

- **Invoked by:** `js/executor_linux.js` in `claude-desktop-bin`, via
  `execFile(bin, [subcommand, ...args])`, reading `JSON.parse(stdout.trim())`.
  The binary name resolves from `$KWIN_PORTAL_BRIDGE_BIN` /
  `globalThis.__cuKwinBridgeBin` / `"kwin-portal-bridge"` today; wiring the X11
  bridge in is a follow-up on the JS side (out of scope for this project).
- **One-shot:** every subcommand is a short-lived process. No daemon, no
  persistent socket (see [No-daemon rationale](#no-daemon-rationale)).
- **Output:** exactly one JSON object/array on stdout. Errors: a human message
  on stderr and **exit code 1**. clap parse errors exit 2.

## Coordinate system

All coordinates are **X11 root-window (global) pixels** - the same space the JS
sends (`Math.round(x)`, `Math.round(y)`). A multi-monitor setup is one virtual
screen; a monitor's `geometry.x/y` is its offset within it. **Scale is 1.0** for
v0.1: X11 has no first-class per-monitor logical scale, and Xft.dpi-based
scaling is deferred (the JS `scaleFactor` defaults to 1 when `scale` is absent
or non-finite, so 1.0 is safe). Region/zoom math therefore uses raw pixels with
no scale conversion in v0.1.

## Subcommand → JSON output contract

Shapes are copied from `kwin-portal-bridge` (`src/model.rs`, `src/capture.rs`,
`src/kwin.rs`) and verified against the JS reader in `js/executor_linux.js`.
Field-name casing is **not uniform** and must be preserved exactly:
screenshots/app-refs are camelCase; screens/windows are snake_case (that is how
the kwin bridge emits them and how the JS reads them).

| Subcommand | Output shape | JS reader (evidence) |
|---|---|---|
| `doctor` | `{display, server_vendor, protocol_major, protocol_minor, screen_count, extensions:{xtest,randr,xfixes}, keyboard:{min_keycode,max_keycode}}` | human-facing only; not parsed by JS |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary}]` | `mapRustScreenToDisplay` reads `geometry.*`, `scale`, `is_primary`, `name`, `id` (→ `_bridgeId`) |
| `screenshot [--display N]` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY}` (camelCase) | `screenshotResultFromRust` reads `result.displayWidth`/`displayHeight`/`originX`/`originY` |
| `zoom --x --y --w --h [--display N]` | `{base64, width, height}` | `zoom` reads `result.base64`/`width`/`height` |
| `windows` | `[{id, title, geometry, pid, desktop_file_name, resource_class, resource_name, window_role, window_type, is_dock, is_desktop, is_visible, is_minimized, is_normal_window, is_dialog, transient, transient_for, output, stacking_order, is_active, exclude_from_capture, keep_above}]` (snake_case) | `normalizeBundleIdFromWindow`, `dockableWindow`, `findHostMainWindow` read `desktop_file_name`, `resource_class`, `resource_name`, `is_minimized`, `is_visible`, `is_dialog`, `is_normal_window`, `is_active`, `stacking_order`, `output`, `keep_above`, `transient`, `transient_for` |
| `cursor-position` | `{x, y}` | `getCursorPosition` reads `result.x`/`result.y` |
| `frontmost-app` | `{bundleId, displayName}` (camelCase) | `commandOutputToAppRef` reads `result.bundleId`/`displayName` |
| `app-under-point --x --y` | `{bundleId, displayName}` | same as above |
| `activate-window --window <id>` | `{activated: <id>}` (or similar ack object) | `activateWindow` ignores the body (fire-and-forget) |
| `pointer-move --x --y` | `{action, x, y}` (camelCase) | body ignored |
| `pointer-click --x --y [--button] [--count] [--modifier ...]` | `{action, x, y, raised?}` | body ignored |
| `pointer-scroll --x --y [--dx] [--dy]` | `{action, x, y}` | body ignored |
| `pointer-drag --from-x --from-y --to-x --to-y` | `{action, fromX, fromY, toX, toY}` | body ignored |
| `left-mouse-down` / `left-mouse-up` | `{action, button, isHeld}` | body ignored |
| `key-sequence --keys <spec> [--repeat N]` | `{action, keys, repeat?}` | body ignored |
| `type --text <t> [--delay-ms N]` | `{action, text, charCount}` | body ignored |
| `hold-key --key <k>... --duration-ms N` | `{action, keys, durationMs}` | body ignored |
| `session-start [--foreground]` | `{ok:true, session:"noop"}` | `execBridge` tolerates any/empty output |
| `session-end` | `{ok:true, ended:true}` | same |

Many action subcommands have their JSON body ignored by the JS (it only cares
about exit code / stderr), but they still emit a structured object for parity
with the kwin bridge and for direct CLI/debugging use.

### `--display` selection

`--display <name>` matches a monitor's `name`/`id` (e.g. `eDP-1`, `DP-2`). The
JS passes `display._bridgeId`, which is the `id` field from `screens` - for the
X11 bridge that is the RandR monitor name. Default (no flag): the RandR
**primary** monitor, else the first. (Note: some single-head setups report no
RandR primary, which would leave `is_primary`/`is_active` `false` on every
entry. When there is exactly one monitor the `screens` mapping treats it as
primary and active - forcing both flags true - so `--display` selection and the
JS `isPrimary` display picker both resolve it unambiguously. With two or more
monitors and no RandR primary, the flags stay driven by RandR and the JS falls
back to `displays[0]`, which is correct.)

## Key-spec grammar

Accepted by `key-sequence` (and each `--key` of `hold-key`). Mirrors the JS
`combo.split("+").map(_mapKey)` in `cu_linux_executor.js`:

```
spec     := token ("+" token)*
token    := modifier | keyname
modifier := ctrl | control | alt | shift | super | meta | cmd | command
keyname  := <CU key-name from table> | <single literal char>
```

- Case-insensitive. The base key is the last non-modifier token.
- **Modifier aliases:** `cmd`, `command`, `meta` all normalize to **`super`**;
  `control` → `ctrl`.
- **CU key-name table** (CU token → X11 keysym name), extracted verbatim from
  `_mapKey`: `enter`/`return`→`Return`, `backspace`→`BackSpace`,
  `delete`→`Delete`, `escape`/`esc`→`Escape`, `tab`→`Tab`, `space`/` `→`space`,
  `up`/`down`/`left`/`right`→`Up`/`Down`/`Left`/`Right`,
  `home`/`end`→`Home`/`End`, `pageup`/`page_up`→`Prior`,
  `pagedown`/`page_down`→`Next`, `capslock`→`Caps_Lock`. Anything else (letters,
  digits, punctuation) passes through and is resolved as a keysym by char/name.
- CU emits both `page_down`/`page_up` and `pagedown`/`pageup`; the parser
  accepts both (normalize by stripping `_`). Examples the parser must accept:
  `return`, `escape`, `cmd+a`, `ctrl+shift+tab`, `page_down`.

The authoritative starting table lives in `src/input/keymap.rs`
(`CU_KEY_NAMES`, `MODIFIER_TOKENS`).

## Error contract

- Success: one JSON value on stdout, exit 0.
- Failure: `anyhow` error chain printed to stderr (`{error:#}`), exit 1.
- Unknown/invalid CLI: clap message on stderr, exit 2.
- The JS `execBridge` surfaces stderr (or stdout, or `error.message`) as the
  thrown error detail, and special-cases `/already active/i` for session start.

## No-daemon rationale

`kwin-portal-bridge` runs a long-lived `session-start` daemon because Wayland's
`RemoteDesktop`/`ScreenCast` portal grants (and the PipeWire stream + input
capability) must stay alive for the whole tool-use lock - tearing them down
between actions would re-prompt the user and drop the stream.

X11 has no such session object. XTEST input synthesis and `GetImage` capture are
**stateless, per-request** operations against the root window that need no
prior grant. Button/key hold state (`left-mouse-down` … `left-mouse-up`) is
held by the **X server**, not our process, so it survives across separate
short-lived invocations. Therefore `session-start`/`session-end` are no-ops that
just report `{ok:true}` to satisfy the JS session bookkeeping, and every other
subcommand opens a fresh `RustConnection`, does its work, and exits.

## Explicitly out of scope for v0.1

- **`open-app` / `list-installed-apps` / `get-app-icon`** - the JS executor
  already resolves and launches `.desktop` entries portably in-process
  (`createRustLinuxBackend` calls these on the kwin bridge, but the fallback
  `globalThis.__linuxExecutor` implements them directly in JS). No X11-specific
  behavior needed; deferred.
- **`read-clipboard` / `write-clipboard`** - in regular (non-Cowork) mode the
  Electron `clipboard` API is used directly by the JS; an X11 clipboard
  (`XA_CLIPBOARD` selection owner) path is deferred.
- **Teach overlays** (`teach-*`, `session-overlay`, `set-overlay-display`) and
  **capture-exclusion / window-geometry control**
  (`set-exclude`, `set-window-geometry`, `set-window-keep-above`,
  `prepare-for-action`, `resolve-prepare-capture`, `preview-hide-set`,
  `raise-allowed-at-point`) - these are KWin-specific niceties
  (`excludeFromCapture`, compositor-driven docking, layer-shell overlays) with
  no portable X11 equivalent. Future work; not part of the core CU loop.

## Future backends

This binary is the X11 member of a family. `kwin-portal-bridge` covers
KDE/Wayland; `x11-bridge` covers X11 and XWayland fallback. GNOME-Wayland and
generic wlroots-Wayland siblings (portal+PipeWire / `grim`+`ydotool`) may follow
using the same JSON contract, so the JS executor can pick a backend per session
without changing its parsing.

## Implementation notes for wave-2

- **`conn.rs` is done.** It connects (`x11rb::connect`), exposes `root`, and
  enumerates RandR monitors (`randr_get_monitors`, name atoms resolved via
  `get_atom_name`). `resolve_monitor(Option<&str>)` implements the
  default-primary / by-name selection above.
- **capture** - `xproto::get_image(ZPixmap, root, mx, my, mw, mh, !0)` over the
  monitor rect; convert to RGB; downscale to the caps (`MAX_LONG_EDGE=1568`,
  `MAX_PIXELS=1_150_000` - same as kwin); JPEG-encode (quality 75, matching the
  kwin bridge) with the `image` crate; base64 inline. `zoom` crops the region
  first.
- **input** - XTEST (`xtest_fake_input`): motion (absolute root), button
  press/release (buttons 1/3/2; scroll 4/5/6/7), key press/release. `keymap`
  resolves keysyms and finds keycodes from the server keyboard mapping
  (temporarily rebinding a spare keycode for off-layout glyphs when typing).
- **wm** - EWMH/ICCCM root + per-window properties: `_NET_CLIENT_LIST_STACKING`
  (stacking order, bottom→top), `_NET_ACTIVE_WINDOW` (frontmost), `WM_CLASS`
  (`resource_name` = instance, `resource_class` = class), `_NET_WM_NAME` (UTF-8,
  falling back to `WM_NAME`), `_NET_WM_PID`, `_NET_WM_WINDOW_TYPE` (mapped to
  `window_type` + the `is_dock`/`is_desktop`/`is_dialog`/`is_normal_window`
  flags), `_NET_WM_STATE` (`_NET_WM_STATE_HIDDEN` → `is_minimized`,
  `_NET_WM_STATE_ABOVE` → `keep_above`), `WM_TRANSIENT_FOR` (the `transient` /
  `transient_for` chain), `WM_WINDOW_ROLE`, and `_KDE_NET_WM_DESKTOP_FILE` /
  `_GTK_APPLICATION_ID` (→ `desktop_file_name`). Geometry is
  `get_geometry` + `translate_coordinates` to root coordinates; `output` is the
  RandR monitor whose rect contains the window center. Activation sends a
  `_NET_ACTIVE_WINDOW` client message to root (source indication 2) plus a
  `ConfigureWindow` raise fallback for non-EWMH WMs.
  - **`id` format:** the X11 window id as a **decimal** string (e.g.
    `"92274692"`). `activate-window --window <id>` parses **both** decimal and
    `0x`-prefixed hex, so an id copied from `xwininfo`/`xprop` also works.
  - **`bundle_id` derivation** (`frontmost-app` / `app-under-point`): mirrors the
    JS `normalizeBundleIdFromWindow` - first non-empty of `desktop_file_name`,
    `resource_class`, `resource_name`, then the numeric id, with a trailing
    `.desktop` stripped. `display_name` is the window title, else the bundle id.
  - **`exclude_from_capture` is always `false`** - X11 has no per-window
    capture-exclusion (that is a KWin compositor feature); the field exists only
    for JSON parity with the kwin bridge.
  - **EWMH-absent fallback:** on bare X / Xvfb / minimal WMs there is no
    `_NET_CLIENT_LIST_STACKING`. `windows` then falls back to a `query_tree` of
    the root's children, keeping mapped, non-override-redirect top-levels that
    carry a `WM_CLASS`; `frontmost-app` falls back to `GetInputFocus`;
    `app-under-point` falls back to a `translate_coordinates` child walk. Every
    optional property that is absent maps to `None`/`false` rather than an error,
    so read-only commands never crash on a WM-less display (they return an empty
    list or a graceful `exit 1`).
- Fill the stub bodies and remove the crate-level `#![allow(dead_code)]` in
  `main.rs` once every scaffold item has a caller. **Do not** change signatures
  in `main.rs`/`cli.rs`.
