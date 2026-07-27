#!/usr/bin/env bash
# scripts/install/provision-daemon.sh — machine-level loom-daemon provisioning
#
# Issue #3922: a consumer repo ships `.loom/scripts/cli/loom-daemon-start.sh`
# but NO `loom-daemon` binary. That start script resolves the binary via:
#   LOOM_DAEMON_BIN env → `command -v loom-daemon` (PATH) →
#   <repo>/loom-daemon/target/release/loom-daemon → <repo>/target/release/…
# In a freshly-installed consumer repo NONE of these exist (no Rust source to
# build, nothing on PATH, LOOM_DAEMON_BIN unset), so autonomous daemon mode —
# the headline v0.14 feature — cannot start post-install.
#
# The v0.14.1 stopgap (toward the full machine-level install epic #3835):
# install the freshly-built binary to a machine-level location on PATH
# (~/.local/bin/loom-daemon), install-once per machine, shared across every
# consumer repo. The consumer side needs NO change — loom-daemon-start.sh
# already resolves via `command -v loom-daemon`.
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/provision-daemon.sh"
# then call `provision_machine_daemon <src_bin> [dest_dir]`.
#
# It is deliberately self-contained (defines its own output helpers) so the
# test suite can source it without pulling in the full installer.

# Emit a machine-level-provision status line. Prefixed so the installer's
# output stays scannable; plain text so `source`-ing tests can assert on it.
_pmd_info()    { echo "  [loom-daemon] $*"; }
_pmd_ok()      { echo "  [loom-daemon] $*"; }
_pmd_warn()    { echo "  [loom-daemon] WARNING: $*" >&2; }

# Set by provision_machine_daemon before every successful return so the caller
# can locate the destination it wrote to WITHOUT re-deriving the
# LOOM_DAEMON_BIN_DIR default itself (which would duplicate the fallback in two
# files). This is the "expose enough for the caller to verify" contract from
# issue #4053: a caller (loom-daemon-update.sh) reads $PROVISIONED_DAEMON_BIN to
# assert the destination binary is the expected build after provisioning — the
# direct fix for "provisioning reports success while shipping nothing". It is
# assigned as a GLOBAL (no `local`) precisely so it survives the function
# return, and is set even on the version-equality short-circuit path (the very
# path under suspicion, so it must NOT be the one that leaves it unset).
PROVISIONED_DAEMON_BIN=""

# provision_machine_daemon <src_bin> [dest_dir]
#
# Installs <src_bin> to <dest_dir>/loom-daemon (default: LOOM_DAEMON_BIN_DIR,
# else ~/.local/bin). Idempotent + version-aware: a no-op when the destination
# already holds the same `--version`. Best-effort — never fatal; returns 1 on a
# soft failure so the caller can note it, but the installer must NOT abort on a
# non-zero return (a repo can still run the daemon via an explicit
# LOOM_DAEMON_BIN or an in-repo build).
#
# On a successful return (0), sets the global PROVISIONED_DAEMON_BIN to the
# destination path it resolved (whether it copied or short-circuited), so the
# caller can verify the destination binary (#4053).
provision_machine_daemon() {
  local src_bin="${1:-}"
  local dest_dir="${2:-${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}}"
  local dest_bin="$dest_dir/loom-daemon"
  # Publish the resolved destination to the caller up front, so EVERY return
  # path below (including the short-circuit) communicates where the binary
  # lives — even the early soft-failure returns (the caller gates on the return
  # code, so a set-but-unprovisioned value there is harmless).
  PROVISIONED_DAEMON_BIN="$dest_bin"

  if [[ -z "$src_bin" || ! -x "$src_bin" ]]; then
    _pmd_warn "built binary not found at '${src_bin:-<unset>}'; skipping machine-level install"
    return 1
  fi

  local src_ver dest_ver
  src_ver=$("$src_bin" --version 2>/dev/null || echo "unknown")

  # Version-aware short-circuit: skip the copy when the destination already
  # holds the same version (compare `--version` strings).
  if [[ -x "$dest_bin" ]]; then
    dest_ver=$("$dest_bin" --version 2>/dev/null || echo "unknown")
    if [[ "$src_ver" == "$dest_ver" && "$src_ver" != "unknown" ]]; then
      _pmd_ok "already current at $dest_bin ($dest_ver)"
      _pmd_check_path "$dest_dir"
      return 0
    fi
  fi

  if ! mkdir -p "$dest_dir" 2>/dev/null; then
    _pmd_warn "could not create $dest_dir; skipping machine-level install"
    _pmd_warn "set LOOM_DAEMON_BIN=$src_bin in the consumer env to run the daemon"
    return 1
  fi

  # Prefer install(1) for the atomic mode-set; fall back to cp + chmod.
  if install -m 755 "$src_bin" "$dest_bin" 2>/dev/null || \
     { cp -f "$src_bin" "$dest_bin" 2>/dev/null && chmod 755 "$dest_bin" 2>/dev/null; }; then
    _pmd_ok "installed loom-daemon → $dest_bin ($src_ver)"
  else
    _pmd_warn "failed to install loom-daemon to $dest_bin"
    _pmd_warn "set LOOM_DAEMON_BIN=$src_bin in the consumer env to run the daemon"
    return 1
  fi

  _pmd_check_path "$dest_dir"
  return 0
}

# Warn (one clear line, never fatal) when <dir> is not on PATH, so the operator
# knows `command -v loom-daemon` will not resolve until they add it.
_pmd_check_path() {
  local dir="$1"
  case ":${PATH:-}:" in
    *":$dir:"*) return 0 ;;
    *)
      _pmd_warn "$dir is not on your PATH — add it so 'loom-daemon' resolves:"
      _pmd_warn "    export PATH=\"$dir:\$PATH\"   # add to ~/.zshrc or ~/.bashrc"
      return 0
      ;;
  esac
}
