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

  # Epic #4990 Phase 3 (#5020): never force-resign a binary that already
  # carries a REAL, certificate-backed signature -- e.g. a fetched release
  # artifact signed in CI with the project's Developer ID (Phase 2,
  # #5011/#5018). `codesign -f` below UNCONDITIONALLY REPLACES whatever
  # signature is present, and an ad-hoc re-sign has no certificate chain, so
  # forcing it here would silently downgrade a Developer ID signature to
  # ad-hoc on every provision -- exactly the Gatekeeper regression Phase 2
  # exists to avoid. `codesign -dvvv` prints an `Authority=` line only for a
  # certificate-backed signature; a LOCALLY BUILT binary (the pre-#4990 case
  # this function was originally written for) never has one -- cargo's own
  # linker-applied signature is always ad-hoc -- so this check is a no-op for
  # that path and changes nothing there.
  if codesign -dvvv "$bin" 2>&1 | grep -q '^Authority='; then
    _pmd_ok "already signed with a real certificate (not re-signing): $bin"
    return 0
  fi

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
# now that their Python console-script entry points are removed. Issue #4275
# (family 5) adds `loom-claim` for the same reason, but with a stronger
# requirement: that name is not muscle memory, it is an AGENT-FACING contract —
# `builder-worktree.md` instructs builders to run `loom-claim claim <issue>
# <agent-id>` and branches on its exit codes, behind a `command -v loom-claim`
# guard that would silently disable file-based claiming if the name vanished.
# Each shim is a tiny script that execs `loom-daemon <daemon_subcommand> "$@"` —
# resolved via `dest_dir` at call time (not baked in), so a later daemon
# rebuild/reprovision at the same path is picked up automatically.
#
# Best-effort and never fatal: a write failure here must not fail the
# broader daemon provisioning (the shim is muscle-memory convenience, not
# load-bearing — `./.loom/scripts/clean.sh` etc. resolve the daemon binary
# independently via `lib/locate-daemon-bin.sh`).
#
# Issue #5386: every install used to log a bare, non-actionable
# "$shim_path: No such file or directory" for all three shims (never
# self-healing across repeated installs). Root-caused to TWO compounding
# problems in the version-match short-circuit branch of
# provision_machine_daemon (the "already current at ..." path, which is what
# every reported repro hit):
#   1. That branch never calls `mkdir -p "$dest_dir"` — unlike the
#      fresh-install branch just below it — so it silently assumes dest_dir
#      already exists. That assumption can be violated by anything that
#      removes/recreates the directory between installs (a stale PATH
#      cleanup, a synced-folder placeholder, manual `rm -rf ~/.local/bin`,
#      etc.), and when it is, the shim write is the first thing in the whole
#      function to touch that directory — so it is the first (and only)
#      thing to fail.
#   2. The failure path here gave the CALLER (a `cat > file <<EOF` heredoc)
#      no chance to explain itself: bash's own redirection-setup error
#      ("No such file or directory") reaches the terminal raw, a full line
#      before this function's own `_pmd_warn` even runs, with zero context
#      about *why* the destination path was unwritable.
# Fixed by re-asserting the dest_dir/writability invariants on EVERY call
# (self-healing a missing directory instead of assuming a sibling branch
# already created it) and by naming the actual failure mode in the warning.
# The heredoc is also replaced with a `printf`-based writer: no functional
# difference, but it removes any dependency on a bash build correctly
# parsing a multi-byte character embedded in heredoc body content (the
# em dash in the comment two lines below, in the ORIGINAL version of this
# shim body) — one less variable when diagnosing a future report like this.
_pmd_install_shim() {
  local shim_name="$1" subcommand="$2" dest_dir="$3"
  local shim_path="$dest_dir/$shim_name"

  if [[ ! -d "$dest_dir" ]] && ! mkdir -p "$dest_dir" 2>/dev/null; then
    _pmd_warn "skipping $shim_name shim: $dest_dir does not exist and could not be created (non-fatal — loom-daemon itself is still installed; re-run install once that directory is writable to repair this)"
    return 0
  fi

  if [[ ! -w "$dest_dir" ]]; then
    _pmd_warn "skipping $shim_name shim: $dest_dir is not writable (non-fatal — check permissions on that directory, then re-run install to repair)"
    return 0
  fi

  # A pre-#4971 host has ~/.local/bin/loom-* symlinked into the retired
  # loom-tools venv. `>` follows a dangling symlink to its missing target and
  # fails, so the shim could never self-heal. Unlink first (#5386 fixed only
  # the dest_dir causes of the identical error message).
  rm -f "$shim_path"

  if printf '%s\n' \
      '#!/usr/bin/env bash' \
      '# Auto-generated PATH shim (issue #4272) -- do not edit by hand.' \
      '# Regenerated by scripts/install/provision-daemon.sh alongside loom-daemon.' \
      "exec \"\$(dirname \"\$0\")/loom-daemon\" $subcommand \"\$@\"" \
      > "$shim_path" 2>/dev/null
  then
    chmod 755 "$shim_path" 2>/dev/null || true
  else
    _pmd_warn "failed to write $shim_name shim at $shim_path (non-fatal — loom-daemon itself is still installed; re-run install to repair)"
  fi
}

# ---------------------------------------------------------------------------
# `loom-*` PATH-name disposition register (issue #5738)
#
# A pre-#4971 `loom-tools` pip install symlinked FOURTEEN `loom-*` names in
# `~/.local/bin` into `<repo>/loom-tools/.venv/bin/`. #4971 retired that venv,
# which turned every one of those links into a dangling PATH entry. Dangling
# entries are a WORSE failure mode than a missing command: `command -v
# loom-status` still succeeds in some lookups while execution dies with
# "No such file or directory".
#
# Every one of the fourteen must appear in exactly one of the two registers
# below, so the population can never silently regrow:
#
#   _PMD_MANAGED_SHIMS       — mapped to a live `loom-daemon` subcommand and
#                              re-provisioned on every install (self-heals).
#   _PMD_RETIRED_SHIM_NAMES  — no subcommand home, nothing regenerates them;
#                              removed by `_pmd_cleanup_retired_shims` below.
#
# Keep both in sync with the disposition table in
# `docs/migration/daemon-state-consumers.md` ("`loom-*` PATH shims").
# ---------------------------------------------------------------------------

# `<shim name>:<loom-daemon subcommand>` pairs. This IS the install list —
# `_pmd_install_managed_shims` loops it, so a name can only become "managed"
# by being recorded here (bash 3.2 has no associative arrays, hence the
# colon-delimited pairs).
_PMD_MANAGED_SHIMS=(
  "loom-clean:clean"
  "loom-recover-orphans:recover-orphans"
  "loom-claim:claim"
)

# The eleven names the loom-tools retirement stranded with no subcommand home.
_PMD_RETIRED_SHIM_NAMES=(
  loom-agent-monitor
  loom-auto-merge
  loom-baseline-health
  loom-check-completions
  loom-cleanup
  loom-daemon-diagnostic
  loom-forge
  loom-health-monitor
  loom-status
  loom-stuck-detection
  loom-worktree
)

# _pmd_install_managed_shims <dest_dir>
#
# Install every _PMD_MANAGED_SHIMS entry. Single call site for the whole
# managed set so the register above cannot drift from what is actually
# written to disk.
_pmd_install_managed_shims() {
  local dest_dir="$1" entry
  for entry in "${_PMD_MANAGED_SHIMS[@]}"; do
    _pmd_install_shim "${entry%%:*}" "${entry#*:}" "$dest_dir"
  done
}

# _pmd_is_managed_shim_name <name> / _pmd_is_retired_shim_name <name>
_pmd_is_managed_shim_name() {
  local name="$1" entry
  for entry in "${_PMD_MANAGED_SHIMS[@]}"; do
    [[ "${entry%%:*}" == "$name" ]] && return 0
  done
  return 1
}

_pmd_is_retired_shim_name() {
  local name="$1" retired
  for retired in "${_PMD_RETIRED_SHIM_NAMES[@]}"; do
    [[ "$retired" == "$name" ]] && return 0
  done
  return 1
}

# _pmd_is_orphaned_venv_shim <path>
#
# True iff <path> is PROVABLY dead weight left behind by the loom-tools venv
# retirement (#4971). All three conditions must hold:
#
#   1. it is a SYMLINK — never a regular file, so a user-authored script that
#      happens to be named `loom-something` is untouchable by construction;
#   2. it is DANGLING (`-e` false: the target no longer exists) — a symlink
#      into a venv that still exists still works, and is left alone;
#   3. its recorded target threads through a `loom-tools/.venv` directory —
#      the retired venv's own unmistakable shape, so a dangling symlink into
#      some unrelated location is also left alone.
#
# This is the safety guardrail behind the acceptance criterion "never a
# user-authored script that happens to be named `loom-*`".
_pmd_is_orphaned_venv_shim() {
  local path="$1"
  [[ -L "$path" ]] || return 1
  [[ -e "$path" ]] && return 1
  local target
  target="$(readlink "$path" 2>/dev/null)" || return 1
  case "$target" in
    */loom-tools/.venv/*) return 0 ;;
    *) return 1 ;;
  esac
}

# _pmd_list_orphaned_venv_shims <dest_dir>
#
# Print (one absolute path per line) every `loom-*` entry in <dest_dir> that
# `_pmd_is_orphaned_venv_shim` proves is a dead loom-tools/.venv link. Scans
# the actual directory rather than only the recorded names, so an unrecorded
# straggler is still found — `_pmd_cleanup_retired_shims` flags those loudly.
# Managed names are skipped outright: `_pmd_install_shim` owns them and
# repairs a dangling one in place (#5708).
_pmd_list_orphaned_venv_shims() {
  local dest_dir="$1"
  [[ -n "$dest_dir" && -d "$dest_dir" ]] || return 0

  local path name
  for path in "$dest_dir"/loom-*; do
    # Unmatched glob expands to the literal pattern; -L keeps dangling
    # symlinks (for which -e is false) in scope.
    [[ -e "$path" || -L "$path" ]] || continue
    name="${path##*/}"
    _pmd_is_managed_shim_name "$name" && continue
    _pmd_is_orphaned_venv_shim "$path" || continue
    printf '%s\n' "$path"
  done
}

# _pmd_cleanup_retired_shims <dest_dir>
#
# Issue #5738: unlink the dead `loom-*` links found by
# `_pmd_list_orphaned_venv_shims`. Best-effort and never fatal, matching every
# other helper in this file: a permission failure here must not fail the
# broader provisioning (or uninstall) run it is called from.
_pmd_cleanup_retired_shims() {
  local dest_dir="$1"
  [[ -n "$dest_dir" && -d "$dest_dir" ]] || return 0

  local path name
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    name="${path##*/}"
    if ! _pmd_is_retired_shim_name "$name"; then
      _pmd_warn "removing UNRECORDED dead loom-tools/.venv shim $name — add it to _PMD_RETIRED_SHIM_NAMES in scripts/install/provision-daemon.sh and to the disposition table in docs/migration/daemon-state-consumers.md"
    fi
    if rm -f "$path" 2>/dev/null; then
      _pmd_ok "removed retired shim $name (dangling loom-tools/.venv symlink from the pre-#4971 install; #5738)"
    else
      _pmd_warn "failed to remove retired shim $name at $path (non-fatal — delete it by hand to clear the broken PATH entry)"
    fi
  done < <(_pmd_list_orphaned_venv_shims "$dest_dir")
  return 0
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

# _pmd_defaults_dest_dir
#
# Resolves the machine-level location `loom-daemon`'s own `defaults/`
# payload is mirrored to when installed standalone — i.e. with no on-host
# `loom` git checkout for `loom-daemon init` to find via cwd or git-root
# search (Issue #5389; see `resolve_defaults_path()` /
# `MACHINE_DEFAULTS_REL` in `loom-daemon/src/init/git.rs`, which reads
# from this exact location).
#
# Deliberately DISTINCT from `${LOOM_HOME:-$HOME/.local/share/loom}` — the
# FULL machine checkout `provision_loom_dispatcher()` (provision-dispatcher.sh)
# symlinks there — so this narrower payload copy can never collide with, or
# get silently shadowed/left-stale-forever by, that symlink management.
_pmd_defaults_dest_dir() {
  echo "${LOOM_DAEMON_DEFAULTS_DIR:-$HOME/.local/share/loom-daemon/defaults}"
}

# _pmd_provision_defaults_payload <defaults_src_dir>
#
# Best-effort, idempotent mirror of <defaults_src_dir> to
# `_pmd_defaults_dest_dir()`, giving a standalone `loom-daemon` install (no
# on-host `loom` git checkout) a working `loom-daemon init` recovery path
# (Issue #5389). A missing/empty <defaults_src_dir> (e.g. the self-update
# caller in loom-daemon-update.sh, which has no source tree to mirror from)
# silently no-ops — this is optional, not every caller has a payload to
# offer, and the binary provisioning above this call is the load-bearing
# artifact.
#
# Prefers `rsync -a --delete` (handles stale-file deletion cleanly); falls
# back to a copy-to-temp-then-atomic-rename when rsync is unavailable, so a
# failed copy never leaves a half-written destination. Never fatal: returns
# 1 on a soft failure, but the caller must not abort on it.
_pmd_provision_defaults_payload() {
  local src="${1:-}"
  [[ -n "$src" && -d "$src" ]] || return 0

  local dest
  dest="$(_pmd_defaults_dest_dir)"

  # Sanity guard before any destructive filesystem operation below: refuse
  # unless dest resolved to a non-empty path ending in the expected
  # 'defaults' leaf. Belt-and-braces against a future edit (or a mis-set
  # LOOM_DAEMON_DEFAULTS_DIR) turning this into an unintended wide target.
  case "$dest" in
    */defaults) ;;
    *)
      _pmd_warn "refusing to provision defaults payload: unexpected destination '$dest'"
      return 1
      ;;
  esac

  if ! mkdir -p "$(dirname "$dest")" 2>/dev/null; then
    _pmd_warn "could not create $(dirname "$dest"); skipping defaults payload provisioning"
    return 1
  fi

  if command -v rsync >/dev/null 2>&1; then
    if ! rsync -a --delete "$src/" "$dest/" 2>/dev/null; then
      _pmd_warn "failed to mirror defaults payload to $dest"
      return 1
    fi
  else
    local tmp_dest="${dest}.tmp.$$"
    rm -rf "$tmp_dest" 2>/dev/null
    if ! cp -R "$src" "$tmp_dest" 2>/dev/null; then
      rm -rf "$tmp_dest" 2>/dev/null
      _pmd_warn "failed to mirror defaults payload to $dest"
      return 1
    fi
    rm -rf "$dest" 2>/dev/null
    if ! mv "$tmp_dest" "$dest" 2>/dev/null; then
      rm -rf "$tmp_dest" 2>/dev/null
      _pmd_warn "failed to install defaults payload at $dest"
      return 1
    fi
  fi

  _pmd_ok "mirrored defaults payload -> $dest (standalone-install recovery path for 'loom-daemon init', #5389)"
  return 0
}

# provision_machine_daemon <src_bin> [dest_dir] [defaults_src_dir]
#
# Installs <src_bin> to <dest_dir>/loom-daemon (default: LOOM_DAEMON_BIN_DIR,
# else ~/.local/bin). Idempotent + version-aware: a no-op when the destination
# already holds the same `--version`. Best-effort — never fatal; returns 1 on a
# soft failure so the caller can note it, but the installer must NOT abort on a
# non-zero return (a repo can still run the daemon via an explicit
# LOOM_DAEMON_BIN or an in-repo build).
#
# When <defaults_src_dir> is given and exists, it is ALSO mirrored to a
# machine-level location (`_pmd_defaults_dest_dir`) so `loom-daemon init`
# has a working recovery path even on a host with no on-host `loom` git
# checkout (Issue #5389). Omit it (or pass "") when no `defaults/` payload
# is available to the caller (e.g. a self-update from a downloaded release
# artifact) — that is not an error, it just skips this optional step.
#
# On a successful return (0), sets the global PROVISIONED_DAEMON_BIN to the
# destination path it resolved (whether it copied or short-circuited), so the
# caller can verify the destination binary (#4053).
provision_machine_daemon() {
  local src_bin="${1:-}"
  local dest_dir="${2:-${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}}"
  local defaults_src_dir="${3:-}"
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
      _pmd_install_managed_shims "$dest_dir"
      _pmd_cleanup_retired_shims "$dest_dir"
      _pmd_provision_defaults_payload "$defaults_src_dir"
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
    _pmd_install_managed_shims "$dest_dir"
    _pmd_cleanup_retired_shims "$dest_dir"
    _pmd_provision_defaults_payload "$defaults_src_dir"
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
