# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
binary into `claude-desktop-bin`'s `js/executor_linux.js` is the remaining
follow-up.
