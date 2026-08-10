#!/usr/bin/env bash
# scripts/cargo-target-dir.sh
#
# Prints Cargo's *actual* resolved target directory for this workspace to
# stdout — never assume it is the relative `target/` default. Cargo output
# can be redirected via `build.target-dir` in `~/.cargo/config.toml` or the
# `CARGO_TARGET_DIR` environment variable, either of which points the real
# build output somewhere other than `<repo>/target/` (issue #5922).
#
# Usage:
#   TARGET_DIR="$(scripts/cargo-target-dir.sh [WORKSPACE_ROOT])"
#
# Resolution order (mirrors Cargo's own precedence):
#   1. $CARGO_TARGET_DIR, when set and non-empty (env beats config in Cargo).
#      A relative value is resolved against the workspace root — which is the
#      directory every Loom build step `cd`s into before invoking cargo.
#   2. `cargo metadata --format-version 1 --no-deps`, which applies the full
#      `config.toml` hierarchy (including `build.target-dir`). Parsed with
#      `jq` when available, otherwise with a dependency-free `sed` extraction
#      so a host without `jq` still resolves correctly.
#   3. `<workspace root>/target` — Cargo's default, i.e. the historical
#      hardcoded behavior. Used only as a last resort (no cargo on PATH, or
#      `cargo metadata` failed), with a warning on stderr.
#
# Always exits 0 with a path on stdout: the fallback is exactly the behavior
# callers had before this script existed, so a resolution hiccup degrades to
# the old assumption rather than breaking an install outright. Warnings and
# diagnostics go to stderr, never stdout, so `$(...)` capture stays clean.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="${1:-$(cd "$SCRIPT_DIR/.." && pwd)}"

# Resolve a possibly-relative path against the workspace root without
# requiring it to exist yet (the target dir is created by the build itself).
absolutize() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$WORKSPACE_ROOT/$1" ;;
  esac
}

# 1. CARGO_TARGET_DIR wins outright in Cargo, so short-circuit here too. This
#    also means the common CI/test override needs neither cargo nor jq.
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  absolutize "$CARGO_TARGET_DIR"
  exit 0
fi

# 2. Ask Cargo. This is the only path that understands the `config.toml`
#    hierarchy (`build.target-dir` in ~/.cargo/config.toml, .cargo/config.toml
#    beside the workspace, and so on).
if command -v cargo >/dev/null 2>&1; then
  METADATA="$(cd "$WORKSPACE_ROOT" && cargo metadata --format-version 1 --no-deps 2>/dev/null)"
  if [[ -n "$METADATA" ]]; then
    if command -v jq >/dev/null 2>&1; then
      RESOLVED="$(printf '%s' "$METADATA" | jq -r '.target_directory // empty' 2>/dev/null)"
    else
      # `cargo metadata` emits compact single-line JSON, so a targeted
      # extraction of the "target_directory" string value is sufficient and
      # keeps `jq` off the critical path of an install.
      RESOLVED="$(printf '%s' "$METADATA" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    fi
    if [[ -n "${RESOLVED:-}" && "$RESOLVED" != "null" ]]; then
      absolutize "$RESOLVED"
      exit 0
    fi
  fi
  echo "Warning: could not read Cargo's target directory via 'cargo metadata' (from $WORKSPACE_ROOT);" >&2
  echo "         falling back to the default '$WORKSPACE_ROOT/target'." >&2
else
  echo "Warning: no 'cargo' on PATH; assuming the default target directory '$WORKSPACE_ROOT/target'." >&2
fi

# 3. Cargo's default — identical to the behavior that predates this script.
printf '%s\n' "$WORKSPACE_ROOT/target"
