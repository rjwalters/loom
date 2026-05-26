#!/usr/bin/env bash
# check-active-session.sh — Detect an active Loom session in a target repository
#
# Usage:
#   check-active-session.sh <target-path>
#
# Exit codes:
#   0 — No active session detected (or target has no .loom/ directory)
#   1 — Active session detected (one or more indicators fired)
#   2 — Invalid usage (missing or unreadable target)
#
# Indicators (any one triggers detection):
#   1. Live daemon: .loom/daemon-loop.pid exists AND kill -0 <pid> succeeds.
#      kill -0 returns EPERM on cross-user PIDs — we treat that as live (fail
#      closed). Stale PID files alone are not a hit.
#   2. Recent active state: .loom/daemon-state.json has "running": true AND
#      file mtime is within the last 5 minutes (300 seconds).
#   3. In-flight builders: any .loom/worktrees/issue-N directory with mtime
#      activity within the last 5 minutes.
#
# When any indicator fires, each is reported on its own line on stderr, then a
# trailing reason summary line. The script is silent on the happy path.
#
# POSIX/macOS notes:
#   - We use POSIX stat invocations with both BSD (-f) and GNU (-c) syntaxes
#     and a portable arithmetic comparison against `date +%s`. We do not rely
#     on `find -mmin`, which behaves identically on macOS and Linux but is
#     awkward to use with single-file lookups.
#   - jq is preferred for reading daemon-state.json; we degrade to a grep
#     fallback when jq is unavailable.

set -euo pipefail

TARGET_PATH="${1:-}"

if [[ -z "$TARGET_PATH" ]]; then
  echo "Usage: $0 <target-path>" >&2
  exit 2
fi

if [[ ! -d "$TARGET_PATH" ]]; then
  echo "Error: target path does not exist or is not a directory: $TARGET_PATH" >&2
  exit 2
fi

LOOM_DIR="$TARGET_PATH/.loom"
THRESHOLD_SECONDS=300

# Short-circuit when there is no .loom/ at all (first-time install).
if [[ ! -d "$LOOM_DIR" ]]; then
  exit 0
fi

# Portable mtime-in-epoch-seconds reader. Echoes the mtime on stdout, or
# nothing (and a non-zero exit) when the file doesn't exist.
_file_mtime_epoch() {
  local path="$1"
  if [[ ! -e "$path" ]]; then
    return 1
  fi
  # BSD stat (macOS)
  if stat -f '%m' "$path" 2>/dev/null; then
    return 0
  fi
  # GNU stat (Linux)
  if stat -c '%Y' "$path" 2>/dev/null; then
    return 0
  fi
  return 1
}

# Returns 0 if the file mtime is within THRESHOLD_SECONDS of now.
_file_is_recent() {
  local path="$1"
  local mtime now
  mtime=$(_file_mtime_epoch "$path" 2>/dev/null) || return 1
  now=$(date +%s)
  local age=$((now - mtime))
  if (( age < 0 )); then
    # Clock skew — treat as recent (fail closed).
    return 0
  fi
  if (( age <= THRESHOLD_SECONDS )); then
    return 0
  fi
  return 1
}

# Indicator accumulator. We print one line per fired indicator so the operator
# can see exactly what tripped the check without re-running anything.
INDICATORS=()
DETECTED=0

# ─────────────────────────────────────────────────────────────────────────────
# Indicator 1: Live daemon (daemon-loop.pid + alive PID)
# ─────────────────────────────────────────────────────────────────────────────
PIDFILE="$LOOM_DIR/daemon-loop.pid"
if [[ -f "$PIDFILE" ]]; then
  PID_CONTENT=""
  # Read PID file safely (avoid hangs on weird content).
  PID_CONTENT=$(head -c 64 "$PIDFILE" 2>/dev/null | tr -d '[:space:]' || true)
  if [[ "$PID_CONTENT" =~ ^[0-9]+$ ]]; then
    # kill -0 returns:
    #   0  → process exists and we may signal it
    #   1+ → either no process or EPERM (cross-user PID)
    # We can't distinguish EPERM from ESRCH in pure bash. Use `ps -p` to break
    # the tie: if the process exists in the process table we treat it as live.
    if kill -0 "$PID_CONTENT" 2>/dev/null; then
      INDICATORS+=("Daemon PID file present: .loom/daemon-loop.pid (PID $PID_CONTENT alive)")
      DETECTED=1
    elif ps -p "$PID_CONTENT" >/dev/null 2>&1; then
      # Cross-user PID — fail closed.
      INDICATORS+=("Daemon PID file present: .loom/daemon-loop.pid (PID $PID_CONTENT alive, different user)")
      DETECTED=1
    fi
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
# Indicator 2: Recently-active daemon state file with running=true
# ─────────────────────────────────────────────────────────────────────────────
STATE_FILE="$LOOM_DIR/daemon-state.json"
if [[ -f "$STATE_FILE" ]]; then
  RUNNING=""
  if command -v jq >/dev/null 2>&1; then
    RUNNING=$(jq -r '.running // empty' "$STATE_FILE" 2>/dev/null || true)
  else
    # Grep fallback — match "running": true (allow whitespace, ignore commas).
    if grep -Eq '"running"[[:space:]]*:[[:space:]]*true' "$STATE_FILE" 2>/dev/null; then
      RUNNING="true"
    fi
  fi

  if [[ "$RUNNING" == "true" ]] && _file_is_recent "$STATE_FILE"; then
    INDICATORS+=("Active daemon state: .loom/daemon-state.json has running=true and was updated within the last 5 minutes")
    DETECTED=1
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
# Indicator 3: In-flight issue worktrees (.loom/worktrees/issue-N) with recent activity
# ─────────────────────────────────────────────────────────────────────────────
WORKTREES_DIR="$LOOM_DIR/worktrees"
if [[ -d "$WORKTREES_DIR" ]]; then
  ACTIVE_WORKTREE_COUNT=0
  ACTIVE_WORKTREE_NAMES=()
  # Iterate only directories matching issue-N.
  # Using a glob is portable; null when no matches.
  shopt -s nullglob
  for wt in "$WORKTREES_DIR"/issue-*; do
    if [[ -d "$wt" ]]; then
      if _file_is_recent "$wt"; then
        ACTIVE_WORKTREE_COUNT=$((ACTIVE_WORKTREE_COUNT + 1))
        ACTIVE_WORKTREE_NAMES+=("$(basename "$wt")")
      fi
    fi
  done
  shopt -u nullglob

  if (( ACTIVE_WORKTREE_COUNT > 0 )); then
    # Cap the list in the message to avoid wall-of-text for large pools.
    if (( ACTIVE_WORKTREE_COUNT <= 5 )); then
      INDICATORS+=("In-flight builder worktrees ($ACTIVE_WORKTREE_COUNT): ${ACTIVE_WORKTREE_NAMES[*]}")
    else
      FIRST_FIVE="${ACTIVE_WORKTREE_NAMES[*]:0:5}"
      INDICATORS+=("In-flight builder worktrees ($ACTIVE_WORKTREE_COUNT): $FIRST_FIVE …")
    fi
    DETECTED=1
  fi
fi

# ─────────────────────────────────────────────────────────────────────────────
# Output
# ─────────────────────────────────────────────────────────────────────────────
if (( DETECTED == 1 )); then
  {
    echo "Active Loom session detected in target: $TARGET_PATH"
    for line in "${INDICATORS[@]}"; do
      echo "  - $line"
    done
    echo "Reason: installing into a live system risks state corruption (concurrent worktree mutations, daemon races, lost work)."
  } >&2
  exit 1
fi

exit 0
