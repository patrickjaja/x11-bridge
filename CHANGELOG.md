# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
