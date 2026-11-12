# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 2026-11-12

### Fixed

- `type` lost every character the layout has no key for at levels 1-2 (on a
  German layout `@ { [ ] } \ | ~ € ^ ´ \``, plus accents, emoji, CJK on any
  layout): the spare keycode was restored right after the key, before the
  client had read the new mapping. Measured under Xvfb in GTK, urxvt and
  Chrome: all lost at the default delay, the previous character typed instead
  at `--delay-ms 0`. Such characters are now bound page by page to the
  keycodes with no keysym (case pairs as `[small, capital]`, two characters
  without case per keycode), typed 50 ms after the binding and restored
  200 ms after the last of them, once per page instead of once per
  character. Below 4 ms per key a page change also waits for a client reading
  one key per 4 ms to catch up, at most 2 s.
- `type` uses the layout's AltGr levels (German `@ { [ ] } \ | ~ €` need no
  remap any more), prefers the main block over the keypad (`(` is Shift+9,
  not the keypad's parenthesis key) and types exactly the text under Caps
  Lock: bound case pairs get a key type that uses Lock (set explicitly for
  Unicode keysyms, where the server would pick TWO_LEVEL), and the clients'
  own Caps Lock upper-casing is accounted for.
- With a second layout active (`us,de` switched to `de`), every command
  pressed the first layout's key: `y` came out as `z`, symbols as other
  symbols or dead keys. Keys are now chosen in the active XKB group.
- `key-sequence` / `hold-key` press the physical key: on an active Russian
  layout `ctrl+c` presses the Latin layout's C key (it used to rebind a spare
  keycode). A key no group has (`key €` on `us`) is bound and kept until
  200 ms after the release.
- Two bridges at once could rebind each other's keycodes and leave them
  bound for good. Binding keycodes now takes a per-display lock (the
  selection `_X11_BRIDGE_KEYMAP_LOCK`, released by the server when the
  bridge dies); a second bridge waits up to 3 s, then fails clearly.
- The restore and the crash recovery only touch keycodes that still carry
  exactly the row the bridge wrote: a layout loaded meanwhile (setxkbmap,
  xmodmap, a compositor's keymap on XWayland) is kept. `type` stops with
  `reason: "keymapChanged"` when the keymap changes under it.
- A `type` killed by the app's timeout (SIGTERM) restores the keymap and
  prints its result with `charCount`, `interrupted: true` and `reason` on
  stdout (exit 1), so the caller can type the rest. A signal after the last
  character is success. `hold-key` and `key-sequence` stop on SIGTERM too,
  with the keys released. After a SIGKILL the next keyboard command restores
  the keycodes from a root-window journal.

### Tests

- `tests/type_xvfb.rs` (the built binary against private Xvfbs): Caps Lock
  and Num Lock text in an Xlib terminal (xterm or urxvt), SIGTERM mid-text,
  a signal after the last character, SIGKILL recovery, a real key on a
  journaled keycode after a SIGKILL, two bridges at once, `ctrl+c` on
  `us,ru`, a foreign keymap change mid-text, SIGTERM during `hold-key`,
  `(` and `)` from the main block, a 4000-character burst. All but the
  SIGKILL recovery fail on the previous build.
- In-crate Xvfb tests: German layout, `us,de` on group 2 and more distinct
  characters than spare keycodes against a client that reads the keymap
  lazily; unit tests for levels, groups, Caps Lock, paging, slot reuse and
  the journal. CI installs `x11-xkb-utils` and `xterm`.

## 2026-11-11

### Fixed

- `app-under-point` on a reparenting window manager (xfwm4, Openbox, Marco,
  ...): a point on a window's title bar, buttons or border answered with a
  bare decoration-window id (`{"bundleId":"7710972"}`), so Claude Desktop
  refused clicks on window controls. The hit test now walks the X stacking
  order and resolves every root child to its ICCCM client (`WM_STATE`), so
  the frame answers with the client's app. A title bar lying over another
  window's client area no longer answers with that lower window.
- The invisible shadow margin of a client-side-decorated window
  (`_GTK_FRAME_EXTENTS`) no longer counts as the window; the point falls
  through to what is below.
- `app-under-point` looks through click-through windows: a root child whose
  input shape (SHAPE) does not contain the point is skipped, as the server
  routes the click past it. Claude's own Computer Use glow (override-redirect,
  1x1 input rectangle) no longer answers for the dock, the desktop or the
  windows below it. `doctor` reports `capabilities.input_shape`.
- A client deeper than three levels below its frame is found through the WM's
  client list instead of the point falling through to the window below.
- A client that names no app answers with its own window id (the `id` that
  `windows` lists), never a frame or decoration id, in `app-under-point` and
  in the `frontmost-app` focus fallback. `app-under-point` prints `null` only
  when no window is at the point (the empty desktop), and exits 0 there
  instead of failing.

## 2026-11-10

### Added

- `abort-watch`: long-running watcher for the user's Escape (Escape stops
  Computer Use). XInput2 raw key events on the root window, passive (the key
  still reaches the focused window), delivered under other clients' grabs.
  Escape injected over XTEST (the bridge's own `key-sequence`, `xdotool`) is
  ignored by source device, so the `ready` line reports `syntheticSafe: true`
  (`false` on Xwayland). Follows device hotplug, new masters and keymap
  changes; a lost X connection ends it with an `error` line and exit 1.
  Protocol from the shared `cu-abort-watch` crate.
- `doctor` reports `abortWatch` (`available` with XInput >= 2.1, else a
  `reason`).

## 2026-11-04

### Added

- Mask mode for `screenshot` and `zoom`: `--keep-window <id>` (repeatable) or
  `--mask-all` paints every pixel outside the kept windows' visible region with
  `#1E1E1E`. Menus, tooltips and dialogs of a kept window stay visible. The
  window tree and the pixels are read under one server grab, and any read error
  fails the command instead of returning an unmasked image. The output gains a
  `mask` object (kept, hidden, missing and masked windows) and `doctor` reports
  `capabilities.screenshot_mask: true`.

## 2026-10-27

### Fixed

- `frontmost-app` prints `null` when no window holds input focus (`None`,
  `PointerRoot` or the root window, e.g. a bare Xvfb) instead of a bogus
  `{"bundleId":"1"}`.
- `windows` and `app-under-point` fall back to `_NET_CLIENT_LIST` when
  `_NET_CLIENT_LIST_STACKING` is absent (xfwm4 4.20 on a fresh Xvfb publishes
  `_NET_CLIENT_LIST` without `_NET_CLIENT_LIST_STACKING`; `windows` returned `[]`). The `query_tree` fallback now also looks one level inside WM
  frame windows for the client.
- Key names accept the standard X keysym names for ASCII punctuation and
  symbols (`ctrl+comma`, `shift+period`, `ctrl+bracketleft`, ...), matched
  case-insensitively; previously only the literal character (`ctrl+,`) worked.

## 2026-10-23

### Changed

- Computer Use bridge CLI contract v1: `doctor` always exits 0 and leads with
  `contract`, `bridge`, `version`, `backend`, `session_match`,
  `backend_reachable`, `backend_error` and the session keys (the X server
  details follow when reachable). `session-end` now reports
  `{"ok":true,"ended":false,"daemon":"none"}`.

### Added

- `forget-session` (no-op on X11, no side effects).
- `tests/contract.rs`: contract checks against the built binary with no display.

## 2026-07-05

### Added

- Initial v0.1.0 scaffold of `x11-bridge`, the first-party X11 backend for
  Claude Desktop's Computer Use on Linux. A single pure-Rust, statically
  linkable (musl) binary that replaces the shell-out chain (`xdotool`, `scrot`,
  ImageMagick `import`, `gnome-screenshot`, `wmctrl`) on X11 sessions. It is the
  X11 sibling of [`kwin-portal-bridge`](https://github.com/patrickjaja/kwin-portal-bridge)
  (KDE/Wayland) and speaks the same kebab-case one-shot CLI + JSON contract, so
  the Claude Desktop JS executor can treat them interchangeably.
- One-shot subcommands with a documented JSON contract (see `DESIGN.md`):
  `doctor`, `screens`, `windows`, `screenshot`, `zoom`, pointer actions
  (`pointer-move`/`-click`/`-scroll`/`-drag`, `left-mouse-down`/`-up`), keyboard
  actions (`key-sequence`, `type`, `hold-key`), window/app queries, and no-op
  `session-start`/`session-end` (X11 needs no portal session).
- Fully static builds for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl`, linked with `rust-lld` (no external C
  cross-linker, no system libraries). The resulting binary is
  distro-independent and runs on any Linux, including NixOS.
- CI (`fmt` + `clippy -D warnings`, `build --release --locked` and `test` for
  both musl targets, a static-linkage assertion via `file`/`ldd`, and an Xvfb
  smoke test) and a tag-driven draft GitHub Release that publishes per-arch
  `.tar.gz` archives with per-artifact `.sha256` and a combined `SHA256SUMS`.
- `scripts/pipeline-build-local.sh` for a reproducible local build of both musl
  targets in one Docker run, mirroring the CI pipeline.

Status: experimental. All subcommands are implemented (capture, XTEST
pointer/keyboard synthesis, EWMH/ICCCM window queries) and the XTEST-injection
paths are covered by Xvfb integration tests. Screenshots are JPEG (quality 75,
matching `kwin-portal-bridge`), encoded with the `image` crate. Wiring the
binary into `claude-desktop-extra`'s `js/executor_linux.js` is the remaining
follow-up.
