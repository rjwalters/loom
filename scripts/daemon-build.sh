#!/usr/bin/env bash
# scripts/daemon-build.sh
#
# Builds the loom-daemon release binary and produces the
# architecture-specific copy (matches release artifact naming) alongside it.
#
# Resolves Cargo's *actual* target directory via `scripts/cargo-target-dir.sh`
# instead of assuming the relative `target/` default — respects
# `build.target-dir` in `~/.cargo/config.toml` and `CARGO_TARGET_DIR` (issue
# #5922). Without this, a redirected target dir makes the build itself
# succeed while the subsequent `cp` fails with a misleading "Failed to build
# loom-daemon" error, because the binary was never at the assumed path.
#
# On success, prints the resolved target directory as the ONLY stdout line —
# callers that need it (scripts/install-loom.sh) can capture it directly:
#   TARGET_DIR="$(scripts/daemon-build.sh)" || exit 1
# All progress/diagnostic output goes to stderr.
#
# Distinguishes two failure modes — via distinct exit codes, so a caller can
# tell them apart without string-matching stderr — so a genuine compile
# failure is never confused with a missing binary after a successful build:
#   - cargo build itself fails            -> exit 1, "Failed to build loom-daemon"
#   - build succeeds but binary is absent -> exit 3, a distinct "binary not found" message

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

TARGET_DIR="$("$SCRIPT_DIR/cargo-target-dir.sh" "$REPO_ROOT")"

if ! cargo build --package loom-daemon --release >&2; then
  echo "Error: Failed to build loom-daemon" >&2
  exit 1
fi

DAEMON_BIN="$TARGET_DIR/release/loom-daemon"
if [[ ! -x "$DAEMON_BIN" ]]; then
  echo "Error: loom-daemon build succeeded, but no binary was found at $DAEMON_BIN" >&2
  echo "       (Cargo target dir resolved to: $TARGET_DIR — check build.target-dir / CARGO_TARGET_DIR)" >&2
  exit 3
fi

# Copy to architecture-specific name (matches release artifact naming)
rm -f "$TARGET_DIR/release/loom-daemon-aarch64-apple-darwin"
cp "$DAEMON_BIN" "$TARGET_DIR/release/loom-daemon-aarch64-apple-darwin"

echo "$TARGET_DIR"
