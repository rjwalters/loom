#!/usr/bin/env bash
# scripts/install/loom-source-path.sh — shared helpers for the
# `.loom/loom-source-path` sidecar (#6780).
#
# The sidecar records where a consumer repo's Loom install came from. It is a
# DURABLE pointer — resync-installed.sh, check-main-freshness.sh, and the
# tool-package tooling all read it long after the install that wrote it — but
# nothing previously validated what got written into it. A clone made inside
# an ephemeral per-session scratch directory (e.g. `/tmp/.../scratchpad/...`)
# is a reasonable thing to do once; recording that transient location as a
# repo's permanent upstream pointer is what turns it into a dead reference the
# first time the scratch area is cleaned up.
#
# This file provides:
#   - is_ephemeral_loom_source_path <path>   -- predicate, no output
#   - warn_if_ephemeral_loom_source_path <path> -- prints a non-blocking
#     warning (via the caller's own `warning()` helper if defined, else a
#     plain stderr line) when <path> resolves under a well-known scratch
#     location. Never blocks the install -- advisory only, matching every
#     other install-time warning in this repo.
#
# Source with:
#     source "$LOOM_ROOT/scripts/install/loom-source-path.sh"
# then call `warn_if_ephemeral_loom_source_path "$loom_root"` right before
# writing the sidecar.

# Well-known ephemeral roots a source clone should never be recorded from as a
# durable pointer. Checked as path PREFIXES (after the path itself, and
# $TMPDIR if set). Not exhaustive by design -- this is a best-effort warning,
# not a hard block, so a false negative here is far cheaper than a false
# positive that scares off a legitimate custom TMPDIR-adjacent setup.
_LOOM_EPHEMERAL_PATH_PREFIXES=(
  "/tmp/"
  "/private/tmp/"
  "/var/tmp/"
  "/private/var/tmp/"
  "/dev/shm/"
)

is_ephemeral_loom_source_path() {
  local path="$1"
  [[ -n "$path" ]] || return 1

  # Normalize: ensure a trailing slash so a prefix match can't false-positive
  # on a sibling directory that merely shares a string prefix (e.g. a real
  # "/tmp2/..." repo would not match "/tmp/" as a path-component prefix).
  local normalized="$path/"

  local prefix
  for prefix in "${_LOOM_EPHEMERAL_PATH_PREFIXES[@]}"; do
    [[ "$normalized" == "$prefix"* ]] && return 0
  done

  # Also flag the current $TMPDIR, if set to something other than one of the
  # above (e.g. a per-CI-job or per-session override).
  if [[ -n "${TMPDIR:-}" ]]; then
    local tmpdir_normalized="${TMPDIR%/}/"
    [[ "$normalized" == "$tmpdir_normalized"* ]] && return 0
  fi

  return 1
}

warn_if_ephemeral_loom_source_path() {
  local path="$1"
  is_ephemeral_loom_source_path "$path" || return 0

  local msg="Loom source path '$path' resolves under a scratch/ephemeral location. .loom/loom-source-path is a DURABLE pointer read by resync-installed.sh and check-main-freshness.sh long after this install finishes -- once this directory is cleaned up, that resolution will silently fail. Consider re-running the installer from a persistent clone."
  if declare -F warning >/dev/null 2>&1; then
    warning "$msg"
  else
    printf 'WARN: %s\n' "$msg" >&2
  fi
}

# Best-effort: resolve <path>'s `origin` remote URL, if it is a git checkout
# with one configured. Empty output (not an error) when unavailable -- callers
# treat a blank remote as "unknown", never as a fatal condition (#6780 AC3:
# recording the remote lets a vanished local clone still be identified/
# re-resolved, but its absence must never block an install).
loom_source_remote_url() {
  local path="$1"
  [[ -n "$path" && -d "$path" ]] || return 0
  git -C "$path" remote get-url origin 2>/dev/null || true
}
