#!/bin/bash
# macOS test runner for cargo
#
# Workaround for _dyld_start hangs on macOS ARM64 (issue #2298).
# Ad-hoc signs test binaries before execution to satisfy macOS
# code signature verification, preventing dyld from hanging
# during binary load.
#
# Configured via .cargo/config.toml:
#   [target.aarch64-apple-darwin]
#   runner = ".cargo/macos-test-runner.sh"

binary="$1"
shift

# Ad-hoc sign the binary to prevent _dyld_start verification hangs.
# -f: force replace any existing signature
# -s -: use ad-hoc identity (no developer certificate needed)
codesign -f -s - "$binary" 2>/dev/null

exec "$binary" "$@"
