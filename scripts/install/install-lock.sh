#!/usr/bin/env bash
# scripts/install/install-lock.sh — per-target install lock (issue #4928).
#
# Two installers running against the SAME target repository interleave
# destructively. The `--quick` reinstall path stages Loom file deletions and
# strips the Loom sections out of CLAUDE.md / .gitignore *in place* before
# writing the new payload; a second installer's copy phase landing inside that
# window can corrupt the target. In the reported incident (2026-08-01) the only
# thing that serialized two concurrent `--quick --confirm-reinstall` runs was
# cargo's build-directory lock — by luck, and only for the window in which both
# runs happened to be building. Nothing serialized the install phases on either
# side of the build.
#
# This helper adds the missing mutual exclusion: a single lock file at
#   <target>/.loom/.install.lock
# carrying the owning PID, the host, the start time, and the run's current
# PHASE. It is
#   - created with O_EXCL (`set -o noclobber`) so two racing installers cannot
#     both believe they hold it,
#   - released by install.sh's EXIT trap, so `error()`, a `set -e` abort,
#     Ctrl-C and SIGTERM all release it,
#   - reclaimed automatically when the recorded PID is no longer alive, so an
#     installer killed with SIGKILL never wedges the target permanently.
#
# The recorded phase does double duty as the "half-uninstalled window is
# explicit in state" marker: a lock left behind by a hard-killed run tells the
# next installer — and the operator — exactly which phase was interrupted, so
# the target is recoverable from the printed commands instead of a blind
# `git reset --hard`.
#
# Source with:
#     source "$LOOM_ROOT/scripts/install/install-lock.sh"
#
# Public functions:
#   acquire_install_lock <target> [phase]
#       Take the lock for <target>. Returns 0 on success (or when this process
#       already holds it — the call is idempotent and just updates the phase).
#       Returns 1 when another LIVE installer holds it, after printing who
#       holds it and how to recover. Callers turn that into a fatal error.
#
#   set_install_lock_phase <phase>
#       Record the current phase in the held lock. No-op without a lock.
#       Known phases: preparing | uninstalling | installing | restoring |
#       complete. The middle three are "destructive" — an interruption during
#       one of them can leave the target partially uninstalled.
#
#   release_install_lock
#       Remove the lock this process holds (never another process's). No-op
#       when no lock is held, so it is safe to call unconditionally — e.g.
#       immediately before an `exec`, which would otherwise strand the lock
#       (the EXIT trap does not survive `exec`).
#
#   install_lock_recovery_banner <phase> [target]
#       Print the "target may be partially uninstalled" recovery instructions
#       for a destructive phase. No-op for a non-destructive phase.
#
#   announce_destructive_window <target>
#       Print the explicit "entering the destructive uninstall→reinstall
#       window" notice before the uninstall runs.
#
#   install_lock_is_destructive_phase <phase>
#       Predicate: does an interruption in <phase> risk a half-uninstalled
#       target?

# Logging helpers. install.sh defines these; provide plain fallbacks so the
# library can also be sourced standalone (e.g. by its test suite).
if ! declare -f info >/dev/null 2>&1; then
  info() { echo "ℹ $*"; }
fi
if ! declare -f warning >/dev/null 2>&1; then
  warning() { echo "⚠ $*" >&2; }
fi

# Path of the lock this process currently holds ("" ⇒ none held).
INSTALL_LOCK_FILE=""
# Target the held lock belongs to.
INSTALL_LOCK_TARGET=""
# Phase recorded in the held lock.
INSTALL_LOCK_PHASE=""
# Whether acquire_install_lock created <target>/.loom itself (a fresh install
# into a target with no .loom/ yet). If so, release removes the directory again
# when it is empty — install.sh's reinstall gate keys off `-d "$TARGET/.loom"`,
# so a bare .loom/ left behind by a failed fresh install would make the next
# run demand --confirm-reinstall for an install that never happened.
INSTALL_LOCK_DIR_CREATED=false

# Age (in seconds) after which a lock owned by a DIFFERENT host is treated as
# abandoned. PID liveness is not probeable across hosts (a target on a shared
# filesystem), so age is the only available signal — deliberately generous, and
# overridable with LOOM_INSTALL_LOCK_MAX_AGE.
INSTALL_LOCK_FOREIGN_MAX_AGE="${LOOM_INSTALL_LOCK_MAX_AGE:-21600}"

_install_lock_host() {
  hostname 2>/dev/null || uname -n 2>/dev/null || echo "unknown-host"
}

# Read one `key=value` field out of a lock file. Empty output ⇒ absent.
_install_lock_field() {
  local field="$1" file="$2"
  [[ -r "$file" ]] || return 0
  sed -n "s/^${field}=//p" "$file" 2>/dev/null | head -n1
}

# Is <pid> a live process?
#
# `kill -0` also fails (EPERM) for a LIVE process owned by another user, so a
# negative result is confirmed with `ps` before the lock is declared stale —
# otherwise a second user's in-flight install would be silently stolen.
_install_lock_pid_alive() {
  local pid="${1:-}"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  [[ "$pid" -gt 0 ]] || return 1
  kill -0 "$pid" 2>/dev/null && return 0
  ps -p "$pid" >/dev/null 2>&1
}

install_lock_is_destructive_phase() {
  case "${1:-}" in
    uninstalling|installing|restoring) return 0 ;;
    *) return 1 ;;
  esac
}

# Render the lock payload on stdout.
_install_lock_render() {
  local phase="$1"
  echo "# Loom install lock (issue #4928)."
  echo "# Written by install.sh; removed automatically when the run exits."
  echo "# Safe to delete by hand ONLY if the pid below is no longer running."
  echo "pid=$$"
  echo "host=${INSTALL_LOCK_HOST:-unknown-host}"
  echo "started=${INSTALL_LOCK_STARTED:-unknown}"
  echo "started_epoch=${INSTALL_LOCK_STARTED_EPOCH:-0}"
  echo "phase=${phase}"
  echo "target=${INSTALL_LOCK_TARGET:-unknown}"
  echo "source=${LOOM_ROOT:-unknown}"
}

# Create the lock file atomically, or fail because it already exists.
#
# `set -o noclobber` makes the `>` redirection open with O_CREAT|O_EXCL, so
# exactly one of two racing installers can win. It runs in a subshell so the
# option never leaks into the caller's shell.
_install_lock_create() {
  local file="$1" phase="$2"
  ( set -o noclobber; _install_lock_render "$phase" > "$file" ) 2>/dev/null
}

# Print who holds the lock and how to proceed.
_install_lock_report_held() {
  local file="$1" target="$2" pid="$3" host="$4" phase="$5" started="$6"
  {
    echo ""
    echo "  Another Loom install is already running against this target."
    echo "    Target: $target"
    echo "    Lock:   $file"
    echo "    Held by pid ${pid:-unknown} on host ${host:-unknown}${started:+ (started $started)}"
    [[ -n "$phase" ]] && echo "    Phase:  $phase"
    echo ""
    echo "  Refusing to run two installers against the same target — their"
    echo "  uninstall and copy phases interleave destructively."
    echo ""
    echo "  Wait for that run to finish and retry. If you are certain the"
    echo "  process above is gone, remove the lock and retry:"
    echo "    rm -f \"$file\""
  } >&2
}

# Print recovery guidance for an interrupted destructive phase.
install_lock_recovery_banner() {
  local phase="${1:-}"
  local target="${2:-$INSTALL_LOCK_TARGET}"
  install_lock_is_destructive_phase "$phase" || return 0
  echo ""
  warning "The target may be left PARTIALLY UNINSTALLED (staged Loom file"
  warning "deletions, a stripped CLAUDE.md / .gitignore, or a partly written"
  warning ".loom/) — the '$phase' phase did not finish."
  echo ""
  echo "  Inspect and recover before retrying:"
  echo "    git -C \"$target\" status --short"
  echo "    git -C \"$target\" restore --staged --worktree -- .loom .claude CLAUDE.md .gitignore"
  echo "      # older git: git -C \"$target\" reset -q HEAD -- <paths> && git -C \"$target\" checkout -- <paths>"
  echo "    git -C \"$target\" stash list | grep loom-install   # changes this installer stashed, if any"
  echo ""
  echo "  Re-running the installer is also safe once the tree is restored."
  echo ""
}

# Print the explicit "destructive window opening" notice (issue #4928): the
# uninstall→reinstall sequence mutates the target's main checkout in place, so
# say so BEFORE it starts rather than leaving an interrupted run to be
# discovered as an unexplained half-uninstalled tree.
announce_destructive_window() {
  local target="$1"
  echo ""
  warning "Entering the destructive uninstall→reinstall window for $target"
  info "  The uninstall stages Loom file deletions and strips the Loom sections"
  info "  from CLAUDE.md / .gitignore in place; the target is only partially"
  info "  installed until this run completes."
  info "  Progress state: ${INSTALL_LOCK_FILE:-<no lock>}"
  info "  If this run is interrupted, the next installer reports the interrupted"
  info "  phase and the exact recovery commands (no blind 'git reset --hard')."
  echo ""
}

# Remove the lock if its owner is demonstrably gone. Returns 0 when the lock
# was reclaimed (caller may retry the create), 1 when it is genuinely held.
_install_lock_reclaim_if_stale() {
  local file="$1"

  # A racing installer creates the file and writes its payload in one
  # redirection, but a reader can still observe the (briefly) empty file
  # between the O_EXCL create and the write. Re-read a few times before
  # concluding the lock is malformed, so a live installer is never stolen from
  # in that microsecond window.
  local pid tries=0
  pid="$(_install_lock_field pid "$file")"
  while [[ -z "$pid" && $tries -lt 5 ]]; do
    sleep 0.2 2>/dev/null || sleep 1
    pid="$(_install_lock_field pid "$file")"
    tries=$((tries + 1))
  done

  local host phase started reason=""
  host="$(_install_lock_field host "$file")"
  phase="$(_install_lock_field phase "$file")"
  started="$(_install_lock_field started "$file")"

  if [[ -z "$pid" ]]; then
    reason="the lock file carries no owning pid"
  elif [[ -n "$host" && "$host" != "$(_install_lock_host)" ]]; then
    # Foreign host: PID liveness is unknowable, so fall back to age. Below the
    # cutoff the lock is respected (report it as held); above it, reclaimed.
    local started_epoch now age
    started_epoch="$(_install_lock_field started_epoch "$file")"
    [[ "$started_epoch" =~ ^[0-9]+$ ]] || started_epoch=0
    now="$(date +%s 2>/dev/null || echo 0)"
    age=$((now - started_epoch))
    if [[ $started_epoch -gt 0 && $age -gt $INSTALL_LOCK_FOREIGN_MAX_AGE ]]; then
      reason="it is owned by host '$host' and is ${age}s old (> ${INSTALL_LOCK_FOREIGN_MAX_AGE}s)"
    else
      return 1
    fi
  elif _install_lock_pid_alive "$pid"; then
    return 1
  else
    reason="owning pid $pid is no longer running"
  fi

  warning "Reclaiming stale Loom install lock — $reason"
  info "  Lock: $file"
  if install_lock_is_destructive_phase "$phase"; then
    warning "That run was interrupted during the '$phase' phase."
    install_lock_recovery_banner "$phase" "$INSTALL_LOCK_TARGET"
  fi
  rm -f "$file" 2>/dev/null || return 1
  return 0
}

# Take the per-target install lock. See the header for the contract.
acquire_install_lock() {
  local target="$1"
  local phase="${2:-preparing}"

  # Idempotent: an already-held lock just moves to the requested phase.
  if [[ -n "$INSTALL_LOCK_FILE" ]]; then
    set_install_lock_phase "$phase"
    return 0
  fi

  local lock_dir="$target/.loom"
  local file="$lock_dir/.install.lock"

  if [[ ! -d "$lock_dir" ]]; then
    mkdir -p "$lock_dir" 2>/dev/null || {
      warning "Cannot create $lock_dir — install lock unavailable"
      return 1
    }
    INSTALL_LOCK_DIR_CREATED=true
  fi

  INSTALL_LOCK_TARGET="$target"
  INSTALL_LOCK_HOST="$(_install_lock_host)"
  INSTALL_LOCK_STARTED="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
  INSTALL_LOCK_STARTED_EPOCH="$(date +%s 2>/dev/null || echo 0)"

  local attempt
  for attempt in 1 2; do
    if _install_lock_create "$file" "$phase"; then
      INSTALL_LOCK_FILE="$file"
      INSTALL_LOCK_PHASE="$phase"
      return 0
    fi
    # Lost the create: the lock exists. Reclaim it if its owner is gone, then
    # retry exactly once (a third installer may have won the reclaimed slot).
    if [[ $attempt -eq 1 ]] && _install_lock_reclaim_if_stale "$file"; then
      continue
    fi
    break
  done

  _install_lock_report_held "$file" "$target" \
    "$(_install_lock_field pid "$file")" \
    "$(_install_lock_field host "$file")" \
    "$(_install_lock_field phase "$file")" \
    "$(_install_lock_field started "$file")"

  # Never clean up a directory we may have created for a lock we do not own —
  # a concurrent installer is using it.
  INSTALL_LOCK_DIR_CREATED=false
  return 1
}

# Record the current phase in the held lock (tmp+rename so a concurrent reader
# never observes a torn file, and the path never briefly disappears).
set_install_lock_phase() {
  local phase="$1"
  INSTALL_LOCK_PHASE="$phase"
  [[ -n "$INSTALL_LOCK_FILE" ]] || return 0
  local tmp="${INSTALL_LOCK_FILE}.tmp"
  if _install_lock_render "$phase" > "$tmp" 2>/dev/null; then
    mv -f "$tmp" "$INSTALL_LOCK_FILE" 2>/dev/null || rm -f "$tmp" 2>/dev/null || true
  else
    rm -f "$tmp" 2>/dev/null || true
  fi
  return 0
}

# Release the lock held by THIS process. Safe to call unconditionally.
release_install_lock() {
  local file="$INSTALL_LOCK_FILE"
  [[ -n "$file" ]] || return 0
  INSTALL_LOCK_FILE=""
  # shellcheck disable=SC2034  # read by install.sh's EXIT trap, not here
  INSTALL_LOCK_PHASE=""

  # Only remove a lock we still own: if the file was reclaimed by another
  # installer (e.g. this process was stopped long enough to look dead), that
  # installer's lock must survive.
  local owner
  owner="$(_install_lock_field pid "$file")"
  if [[ -n "$owner" && "$owner" != "$$" ]]; then
    return 0
  fi

  rm -f "$file" 2>/dev/null || true
  if [[ "$INSTALL_LOCK_DIR_CREATED" == true ]]; then
    rmdir "$(dirname "$file")" 2>/dev/null || true
    INSTALL_LOCK_DIR_CREATED=false
  fi
  return 0
}
