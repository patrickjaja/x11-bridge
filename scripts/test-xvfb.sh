#!/usr/bin/env bash
# Run the x11-bridge input integration tests against a throwaway Xvfb display.
#
# The XTEST-injection tests are #[ignore]d by default because they synthesize
# real pointer/keyboard events. Running them against your live session would
# move your cursor and type into your focused window. This wrapper spins up an
# isolated Xvfb so the injection is harmless.
#
# Every suite runs even when an earlier one fails; the script exits non-zero
# at the end if any failed.
#
# Usage:
#   scripts/test-xvfb.sh                # run all #[ignore]d integration tests
#   scripts/test-xvfb.sh type_text      # run a single test by name filter
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

if ! command -v xvfb-run >/dev/null 2>&1; then
  echo "error: xvfb-run not found (install xorg-server-xvfb / xvfb)" >&2
  exit 1
fi

FILTER="${1:-}"
failed=()

# --ignored runs the #[ignore]d tests; the module is built into the bin target.
# A single test thread avoids two tests warping the shared virtual pointer at
# once. `xdotool` inside the same Xvfb is a handy manual oracle but the tests
# assert via x11rb events directly, so it is not required here.
if ! xvfb-run -a -s "-screen 0 1280x800x24" \
  cargo test --bin x11-bridge -- --ignored --test-threads=1 ${FILTER:+"$FILTER"}; then
  failed+=("bin x11-bridge (in-crate Xvfb tests)")
fi

# `abort-watch` drives the built binary (spawned as a child, like Claude
# Desktop does) and injects "physical" keys through Xvfb's own keyboard slave.
# One test starts and kills a second, private Xvfb (-displayfd).
if ! xvfb-run -a -s "-screen 0 1280x800x24" \
  cargo test --test abort_watch_xvfb -- --ignored --test-threads=1 ${FILTER:+"$FILTER"}; then
  failed+=("abort_watch_xvfb")
fi

# `app-under-point` under a reparenting window manager: every test starts its
# own private Xvfb (-displayfd) plus an xfwm4 on its own dbus-daemon and an
# empty XDG_CONFIG_HOME, so the desktop session is never touched. Needs xfwm4
# (with xfconf) and dbus-daemon; missing ones fail the suite, not skip it.
if ! command -v xfwm4 >/dev/null 2>&1 || ! command -v dbus-daemon >/dev/null 2>&1; then
  echo "error: wm_hit_xvfb needs xfwm4 and dbus-daemon (install xfwm4, xfconf, dbus)" >&2
  failed+=("wm_hit_xvfb (xfwm4 or dbus-daemon missing)")
elif ! env -u DISPLAY -u WAYLAND_DISPLAY cargo test --test wm_hit_xvfb -- --ignored --test-threads=1 ${FILTER:+"$FILTER"}; then
  failed+=("wm_hit_xvfb")
fi

if ((${#failed[@]})); then
  printf 'FAILED suite: %s\n' "${failed[@]}" >&2
  exit 1
fi
echo "all Xvfb suites passed"
