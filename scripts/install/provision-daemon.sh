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

# _pmd_resolve_codesign_identity
#
# Issue #4244: resolve an optional STABLE codesign identity (env > config >
# default, the repo's standard precedence — see spawn-worker.sh's RUNTIME
# resolution for the same shape) so a self-signed certificate can be used in
# place of ad-hoc signing, letting macOS TCC anchor a designated requirement
# to the certificate rather than a per-build cdhash (see sign_daemon_binary's
# doc comment below for why that distinction matters).
#
#   1. $LOOM_CODESIGN_IDENTITY (env) — highest precedence.
#   2. `codesign.identity` in the resolved config (.loom/config.json /
#      .loom-project/project.json / .loom-local/local.json), read via the
#      shared config-resolver.sh when it can be located and `jq` is present.
#      Resolved relative to $LOOM_ROOT (if the caller exported it) else the
#      git toplevel of $PWD; soft-skipped when neither resolves.
#   3. Empty (default) — the caller falls back to ad-hoc signing.
#
# Echoes the resolved identity (possibly empty). Never fails the caller: any
# missing piece (no repo root, no config-resolver.sh, no jq) soft-skips to
# the next tier, exactly like loom_config_get's own soft-fail contract.
_pmd_resolve_codesign_identity() {
  if [[ -n "${LOOM_CODESIGN_IDENTITY:-}" ]]; then
    printf '%s' "$LOOM_CODESIGN_IDENTITY"
    return 0
  fi

  local repo_root="${LOOM_ROOT:-}"
  if [[ -z "$repo_root" ]]; then
    repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
  fi
  [[ -n "$repo_root" ]] || return 0

  local lib candidate
  for candidate in \
    "$repo_root/.loom/scripts/lib/config-resolver.sh" \
    "$repo_root/defaults/scripts/lib/config-resolver.sh"; do
    if [[ -r "$candidate" ]]; then
      lib="$candidate"
      break
    fi
  done
  [[ -n "${lib:-}" ]] || return 0

  # shellcheck source=/dev/null
  source "$lib"
  declare -F loom_config_get >/dev/null 2>&1 || return 0
  loom_config_get "$repo_root" "codesign.identity" ""
}

# sign_daemon_binary <bin>
#
# Issue #4016: ad-hoc-sign a freshly built/installed `loom-daemon` binary with
# a STABLE identifier (`com.rjwalters.loom-daemon`) instead of the rustc
# `-C metadata` hash cargo bakes in by default (e.g.
# `loom_daemon-72d9e1b56839d6c3`, which changes on every version bump). That
# hash surfaces in `codesign -dv` output, in System Settings -> Privacy &
# Security entries, and in any future crash/signing diagnostic — pinning a
# human-legible identifier there is cheap and hermetic.
#
# IMPORTANT — plain ad-hoc signing does NOT make a TCC grant survive a
# rebuild. An ad-hoc signature has no certificate chain for codesign to
# anchor a designated requirement to, so it falls back to a cdhash-only DR
# regardless of what --identifier is passed; a rebuild that changes any byte
# of the binary (which a self-update roll always does, since build.rs embeds
# the git commit and build time) produces a new cdhash and orphans any grant
# just as an unsigned binary would. See .loom/docs/daemon-reference.md's
# "Ad-hoc code signing" section for the measured proof.
#
# Issue #4244: when $LOOM_CODESIGN_IDENTITY (or the `codesign.identity`
# config key) names an identity present in the keychain
# (`security find-identity -v -p codesigning`), sign with THAT identity
# instead — a real certificate chain gives codesign a stable designated
# requirement, so a TCC grant made to the resulting binary survives a
# rebuild/reprovision (the identity, not the cdhash, is what's pinned). See
# defaults/docs/macos-tcc-codesign.md for the one-time cert setup. This is
# opt-in only: unset (or an identity the keychain doesn't have) falls back to
# the ad-hoc path below, unchanged.
#
# Darwin-only, best-effort, and NEVER fatal: the linker-signed ad-hoc
# signature the binary already carries (from `cargo build`) is sufficient to
# run, so an absent `codesign`, a non-Darwin host, or a `codesign` failure
# must never fail the caller's build/provision step — this function always
# returns 0.
sign_daemon_binary() {
  local bin="${1:-}"

  [[ -n "$bin" && -x "$bin" ]] || return 0
  [[ "$(uname -s 2>/dev/null)" == "Darwin" ]] || return 0
  command -v codesign >/dev/null 2>&1 || return 0

  local identity
  identity="$(_pmd_resolve_codesign_identity)"

  if [[ -n "$identity" ]] && command -v security >/dev/null 2>&1 \
      && security find-identity -v -p codesigning 2>/dev/null | grep -qF "$identity"; then
    if codesign -f -s "$identity" --identifier com.rjwalters.loom-daemon "$bin" 2>/dev/null; then
      _pmd_ok "signed $bin with identity '$identity' (identifier=com.rjwalters.loom-daemon) — TCC grants survive rebuilds"
      return 0
    fi
    _pmd_warn "codesign with identity '$identity' failed for $bin; falling back to ad-hoc signing"
  elif [[ -n "$identity" ]]; then
    _pmd_warn "LOOM_CODESIGN_IDENTITY '$identity' not found via 'security find-identity -v -p codesigning'; falling back to ad-hoc signing (see defaults/docs/macos-tcc-codesign.md)"
  fi

  if codesign -f -s - --identifier com.rjwalters.loom-daemon "$bin" 2>/dev/null; then
    _pmd_ok "ad-hoc signed $bin (identifier=com.rjwalters.loom-daemon)"
  else
    _pmd_warn "codesign failed for $bin (non-fatal; the binary's existing linker-signed ad-hoc signature is still sufficient to run)"
  fi
  return 0
}

# _pmd_install_shim <shim_name> <daemon_subcommand> <dest_dir>
#
# Issue #4272 (epic #4081 Phase 3 family 2): install a thin PATH shim next to
# the provisioned `loom-daemon` binary so operator muscle-memory commands
# (`loom-clean`, `loom-recover-orphans`) keep working with zero pip installs
# now that their Python console-script entry points are removed. Each shim
# is a tiny script that execs `loom-daemon <daemon_subcommand> "$@"` —
# resolved via `dest_dir` at call time (not baked in), so a later daemon
# rebuild/reprovision at the same path is picked up automatically.
#
# Best-effort and never fatal: a write failure here must not fail the
# broader daemon provisioning (the shim is muscle-memory convenience, not
# load-bearing — `./.loom/scripts/clean.sh` etc. resolve the daemon binary
# independently via `lib/locate-daemon-bin.sh`).
_pmd_install_shim() {
  local shim_name="$1" subcommand="$2" dest_dir="$3"
  local shim_path="$dest_dir/$shim_name"
  if cat > "$shim_path" <<SHIM_EOF
#!/usr/bin/env bash
# Auto-generated PATH shim (issue #4272) — do not edit by hand.
# Regenerated by scripts/install/provision-daemon.sh alongside loom-daemon.
exec "\$(dirname "\$0")/loom-daemon" $subcommand "\$@"
SHIM_EOF
  then
    chmod 755 "$shim_path" 2>/dev/null || true
  else
    _pmd_warn "failed to install $shim_name shim at $shim_path (non-fatal)"
  fi
}

# _pmd_is_real_binary <path>
#
# Issue #4397 (deferred from #4381's incident review, PR #4396): a `file(1)`-based
# sanity check that <path> is an actual compiled executable (Mach-O on Darwin,
# ELF on Linux) rather than a shell script, text file, or other non-binary
# masquerading as the daemon. Matches on the `Mach-O` / `ELF` substrings that
# `file -b` emits for every architecture/variant this repo ships for (universal
# binaries, PIE executables, shared objects, etc. all still contain one of
# those two tokens); a shell script instead reports "... script text
# executable" and a plain text file reports "ASCII text" — neither matches.
#
# Soft-passes (returns 0) when `file` itself is unavailable, rather than
# blocking an install on a missing diagnostic tool that has nothing to do with
# the binary's actual validity — the pre-existing `-x` executable-bit check in
# the caller is still enforced regardless.
_pmd_is_real_binary() {
  local path="$1"
  command -v file >/dev/null 2>&1 || return 0
  local desc
  desc="$(file -b "$path" 2>/dev/null)"
  case "$desc" in
    *Mach-O*|*ELF*) return 0 ;;
    *) return 1 ;;
  esac
}

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

  # Binary-format sanity gate (#4397, deferred from #4381's incident review):
  # refuse to install anything that isn't a real compiled binary to the
  # machine-level daemon path. #4396 sandboxed + checksum-guarded the TEST
  # SUITE's own fixtures from ever touching the real destination; this gate
  # protects every CALLER of this function (the installer, self-update, any
  # future script) so a shell script, text file, or other non-binary can never
  # be installed as `loom-daemon`, regardless of caller. LOOM_PROVISION_ALLOW_SCRIPT=1
  # is an explicit, auditable test-only bypass — set suite-wide by
  # tests/install/test-provision-daemon.sh and
  # defaults/scripts/tests/test-loom-daemon-update.sh (whose fixture "daemon"
  # stand-ins are bash scripts standing in for the real compiled binary);
  # production callers (scripts/install-loom.sh,
  # defaults/scripts/cli/loom-daemon-update.sh) never set it.
  if [[ -z "${LOOM_PROVISION_ALLOW_SCRIPT:-}" ]] && ! _pmd_is_real_binary "$src_bin"; then
    _pmd_warn "refusing to install '$src_bin': not a compiled binary (Mach-O/ELF executable expected)"
    _pmd_warn "  file(1) reports: $(file -b "$src_bin" 2>/dev/null || echo '<file unavailable>')"
    _pmd_warn "  if this is a deliberate test fixture standing in for the real daemon binary, set LOOM_PROVISION_ALLOW_SCRIPT=1"
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
      _pmd_install_shim "loom-clean" "clean" "$dest_dir"
      _pmd_install_shim "loom-recover-orphans" "recover-orphans" "$dest_dir"
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
    # Belt-and-braces (#4016): the source binary passed to this function is
    # signed by loom-daemon-update.sh's own signing step before it gets here,
    # but this covers the installer-only path (install.sh / install-loom.sh),
    # which never goes through loom-daemon-update.sh. Never fatal.
    sign_daemon_binary "$dest_bin"
    _pmd_install_shim "loom-clean" "clean" "$dest_dir"
    _pmd_install_shim "loom-recover-orphans" "recover-orphans" "$dest_dir"
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
