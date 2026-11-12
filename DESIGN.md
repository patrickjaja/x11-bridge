# x11-bridge - design & contract (v0.1, experimental)

First-party X11 bridge for Claude Desktop's Computer Use on Linux. It replaces
the shell-out chain (`xdotool` / `scrot` / ImageMagick `import` /
`gnome-screenshot` / `wmctrl`) on **X11 sessions** with a single pure-Rust,
statically-linkable binary. It is the X11 sibling of `kwin-portal-bridge`
(KDE/Wayland) and speaks the **same JSON contract** so the JS executor treats
them interchangeably.

- **Invoked by:** `js/cu_linux_executor.js` in `claude-desktop-extra`, via
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
`src/kwin.rs`) and verified against the JS reader in `js/cu_linux_executor.js`.
Field-name casing is **not uniform** and must be preserved exactly:
screenshots/app-refs are camelCase; screens/windows are snake_case (that is how
the kwin bridge emits them and how the JS reads them).

| Subcommand | Output shape | JS reader (evidence) |
|---|---|---|
| `doctor` | `{display, server_vendor, protocol_major, protocol_minor, screen_count, extensions:{xtest,randr,xfixes}, keyboard:{min_keycode,max_keycode}}` | human-facing only; not parsed by JS |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary}]` | `mapRustScreenToDisplay` reads `geometry.*`, `scale`, `is_primary`, `name`, `id` (→ `_bridgeId`) |
| `screenshot [--display N] [--keep-window ID]... [--mask-all] [--ignore-window ID]...` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY, mask?}` (camelCase) | `screenshotResultFromRust` reads `result.displayWidth`/`displayHeight`/`originX`/`originY` |
| `zoom --x --y --w --h [--display N] [--keep-window ID]... [--mask-all] [--ignore-window ID]...` | `{base64, width, height, mask?}` | `zoom` reads `result.base64`/`width`/`height` |
| `windows` | `[{id, title, geometry, pid, desktop_file_name, resource_class, resource_name, window_role, window_type, is_dock, is_desktop, is_visible, is_minimized, is_normal_window, is_dialog, transient, transient_for, output, stacking_order, is_active, exclude_from_capture, keep_above}]` (snake_case) | `normalizeBundleIdFromWindow`, `dockableWindow`, `findHostMainWindow` read `desktop_file_name`, `resource_class`, `resource_name`, `is_minimized`, `is_visible`, `is_dialog`, `is_normal_window`, `is_active`, `stacking_order`, `output`, `keep_above`, `transient`, `transient_for` |
| `cursor-position` | `{x, y}` | `getCursorPosition` reads `result.x`/`result.y` |
| `frontmost-app` | `{bundleId, displayName}` (camelCase), or JSON `null` when no window is active | `_appRefFromCommand` reads `result.bundleId`/`displayName` and maps anything else to no app |
| `app-under-point --x --y` | `{bundleId, displayName}` (camelCase; `bundleId` is the client's decimal window id when it names no app), or JSON `null` when no window is at the point | same as above |
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
| `abort-watch` | long-running, JSON lines (see [Escape abort](#escape-abort-abort-watch)) | `js/cu_esc_watch.js` |

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

## Mask mode (allowlist-filtered capture)

`screenshot` and `zoom` take `--keep-window <id>` (repeatable; an id from
`windows`, decimal or `0x`-hex) or `--mask-all` (mask mode with an empty
allowlist). The two conflict (exit 2). Without either flag the capture and its
output are unchanged. `doctor` reports `capabilities.screenshot_mask: true`
(additive; contract stays 1, a missing key means no support).

Every pixel that is not part of a kept window's visible region is painted
`#1E1E1E`: the desktop, panels, notifications, windows that appear later,
override-redirect windows of other apps and Claude's own window are masked
unless kept. A masked window stays a solid rectangle where it is.

Algorithm (`src/mask.rs`):

1. Grab the server; the window tree and the `GetImage` pixels are one snapshot.
2. `QueryTree(root)` gives the root children in the server's real stacking
   order. This decides which window owns a root pixel, so it is used instead of
   `_NET_CLIENT_LIST_STACKING` (no override-redirect windows; absent on xfwm4).
   Each viewable `InputOutput` child is a layer with its outer geometry (border
   included). On a reparenting WM the child is the frame, so decorations belong
   to their window; `_NET_FRAME_EXTENTS` is not needed. A client with
   `_GTK_FRAME_EXTENTS` (CSD) is reduced to its visible part (no shadow margin).
3. Each kept id is walked up to its root child. Ids that do not exist or are not
   viewable are reported in `kept_missing` and masked.
4. Also kept: transients (`WM_TRANSIENT_FOR`) of a kept window, and
   override-redirect windows (menus, tooltips, dropdowns) stacked above a kept
   window with the same `WM_CLIENT_LEADER`, or the same `_NET_WM_PID` and a
   `WM_CLASS` class that does not differ.
5. Painter's algorithm: a pixel is kept only when its topmost window is kept. A
   kept window covered by a masked window is masked there (`GetImage` only has
   the top pixels).

Fail closed: any error reading the tree exits 1 with no image. A virtual-root
WM (`__SWM_VROOT`) is refused.

Output adds `mask`:
`{mode: "keep"|"all", fill: "#1E1E1E", kept: [id], kept_hidden: [id],
kept_missing: [id], auto_kept: [id], masked: [{id, frame, pid,
desktop_file_name, resource_class, resource_name, override_redirect}],
ignored: [id], ignored_missing: [id], ignored_refused: [{id, reason, pid?,
caller_pid?}]}`.
`kept` = requested ids with visible pixels in the image; `kept_hidden` =
requested, viewable, but no visible pixel (covered, or outside the region);
`auto_kept` = windows kept through ownership; `masked` = masked windows with
pixels in the image, topmost first, by client id (as in `windows`). Titles of
masked windows are never reported.

Limits: geometry is rectangular, so a kept shaped window (non-rectangular
`SHAPE` region) shows what is under its cut-out corners, and a kept ARGB window
with a transparent background shows what is behind it, as composited. Under a
compositing manager the pixels are the compositor's last frame: a window that
moved or closed within that frame, or a running effect (animation, wobbly
windows, desktop cube), can leave stale pixels in a kept region. A compositor
that paints into its own full-screen override-redirect window instead of the
Composite overlay masks everything.

### Ignored windows (`--ignore-window <id>`)

Claude's Computer Use glow ("Claude is using your computer") and its teach
overlay are full-display, transparent, click-through Electron windows kept
above everything (Electron creates them override-redirect: `focusable:false`).
Upstream hides them from capture with `setContentProtection`, which does
nothing on X11, so without help they own every pixel and a kept window under
them is reported in `kept_hidden` with a fully masked image.

`--ignore-window <id>` (repeatable; ids as for `--keep-window`, walked up to
the root child; mask mode only, a no-op otherwise) names such an overlay. An
ignored layer owns no pixel: what is under it belongs to the next layer down,
so a pixel shows only when the topmost non-ignored layer is kept. Ignoring
never widens what is kept. The pixels themselves are still `GetImage`'s, i.e.
the composited screen: over a kept window the image shows the overlay's rim or
card on top of the app, exactly as the user sees it (an overlay that is opaque,
e.g. the glow without a compositing manager, which Electron then draws black,
stays opaque in the image).

An id is honoured only when all of these hold, else it fails closed (an
ordinary layer, masked unless kept):

- it exists and is viewable (else `ignored_missing`);
- its root child is override-redirect: never a WM-managed window such as the
  caller's main window or a dialog (`not_override_redirect`);
- the root child's `_NET_WM_PID` is the bridge's parent pid (`getppid`; the app
  execs the bridge directly; Chromium sets `_NET_WM_PID` to its browser
  process on every X window) (`foreign_pid`, `no_pid`, `no_caller_pid` when
  the parent is gone);
- it is not kept, directly or through ownership: keep wins (`kept`).

So a wrong id from the caller can only ever let the caller's own
override-redirect pixels through, never another process's window. The input
shape is deliberately not checked: Electron's click-through leaves a 1x1 input
region rather than an empty one, and the teach overlay turns input back on
while the pointer is over its card.

`mask` adds `ignored: [id]`, `ignored_missing: [id]` and
`ignored_refused: [{id, reason, pid?, caller_pid?}]`; ignored windows are not
listed in `masked`. `doctor` reports `capabilities.screenshot_mask_ignore:
true`.

## Compositing manager (`doctor.capabilities.compositing`)

`doctor` reports whether a compositing manager runs on the bridge's screen
right now: `true` when the EWMH selection `_NET_WM_CM_S<screen>` has an owner,
`false` when it has none (bare X, i3, openbox, xfwm4 with its compositor off).
It is read on every `doctor` call (one InternAtom + one GetSelectionOwner), so
a compositor switched on or off at run time shows on the next call. When it
cannot be read (no X display, a request error) it is `null` with
`compositing_error`; `doctor` still exits 0.

Why it matters: without a compositing manager X has no alpha blending, so a
"transparent" window (Electron `transparent: true`, such as Claude's Computer
Use glow) is drawn opaque, typically black over the whole area. Consumers
should treat `false` and `null` alike as "no compositor": do not show
see-through overlays.

## Input shapes (`doctor.capabilities.input_shape`)

`doctor` reports `true` when the X server has the SHAPE extension at version
1.1 or later (input shapes), `false` without it, `null` without an X display.
With it, `app-under-point` skips windows whose input shape does not contain
the point (see the hit test under Implementation notes), so a click-through
overlay above everything does not answer for the windows below it. Without
it, every window counts in its full rectangle. Xorg and Xvfb have had SHAPE
1.1 since 2006.

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

## Typing and the keyboard layout

`type`, `key-sequence` and `hold-key` turn keysyms into key presses in two
steps (`src/input/layout.rs`, `src/input/remap.rs`):

1. **On the layout.** The XKB map and state (`XkbGetMap`, `XkbGetState`) say,
   for the effective group, which key and level produce a keysym. A level is
   chosen by the key type's modifier map with the locked modifiers (Caps Lock,
   Num Lock) included, and is selected by holding the layout's own Shift key
   and, for `type` only, its ISO_Level3_Shift (AltGr) key - both found as keys
   whose level-1 keysym is the modifier and which the modifier map binds.
   Preference: no modifier, Shift, AltGr, AltGr+Shift; lowest keycode within
   one choice. `key-sequence` and `hold-key` never use AltGr, so a shortcut
   never gains a modifier. Dead keys are never pressed: `^` on a German layout
   is `dead_circumflex`, a different keysym, so it takes step 2. Without XKB
   the core mapping is used (group 1, levels 1-2).
2. **Spare keycodes.** Characters no key of the active group produces are
   bound to keycodes with no keysym in any column (no physical key of the
   layout sends them; Xvfb's German layout has 15: 8, 97, 103, ...; with none
   the last keycode is borrowed). A text is split into pages of at most that
   many distinct characters; a page's characters are bound 50 ms
   (`BIND_WAIT_MS`) before its first key, and the keycodes change again only
   200 ms (`SETTLE_MS`) after the last
   key typed with them - for the next page or for the restore at the end. When
   keys go out faster than 4 ms each (`--delay-ms` below 4), the change also
   waits until a client handling one key per 4 ms (`CLIENT_KEY_MS`) could
   have read every key sent since the page was bound: a busy client's backlog
   otherwise still holds keys of the page (seen in GTK at `--delay-ms 0` on a
   loaded host: 3 of 15 long texts lost characters at page changes).

Why the pause: an X client translates a key event with the keymap it holds
when it processes the event, and fetches a changed keymap only when it
processes the notification of the change. A keycode restored right after its
key (what this bridge did until 2026-11-12, and what xdotool does) is read
with the restored map by every client that gets to the event after the
restore. Measured under Xvfb with GTK 3, urxvt and Chrome 157: every such
character was lost at the default 12 ms delay; with no delay the previous
character came out instead. GTK and urxvt needed 1 ms of pause, Chrome under
load more than 20 ms. Chrome also applies a new keymap a little after it reads
the notification: without the 50 ms before a page's first key, 3 of 12 long
Chrome texts had one character of a new page typed as the character its
keycode carried in the previous page (none of 12 with it). The pauses are
paid once per page (and once at the end), not per character: a text whose
off-layout characters fit one page costs 250 ms more in total, nothing per
character.

Which keycodes: Chrome under X11 takes the character from the keysym, so a
character on a keycode without a DOM code (8) arrives; a character bound to a
named key's keycode runs that key instead (measured: bound to BackSpace's
keycode 22, `é` deleted the previous character). Empty keycodes are never
named keys of the layout.

Safety: before the first change the original rows go to the root-window
property `_X11_BRIDGE_KEYMAP_RESTORE` (`[1, pid, keysyms_per_keycode,
(keycode, row...)...]`, CARDINAL/32). `type` stops between two characters on
SIGTERM / SIGINT / SIGHUP (Node's `execFileSync` timeout sends SIGTERM),
restores and exits 1 (`typing stopped by signal N after M characters`). After
a SIGKILL the next `type` / `key-sequence` / `hold-key` restores every
journaled keycode that still carries a bridge row (one keysym repeated) and
deletes the property; a journal of a bridge that is still running is left to
it. Modifier and character keys are pressed and released within one flush per
character, so no key stays down after a kill.

## Escape abort (`abort-watch`)

The one long-running subcommand: Claude Desktop spawns `x11-bridge abort-watch`
while Computer Use holds its lock, and the user's Escape stops the turn. The
wire protocol, `suspend` / `resume`, signals, parent-death handling and exit
codes come from the shared
[`cu-abort-watch`](https://github.com/patrickjaja/cu-abort-watch) crate (its
README is the contract); x11-bridge supplies the watcher:

```
{"event":"ready","mechanism":"xi2","syntheticSafe":true}   once armed
{"event":"abort"}                                           per physical Escape press
{"event":"error","message":"..."}                           arming failed (exit 2) / X connection lost (exit 1)
```

Mechanism `xi2`:

- Selects `XI_RawKeyPress`, `XI_RawKeyRelease` and `XI_HierarchyChanged` on the
  root window for `XIAllDevices`, after announcing XInput 2.2. Arming fails
  when the XInput extension is missing or the server speaks less than 2.1
  (from 2.1 on, raw events reach every selecting client even while another
  client holds a grab).
- Passive: nothing is grabbed and the key is not consumed, so the user's
  Escape also reaches the focused window. Nothing needs undoing at exit; the
  selection ends with the connection.
- The model's Escape is excluded by source device. `key-sequence` and every
  other input subcommand inject with XTEST, which the server routes through the
  master's `... XTEST keyboard` slave; raw events carry that slave as
  `sourceid`. Devices are classified by the server's `XTEST Device` property
  (name suffix ` XTEST keyboard` / ` XTEST pointer` when the property cannot be
  read) at arm and on every hierarchy change, so a new master's XTEST slaves
  are excluded too. Any other source (hardware keyboards, a newly plugged
  keyboard, a floating slave) is physical. Other XTEST clients (`xdotool`) are
  excluded the same way.
- With `XIAllDevices` every press arrives twice (slave copy, master copy);
  both carry the physical slave as `sourceid`, which is what events are keyed
  by, so the second copy reads as a repeat and a server sending only one copy
  still works. On Xvfb the slave copy arrived under a passive key grab and an
  active keyboard grab.
- One `abort` per press, erring toward reporting (a duplicate is harmless, a
  missed abort is not): a press flagged `KeyRepeat` never reports; a press of
  the key a device last pressed and has not released is a repeat only while
  that hold is under 2 s old (unflagged repeats keep it fresh). Any other key
  press on that device and every hierarchy event clear the hold, so a lost
  release (device disabled while held, VT switch, suspend) cannot swallow
  later presses.
- A device re-scan or keymap re-read that fails is retried twice (50 ms, then
  200 ms) before the watcher fails.
- Escape keycodes: every keycode with the Escape keysym in any column of the
  core keyboard mapping, re-read on each keyboard `MappingNotify` (also covers
  `caps:escape` style remaps). Per-device XKB keymaps (`setxkbmap -device`)
  are not consulted.
- `suspend` / `resume` keep the crate defaults (reporting muted while
  suspended); the shared reference count is not used (XInput selections are
  per client).
- Xwayland: `syntheticSafe` is `false` there. Xwayland started with
  `-enable-ei-portal` (GNOME, KDE) forwards XTEST through libei to the
  compositor, which may send the key back on the non-XTEST
  `xwayland-keyboard` device, where it would read as physical; the X side
  cannot tell. Xwayland also only sees keys while an X client has focus.
  Claude Desktop picks the watcher by host compositor, so `xi2` is the X11
  session mechanism, not the Wayland one.

`doctor` adds `"abortWatch": {"available": bool, "mechanism": "xi2",
"reason": string|null, "consumes": false|null}`: available when the X server is
reachable and speaks XInput >= 2.1, otherwise the reason. `consumes` says
whether arming takes Escape from applications: `false` (the XInput2 watcher is
passive), `null` when unavailable.

## Error contract

- Success: one JSON value on stdout, exit 0 (`abort-watch` excepted, see above).
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
  parses key specs to keysyms; `layout` finds their key and level in the
  active XKB group and `remap` binds the rest to spare keycodes (see
  [Typing and the keyboard layout](#typing-and-the-keyboard-layout)).
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
    `resource_class`, `resource_name`, with a trailing `.desktop` stripped,
    then the window's own decimal id (the `id` that `windows` lists for it, so
    the executor can find that entry and resolve the app by pid, or refuse it
    as an unattributed window). The window is always an application window -
    the ICCCM client, or an override-redirect window itself - never a WM frame
    or decoration window. `display_name` is the window title, else the bundle
    id.
  - **`app-under-point` hit test:** walks the root's children top to bottom
    (the X stacking order, which every WM keeps for its frames). A root child
    whose **input shape** (SHAPE `GetRectangles` of kind `Input`, relative to
    the window origin) does not contain the point is skipped: the server
    routes the click past it. That is how click-through overlays are looked
    through, such as Claude's Computer Use glow (override-redirect, WM_CLASS
    `com.anthropic.Claude`; Electron 45's `setIgnoreMouseEvents(true)` gives
    it a 1x1 input rectangle at its origin, other toolkits an empty one). A
    window without an input shape reports its whole rectangle. Without SHAPE
    1.1 (`doctor.capabilities.input_shape: false`) every window counts in its
    full rectangle. A point in a
    managed window's frame (title bar, buttons, borders) or client area
    resolves to the ICCCM client below that frame: the root child itself when
    it carries `WM_STATE`, else the first window with `WM_STATE` in a
    breadth-first descent of up to three levels (the `XmuClientWindow` route,
    same as the mask reader); failing that, a member of `_NET_CLIENT_LIST` /
    `_NET_CLIENT_LIST_STACKING` whose root-child ancestor is this root child
    (a client deeper than three levels). Without a WM (nobody sets
    `WM_STATE`) a root child with a `WM_CLASS`, else its first viewable child
    with one, counts. A root child with none of these is a window of the WM's
    own and is looked through.
    A client with `_GTK_FRAME_EXTENTS` (client-side decorations) only counts
    inside its visible part (`mask::visual_rect`): the invisible shadow margin
    falls through to whatever is below. `_NET_WM_STATE_HIDDEN` windows are
    looked through. Docks, the desktop window and override-redirect popups
    answer only when no application window is at the point (the topmost of
    them). The answer is `{bundleId, displayName}` of that client (its own id
    when it names no app, see above), or `null` only when no window is at the
    point (the empty desktop). The `windows`
    geometry stays the X client rect, shadow margin included (`ax` maps
    AT-SPI coordinates against that client rect).
  - **`exclude_from_capture` is always `false`** - X11 has no per-window
    capture-exclusion (that is a KWin compositor feature); the field exists only
    for JSON parity with the kwin bridge.
  - **Window-list fallback order:** `windows` uses
    `_NET_CLIENT_LIST_STACKING`, else `_NET_CLIENT_LIST` (xfwm4 sets only this
    one; the WM's own order, not stacking, so frontmost relies on
    `_NET_ACTIVE_WINDOW`), else a `query_tree` of the root's children (bare X /
    Xvfb / minimal WMs), keeping mapped, non-override-redirect top-levels that
    carry a `WM_CLASS` (a root child without one is treated as a WM frame and
    searched one level down for the client).
  - **EWMH-absent fallback:** `frontmost-app` falls back to `GetInputFocus` (focus `None`, `PointerRoot` or the root window means no app: it prints `null`;
    a focused frame or client subwindow is reported as its ICCCM client).
    `app-under-point` needs no EWMH (see its hit test above). Every
    optional property that is absent maps to `None`/`false` rather than an error,
    so read-only commands never crash on a WM-less display (they return an empty
    list or a graceful `exit 1`).
- Fill the stub bodies and remove the crate-level `#![allow(dead_code)]` in
  `main.rs` once every scaffold item has a caller. **Do not** change signatures
  in `main.rs`/`cli.rs`.
