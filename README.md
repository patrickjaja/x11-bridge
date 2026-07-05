# x11-bridge

First-party X11 bridge for Claude Desktop's Computer Use on Linux. A single
pure-Rust, statically-linked binary that replaces the shell-out chain
(`xdotool`, `scrot`, ImageMagick `import`, `gnome-screenshot`, `wmctrl`) on
**X11 sessions** (and the XWayland fallback). It is the X11 sibling of
[`kwin-portal-bridge`](https://github.com/patrickjaja/kwin-portal-bridge)
(KDE/Wayland) and speaks the **same kebab-case one-shot CLI + JSON contract**,
so the Claude Desktop JS executor (`js/executor_linux.js` in `claude-desktop-bin`)
treats them interchangeably and can pick a backend per session without changing
how it parses output.

**Status: experimental, v0.1.** All subcommands are implemented: connection +
RandR monitor enumeration, `GetImage` capture (JPEG), XTEST pointer/keyboard
synthesis, and EWMH/ICCCM window queries. The XTEST-injection paths are covered
by Xvfb integration tests (run via `scripts/test-xvfb.sh`). Wiring the resolver
into `claude-desktop-bin`'s `js/executor_linux.js` is the remaining follow-up.

## Relationship to kwin-portal-bridge

`kwin-portal-bridge` and `x11-bridge` are sibling projects that cover different
Linux session types with one shared JSON contract:

| Bridge | Covers | Input / capture mechanism |
|---|---|---|
| `kwin-portal-bridge` | KDE Plasma / Wayland | RemoteDesktop + ScreenCast portal, PipeWire (long-lived session) |
| `x11-bridge` | X11 and XWayland fallback | XTEST input + `GetImage` capture (stateless, one-shot) |

Because X11 has no portal session to keep alive, `x11-bridge` is
**daemon-free**: every subcommand is a short-lived process that opens a fresh X
connection, does its work, and exits. `session-start`/`session-end` are no-ops
that report `{"ok":true}` for parity. See [DESIGN.md](DESIGN.md) for the
no-daemon rationale and the full contract.

## Portability (static musl, incl. NixOS)

`x11-bridge` is **pure Rust with zero C dependencies** - it uses `x11rb`'s
`RustConnection` (no `libxcb`), the `image` crate for JPEG encoding, and
nothing that links a system library. It builds as a **fully static musl
binary** linked with `rust-lld`, so there is no external cross-linker and no
runtime library dependency at all.

The result is distro-independent: the same binary runs on Arch, Ubuntu,
Debian, Fedora, RHEL, and **NixOS** without patching an interpreter or shipping
shared libraries. This is a deliberate contrast with `kwin-portal-bridge`, which
links `libxkbcommon`/`libpipewire` and is therefore excluded on NixOS.

## Build

```sh
# Native release build (whatever your host target is):
cargo build --release

# Fully-static musl builds (recommended - no C linking anywhere):
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

`.cargo/config.toml` sets `rust-lld` + `+crt-static` for both musl targets, so
those commands work with just the toolchain installed - no `musl-gcc`, no
`gcc-aarch64-linux-gnu`. Dependencies are pure Rust: `x11rb` (RustConnection,
`randr`/`xtest`/`xfixes` extensions), `image` (JPEG), `xkeysym`, `clap`,
`serde`/`serde_json`, `base64`, `anyhow`.

### Reproducible local pipeline build

`scripts/pipeline-build-local.sh` builds **both** musl targets in a single
Docker run (mirroring CI) and drops the binaries in `./dist/`:

```sh
./scripts/pipeline-build-local.sh
# dist/x11-bridge          (x86_64, statically linked)
# dist/x11-bridge-aarch64  (aarch64, statically linked)
```

A persistent cache lives in `./.pipeline-cache/`, so re-runs are fast.

## CLI overview

One-shot subcommands. Each prints a single JSON object/array on stdout; on
failure it prints a message to stderr and exits 1 (clap parse errors exit 2).

| Subcommand | Purpose |
|---|---|
| `doctor` | X server / extension / keyboard diagnostics (human-facing) |
| `screens` | Enumerate monitors (RandR) |
| `windows` | Enumerate windows (EWMH) |
| `screenshot [--display N]` | Capture a full monitor |
| `zoom --display N --x --y --w --h` | Capture a region within a monitor |
| `pointer-move` / `-click` / `-scroll` / `-drag` | Pointer actions (XTEST) |
| `left-mouse-down` / `left-mouse-up` | Press/release-and-hold the left button |
| `key-sequence --keys <spec>` | Send a key combo, e.g. `ctrl+shift+tab` |
| `type --text <t>` | Type text as key events |
| `hold-key --key <k>... --duration-ms N` | Hold keys for a fixed duration |
| `cursor-position` / `frontmost-app` / `app-under-point` | Pointer + app queries |
| `activate-window --window <id>` | Raise + focus a window |
| `session-start` / `session-end` | No-ops on X11 (report `{"ok":true}`) |

```sh
x11-bridge screens
x11-bridge screenshot --display eDP-1
x11-bridge zoom --display eDP-1 --x 100 --y 100 --w 400 --h 300
x11-bridge pointer-click --x 640 --y 480 --button left --count 2
x11-bridge key-sequence --keys 'ctrl+shift+tab' --repeat 1
x11-bridge type --text 'hello' --delay-ms 12
x11-bridge doctor
```

See [DESIGN.md](DESIGN.md) for the full subcommand → JSON output contract, the
key-spec grammar, the coordinate system, and the no-daemon rationale.

## Bundling into claude-desktop-bin

`x11-bridge` is intended to be consumed by
[`claude-desktop-bin`](https://github.com/patrickjaja/claude-desktop-bin) the
same way `kwin-portal-bridge` is: the pre-built static binary is shipped inside
the package and the JS executor resolves it at runtime, in order:

1. the `X11_BRIDGE_BIN` environment variable (explicit override),
2. `process.resourcesPath` (the bundled copy inside the packaged app),
3. `x11-bridge` on `$PATH` (a system install).

This mirrors `KWIN_PORTAL_BRIDGE_BIN` for the KDE/Wayland bridge. Because the
binary is fully static, the bundled copy runs on every supported distro without
extra runtime dependencies. Wiring the resolver into `js/executor_linux.js` is a
follow-up on the `claude-desktop-bin` side and is out of scope for this repo.

## License

MIT. See [LICENSE](LICENSE).
