#!/usr/bin/env bash
# Run the x11-bridge input integration tests against a throwaway Xvfb display.
#
# The XTEST-injection tests are #[ignore]d by default because they synthesize
# real pointer/keyboard events. Running them against your live session would
# move your cursor and type into your focused window. This wrapper spins up an
# isolated Xvfb so the injection is harmless.
#
# Usage:
#   scripts/test-xvfb.sh                # run all #[ignore]d integration tests
#   scripts/test-xvfb.sh type_text      # run a single test by name filter
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v xvfb-run >/dev/null 2>&1; then
  echo "error: xvfb-run not found (install xorg-server-xvfb / xvfb)" >&2
  exit 1
fi

FILTER="${1:-}"

# --ignored runs the #[ignore]d tests; the module is built into the bin target.
# A single test thread avoids two tests warping the shared virtual pointer at
# once. `xdotool` inside the same Xvfb is a handy manual oracle but the tests
# assert via x11rb events directly, so it is not required here.
xvfb-run -a -s "-screen 0 1280x800x24" \
  cargo test --bin x11-bridge -- --ignored --test-threads=1 ${FILTER:+"$FILTER"}
