#!/usr/bin/env bash
# Loom Setup - Install Loom into a target repository
# Usage: ./install.sh [OPTIONS] [/path/to/target-repo]
#
# Options:
#   -y, --yes                  Non-interactive mode (skip confirmation prompts)
#   --quick                    Quick Install - direct install without GitHub workflow
#   --full                     Full Install - creates issue, worktree, and PR
#   --confirm-reinstall        Acknowledge a destructive reinstall over an existing
#                              .loom/ install. Required alongside --quick/--yes/--full
#                              when the target already has Loom installed -- without
#                              it, non-interactive runs stop and ask you to inventory
#                              customizations first (interactive runs still get a
#                              y/N prompt instead). If you only want to bring an
#                              existing install's surfaces up to date -- not replace
#                              the payload -- use the non-destructive
#                              .loom/scripts/resync-installed.sh in the target repo
#                              instead; --confirm-reinstall uninstalls before
#                              reinstalling.
#   --allow-non-main-source    Permit installing from a non-main / detached-HEAD Loom source
#                              (forwarded to scripts/install-loom.sh)
#   --allow-stale-target       Permit installing over a target whose Loom is newer/stale
#                              (forwarded to scripts/install-loom.sh)
#   -h, --help                 Show this help message
#
# Examples:
#   ./install.sh --quick ~/projects/my-app
#   ./install.sh --full /path/to/team-project
#   ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick
#   ./install.sh --quick --confirm-reinstall ~/projects/my-app  # Reinstall over an existing install

set -euo pipefail

# Handle Ctrl-C and SIGTERM during interactive prompts
trap 'echo ""; echo -e "\033[0;34mℹ Installation cancelled\033[0m"; exit 130' SIGINT
trap 'exit 143' SIGTERM

# ANSI color codes
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

error() {
  echo -e "${RED}✗ Error: $*${NC}" >&2
  exit 1
}

info() {
  echo -e "${BLUE}ℹ $*${NC}"
}

success() {
  echo -e "${GREEN}✓ $*${NC}"
}

warning() {
  echo -e "${YELLOW}⚠ $*${NC}"
}

header() {
  echo -e "${CYAN}$*${NC}"
}

# Detect whether the canonical Repo Skills generic guard is present in the
# target repo AND passes BOTH runtime probes the guard-destructive.sh
# dispatcher requires (issue #4041, #4894):
#   1. VERSION probe — carries the rjwalters/repo#29 curl-pipe fix (the
#      marker comment stands in for "has the fix", so no version arithmetic
#      is needed).
#   2. CAPABILITY probe — also carries the `worktree-write-confinement`
#      decision tag, i.e. actually implements the Loom-only Bash-tool
#      write-confinement category (issue #4178), not just the unrelated
#      repo#29 fix.
# Both are required, matching the identical two-probe runtime check in the
# guard-destructive.sh dispatcher, so install-time and runtime always agree on
# which guard wins — and, critically, so this function never treats the
# vendored copy as installable-skippable while the dispatcher would still fall
# back to it (#4894: before this, a canonical guard with repo#29 but without
# write-confinement caused the vendored fallback to be skipped/removed at
# install time even though the dispatcher needed it).
canonical_guard_present() {
  local target="$1"
  local canonical="$target/.claude/skills/repo/hooks/guard-destructive.sh"
  [[ -r "$canonical" ]] \
    && grep -q 'repo#29' "$canonical" 2>/dev/null \
    && grep -q 'worktree-write-confinement' "$canonical" 2>/dev/null
}

# Install hooks and CLI wrapper that loom-daemon init doesn't handle.
#
# Issue #3625: an existing hook may be a downstream-tuned or forked copy — most
# notably a customized guard-destructive.sh with a hand-tuned rm allowlist — so
# it must NOT be silently clobbered on the quick-install/update path. Preserve
# any existing .loom/hooks/<name> unless an explicit force overwrite is
# requested (the caller passes "true", e.g. behind --clean). This mirrors the
# preserve-unless-force behavior already in scripts/install-loom.sh:1099-1116,
# which the quick path previously diverged from with an unconditional cp.
#
# Issue #4041: guard-destructive-generic.sh is the vendored copy of Repo Skills'
# canonical generic guard. When the canonical guard is present (with the
# rjwalters/repo#29 fix), Loom does NOT install its own generic copy — the
# guard-destructive.sh dispatcher defers to the canonical guard at runtime. Any
# stale vendored copy from a prior install is removed so the repo converges to
# "canonical only". When the canonical guard is absent (standalone-Loom repo),
# the vendored copy IS installed so destructive-command coverage is never lost.
install_hooks_and_cli() {
  local loom_root="$1"
  local target="$2"
  local force="${3:-false}"

  local canonical_present=false
  if canonical_guard_present "$target"; then
    canonical_present=true
  fi

  # Install hooks
  if [[ -d "$loom_root/defaults/hooks" ]]; then
    mkdir -p "$target/.loom/hooks"
    for hook_file in "$loom_root/defaults/hooks/"*.sh; do
      [[ -f "$hook_file" ]] || continue
      hook_name=$(basename "$hook_file")
      # The vendored generic guard is conditional on the canonical guard (#4041).
      if [[ "$hook_name" == "guard-destructive-generic.sh" ]] && [[ "$canonical_present" == "true" ]]; then
        if [[ -f "$target/.loom/hooks/$hook_name" ]]; then
          rm -f "$target/.loom/hooks/$hook_name"
          info "Canonical Repo Skills guard present — removed stale vendored $hook_name (dispatcher defers to canonical)"
        else
          info "Canonical Repo Skills guard present — skipping vendored $hook_name (dispatcher defers to canonical)"
        fi
        continue
      fi
      if [[ -f "$target/.loom/hooks/$hook_name" ]] && [[ "$force" != "true" ]]; then
        warning "Preserving existing hook: $hook_name (use --clean to overwrite)"
      else
        cp "$hook_file" "$target/.loom/hooks/$hook_name"
        chmod +x "$target/.loom/hooks/$hook_name"
        success "Installed hook: $hook_name"
      fi
    done
  fi
  # NOTE (#4401): the copies written above are ONLY reachable through a
  # project-level `.claude/settings.json` entry, and neither the 0.16.0 defaults
  # nor `loom-daemon init` writes one. `wire_quick_install_guard_hooks` (below)
  # MUST run after this function on every Quick Install path or the copies are
  # dead weight and the repo has zero guard coverage.

  # Install CLI wrapper
  if [[ -f "$loom_root/defaults/.loom/bin/loom" ]]; then
    mkdir -p "$target/.loom/bin"
    cp "$loom_root/defaults/.loom/bin/loom" "$target/.loom/bin/loom"
    chmod +x "$target/.loom/bin/loom"
    success "Installed .loom/bin/loom CLI"
  fi

  # Install loom.sh convenience wrapper at repo root
  if [[ -f "$loom_root/defaults/loom.sh" ]]; then
    cp "$loom_root/defaults/loom.sh" "$target/loom.sh"
    chmod +x "$target/loom.sh"
    success "Installed loom.sh"
  fi
}

# Guarantee that a Quick Install leaves the target with at least one WORKING
# guard-hook execution path (issue #4401).
#
# Background — why this exists at all. `provision_loom_hooks` had exactly one
# caller (scripts/install-loom.sh:1128), on the Full Install path. The Quick
# Install path (`--quick`, fresh AND `--confirm-reinstall`) ran `loom-daemon init`
# directly and never provisioned any hook wiring, so a repo whose only Loom
# install/update ever went through `--quick` had:
#   - no user-scope ~/.claude/settings.json entries (never provisioned), and
#   - no project-level .claude/settings.json entries either — the 0.16.0
#     defaults/.claude/settings.json deliberately carries no `hooks` block
#     (Phase 5 / #4262), and a --confirm-reinstall's chained uninstall
#     jq-strips every pre-existing `.loom/hooks/`-prefixed command out of the
#     project file before init runs (scripts/uninstall-loom.sh).
# Net: ZERO guards fired after a supported update. That is the #4401 report.
#
# CHOSEN RESOLUTION: Option A + Option B(b2) from the issue.
#   A — call `provision_loom_hooks` here, so the machine-level (user-scope)
#       wiring exists on the quick path too. This is the forward-looking layer:
#       it becomes the live path once a machine checkout exists (Full Install /
#       `loom update` establish ~/.local/share/loom) AND the repo has been
#       migrated past Phase 6 (`loom migrate` untracks the per-repo copies).
#       Until then it self-gates to a silent no-op — it can never double-fire.
#   b2 — call `ensure_project_hook_wiring`, which (re)asserts the project-level
#       `${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>` entries whenever the target
#       still carries per-repo `.loom/hooks/` copies. Since `install_hooks_and_cli`
#       writes exactly those copies on every quick install, this is the layer
#       that is GUARANTEED live on the quick path — it is what turns the reported
#       zero-coverage state into working coverage.
#
# Why not Option B(b1) ("stop writing .loom/hooks/ copies on the quick path")?
# Because the quick path never establishes the machine checkout that the
# user-scope wrapper resolves into, dropping the copies would leave a quick-only
# install with *no* hook scripts to run at all — trading a silent-zero-coverage
# bug for a louder one. Retiring the per-repo copies is `loom migrate`'s job
# (Phase 6 / #4254), which also removes them from the index.
#
# Exactly one path fires in either configuration: with copies present the
# user-scope wrapper defers to the project entry (transition dedup); with copies
# absent the project entries are not written and the wrapper execs the machine
# hook. Never both, never neither. Both calls are best-effort (a soft failure
# warns, never aborts the install), matching install-loom.sh's contract.
wire_quick_install_guard_hooks() {
  local target="$1"

  info "Provisioning user-scope loom guard hooks..."
  provision_loom_hooks || \
    warning "User-scope loom guard hooks not fully provisioned; guards still fire from the per-repo .loom/hooks/ copies + project-level settings entries asserted below."

  info "Asserting project-level guard-hook entries for per-repo .loom/hooks/ copies..."
  ensure_project_hook_wiring "$target" || \
    warning "Project-level guard-hook entries not fully asserted in $target/.claude/settings.json; run scripts/install-loom.sh (Full Install) or re-add them manually — see defaults/docs/guard-hooks.md."
}

# Regenerate .loom/manifest.json's checksums after this function's caller has
# finished ALL post-`loom-daemon init` mutations of Loom-tracked files
# (issue #5279).
#
# `loom-daemon init` generates the manifest as its own last internal step
# (loom-daemon/src/init/post_init.rs::generate_manifest, invoked from
# loom-daemon/src/init/mod.rs) -- which runs BEFORE `wire_quick_install_guard_hooks`
# (above) asserts project-level guard-hook entries into `.claude/settings.json`
# via `ensure_project_hook_wiring`. That means the manifest's stored hash for
# `.claude/settings.json` reflects its PRE-wiring content, not the genuinely
# final installed state.
#
# Left unfixed, `verify-install.sh verify` reports spurious `DRIFT DETECTED` on
# `.claude/settings.json` immediately after EVERY quick install -- even a
# completely vanilla install with zero customization and zero foreign
# content -- because the on-disk file legitimately changes after the manifest
# snapshot was taken. This is the concrete, reproducible mechanism behind the
# "verify-install.sh reports [settings.json] as drift by design" symptom
# described in #5279 (a stricter, always-reproducible version of it: no
# foreign/sibling-tool content is even required to trigger it).
#
# Call this ONLY after every mutator of Loom-tracked files in the current
# install path has run (currently: right after `wire_quick_install_guard_hooks`,
# its last one on the quick-install path). Best-effort: a missing or failing
# verify-install.sh never aborts the install.
regenerate_manifest_after_hook_wiring() {
  local target="$1"
  local script="$target/.loom/scripts/verify-install.sh"

  if [[ -f "$script" ]]; then
    ( cd "$target" && bash "$script" generate --quiet ) || \
      warning "Could not regenerate .loom/manifest.json after guard-hook wiring; 'verify-install.sh verify' may report spurious drift on .claude/settings.json."
  fi
}

# Export LOOM_VERSION and LOOM_COMMIT so `loom-daemon init`'s template
# substitution can fill {{LOOM_VERSION}} / {{LOOM_COMMIT}} placeholders in
# the CLAUDE.md templates instead of falling back to the literal string
# "unknown" (see loom-daemon/src/init/templates.rs:49-50). Issue #3502.
#
# Mirrors the env-export pattern from scripts/install-loom.sh:710,723.
prepare_loom_metadata_env() {
  local loom_root="$1"

  if [[ ! -f "$loom_root/package.json" ]]; then
    warning "Cannot find package.json in $loom_root — LOOM_VERSION will be 'unknown'"
    return 0
  fi

  LOOM_VERSION=$(node -pe "require('$loom_root/package.json').version" 2>/dev/null) || {
    warning "Failed to extract version from package.json — LOOM_VERSION will be 'unknown'"
    return 0
  }

  LOOM_COMMIT=$(git -C "$loom_root" rev-parse --short HEAD 2>/dev/null) || {
    warning "Failed to get git commit hash — LOOM_COMMIT will be 'unknown'"
    LOOM_COMMIT=""
  }

  export LOOM_VERSION
  export LOOM_COMMIT
}

# Post-`loom-daemon init` artifacts that loom-daemon does not write itself.
# Invoked by both the `--quick` reinstall branch and the fresh `--quick`
# install case so neither path drops:
#   - .loom/config/skill-routes.json (port of scripts/install-loom.sh:1032-1048)
#   - .loom/loom-source-path        (port of scripts/install-loom.sh:1067-1074)
#   - .loom/install-metadata.json   (port of scripts/install-loom.sh:1261-1270)
#
# See issue #3502. (This used to note that `--quick` deliberately skipped
# `setup-python-tools.sh`; there is no Python setup step on ANY install path as
# of epic #4081 Phase 4, #4557 — that script and the package it installed are
# both deleted, so `--quick` and the full installer are now identical in this
# respect.)
finalize_quick_install() {
  local loom_root="$1"
  local target="$2"

  # 1. Copy default config files (skill-routes.json template, etc.).
  if [[ -d "$loom_root/defaults/config" ]]; then
    mkdir -p "$target/.loom/config"
    for config_file in "$loom_root/defaults/config/"*.json; do
      [[ -f "$config_file" ]] || continue
      local config_name
      config_name=$(basename "$config_file")
      if [[ -f "$target/.loom/config/$config_name" ]]; then
        info "Skipping existing config: $config_name"
      else
        cp "$config_file" "$target/.loom/config/$config_name"
        success "Installed config: $config_name"
      fi
    done
  fi

  # 2. Record Loom source path (consumed by agent-metrics.sh and other
  # wrapper scripts to locate the Loom source checkout).
  echo "$loom_root" > "$target/.loom/loom-source-path"
  success "Recorded Loom source path"

  # 3. Write install-metadata.json with the same schema as the legacy
  # installer so uninstall-loom.sh and install-loom.sh's upgrade detector
  # can both consume it.
  local installed_files_json="[]"
  if [[ -f "$loom_root/scripts/install/manifest.sh" ]]; then
    # shellcheck source=/dev/null
    LOOM_ROOT="$loom_root" TARGET_PATH="$target" \
      source "$loom_root/scripts/install/manifest.sh"
    installed_files_json="$(LOOM_ROOT="$loom_root" TARGET_PATH="$target" \
      _emit_installed_files_manifest)"
  else
    warning "manifest.sh not found — install-metadata.json will have empty installed_files"
  fi

  local install_date
  install_date="$(date +%Y-%m-%d)"

  cat > "$target/.loom/install-metadata.json" <<METADATA
{
  "loom_version": "${LOOM_VERSION:-unknown}",
  "loom_commit": "${LOOM_COMMIT:-unknown}",
  "install_date": "${install_date}",
  "loom_source": "${loom_root}",
  "installed_files": ${installed_files_json}
}
METADATA
  success "Recorded installation metadata"

  # 3b. Wire the install-metadata.json merge=ours driver (#4528) so this
  # host's resync commits stop conflicting with other hosts' on `git merge`.
  if [[ -f "$loom_root/scripts/install/gitattributes.sh" ]]; then
    # shellcheck source=/dev/null
    source "$loom_root/scripts/install/gitattributes.sh"
    ensure_install_metadata_merge_driver "$target"
    success "Configured install-metadata.json merge=ours driver (.gitattributes + local git config)"
  else
    warning "gitattributes.sh not found — install-metadata.json merge=ours driver not configured"
  fi

  # Quick Install ships .github/labels.yml but does NOT create the labels on
  # the forge (that is a Full Install step). Point the operator at the shipped
  # sync script so the label-based workflow doesn't break on first use (#3582).
  info "Labels not yet synced. Run '.loom/scripts/sync-labels.sh' from the"
  info "  repo root to create the Loom workflow labels on the forge (or use"
  info "  Full Install, which syncs them automatically)."
}

# Verify critical installation files exist
verify_install() {
  local target="$1"
  local critical_files=(
    ".loom/config.json"
    ".loom/scripts/worktree.sh"
    ".loom/scripts/lib/loom-tools.sh"
    ".loom/install-metadata.json"
    ".loom/config/skill-routes.json"
  )
  local missing=0
  for file in "${critical_files[@]}"; do
    if [[ ! -f "$target/$file" ]]; then
      warning "Missing critical file: $file"
      missing=$((missing + 1))
    fi
  done

  # Defense-in-depth: surface any unsubstituted {{LOOM_VERSION}} /
  # {{INSTALL_DATE}} survivors in .loom/CLAUDE.md (issue #3502). Also
  # surface the literal "unknown" version line, which means the daemon's
  # substituter ran but LOOM_VERSION was not exported before invocation.
  local claude_md="$target/.loom/CLAUDE.md"
  if [[ -f "$claude_md" ]]; then
    if grep -q '{{LOOM_VERSION}}\|{{LOOM_COMMIT}}\|{{INSTALL_DATE}}\|{{REPO_OWNER}}\|{{REPO_NAME}}' "$claude_md"; then
      warning "Unsubstituted template placeholder(s) found in .loom/CLAUDE.md"
      missing=$((missing + 1))
    fi
    if grep -Eq '^\*\*Loom Version\*\*:[[:space:]]+unknown' "$claude_md"; then
      warning ".loom/CLAUDE.md has 'Loom Version: unknown' — LOOM_VERSION was not exported before loom-daemon init"
      missing=$((missing + 1))
    fi
  fi

  if [[ $missing -gt 0 ]]; then
    warning "$missing critical file(s) missing or corrupted after installation"
  fi
}

# Returns 0 (true / stale) if the loom-daemon release binary is missing, OR
# if the source tree it would be built from is newer than the binary on
# disk (e.g. `git pull` landed a newer commit since the binary was last
# built). A bare "does it exist" check lets `install.sh --quick` silently
# reuse a stale binary built from an older commit after a source update --
# issue #4188. `find -newer` catches both "never built" (the -f check below)
# and "built from a prior commit" in one pass.
loom_daemon_binary_stale() {
  local loom_root="$1"
  local binary="$loom_root/target/release/loom-daemon"

  [[ -f "$binary" ]] || return 0

  local newer_file
  newer_file="$(find "$loom_root/loom-daemon" "$loom_root/loom-api" \
      "$loom_root/Cargo.toml" "$loom_root/Cargo.lock" \
      -type f -newer "$binary" 2>/dev/null | head -n1)"
  [[ -n "$newer_file" ]]
}

# Issue #4897: `loom_daemon_binary_stale` above only ever compares the
# *source tree's own build artifact* (`$loom_root/target/release/loom-daemon`)
# against source mtimes -- it has no idea a machine-level binary already
# installed at the destination (`provision_machine_daemon`'s target,
# scripts/install/provision-daemon.sh) was already rebuilt from the exact
# commit we're about to build, e.g. by a fleet-mate's install/sweep that ran
# moments ago. Both call sites unconditionally ran `pnpm daemon:build` --
# i.e. `cargo build --package loom-daemon --release` -- whenever the source
# artifact looked stale, even when that build would just block for minutes on
# `target/.cargo-lock` (held by an unrelated `cargo test`/`clippy` in the same
# tree) and then produce a binary identical to one already on disk.
#
# Returns 0 ("current -- skip the build") when a machine-level installed
# binary exists AND its embedded build commit (`--version`, see
# loom-daemon/build.rs / loom_daemon::self_update::BUILD_IDENTITY) matches
# source HEAD. This is the SAME comparison `provision_machine_daemon()`
# already performs (scripts/install/provision-daemon.sh ~line 268, the
# "already current at $dest_bin" short-circuit) -- reused here so install.sh
# can skip the build BEFORE paying for the lock wait, not just discover after
# the fact that the rebuild wasn't needed. On a match it also copies the
# installed binary into `$loom_root/target/release/loom-daemon` (creating
# target/release/ if missing) so the unmodified downstream call sites that
# reference that in-repo path directly (`loom-daemon init`,
# `provision_machine_daemon` itself) keep working without further changes.
#
# Returns 1 (build still needed) when there is no installed binary, its
# `--version` can't be read, its embedded commit is "unknown"/unparsable, or
# it does not match source HEAD.
loom_daemon_dest_binary_current() {
  local loom_root="$1"
  local dest_dir="${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}"
  local dest_bin="$dest_dir/loom-daemon"

  [[ -x "$dest_bin" ]] || return 1

  local head_commit
  head_commit="$(git -C "$loom_root" rev-parse --short HEAD 2>/dev/null)" || return 1
  [[ -n "$head_commit" ]] || return 1

  local dest_version dest_commit
  dest_version="$("$dest_bin" --version 2>/dev/null)" || return 1
  # `--version` embeds "... (commit <sha>, built <ts>)" -- see
  # loom-daemon/build.rs and loom_daemon::self_update::BUILD_IDENTITY.
  case "$dest_version" in
    *"(commit "*)
      dest_commit="${dest_version#*"(commit "}"
      dest_commit="${dest_commit%%,*}"
      ;;
    *) return 1 ;;
  esac
  [[ -n "$dest_commit" && "$dest_commit" != "unknown" ]] || return 1
  [[ "$dest_commit" == "$head_commit" ]] || return 1

  mkdir -p "$loom_root/target/release" 2>/dev/null || return 1
  cp -f "$dest_bin" "$loom_root/target/release/loom-daemon" 2>/dev/null || return 1
  chmod 755 "$loom_root/target/release/loom-daemon" 2>/dev/null || true
  return 0
}

# Issue #4897: runs `pnpm daemon:build` (a `cargo build --package loom-daemon
# --release` under the hood) in the background and emits a periodic progress
# line while it is running, so a wait contended on `target/.cargo-lock` (held
# by an unrelated `cargo test`/`clippy` elsewhere in the same source tree) is
# visibly distinguishable from a hung installer instead of producing zero
# output for 10+ minutes. Best-effort: if `lsof` is unavailable or the lock
# file can't be inspected, the progress line still fires, just without naming
# the competing PID.
run_daemon_build_with_progress() {
  local loom_root="$1"
  local lockfile="$loom_root/target/.cargo-lock"
  local log
  log="$(mktemp 2>/dev/null || echo "/tmp/loom-daemon-build.$$.log")"

  ( cd "$loom_root" && pnpm daemon:build >"$log" 2>&1 ) &
  local build_pid=$!

  local elapsed=0
  local interval=15
  while kill -0 "$build_pid" 2>/dev/null; do
    sleep "$interval"
    elapsed=$((elapsed + interval))
    kill -0 "$build_pid" 2>/dev/null || break

    local holder=""
    if [[ -f "$lockfile" ]] && command -v lsof >/dev/null 2>&1; then
      holder="$(lsof -t "$lockfile" 2>/dev/null | head -n1 || true)"
    fi
    if [[ -n "$holder" ]]; then
      info "  ... still building loom-daemon (${elapsed}s elapsed; waiting on cargo build-dir lock, held by pid $holder)"
    else
      info "  ... still building loom-daemon (${elapsed}s elapsed)"
    fi
  done

  wait "$build_pid"
  local status=$?
  cat "$log" 2>/dev/null || true
  rm -f "$log" 2>/dev/null || true
  return $status
}

# Issue #3588: re-append the current Loom ephemeral .gitignore patterns after a
# --quick reinstall stash pop that was performed against a HEAD-reset .gitignore.
#
# The reinstall restores .gitignore to its committed HEAD state before popping so
# the user's stashed hunk applies cleanly (see the pop block below). That reset
# strips the Loom patterns the daemon's `init` had (re-)written, so we re-apply
# them here. The pattern list is derived from the post-init snapshot (lines that
# were present there but absent from the committed HEAD version) rather than
# hard-coded, so it never drifts from the daemon's authoritative list in
# loom-daemon/src/init/post_init.rs. Appending only missing lines keeps this
# idempotent (append-only), mirroring `update_gitignore`.
reapply_loom_gitignore_patterns() {
  local target_path="$1"
  local postinit_snapshot="$2"
  local gitignore="$target_path/.gitignore"

  [[ -f "$postinit_snapshot" && -f "$gitignore" ]] || return 0

  # The committed .gitignore (the stash base the user's hunk was recorded
  # against). Lines present in the post-init snapshot but not here are exactly
  # the Loom patterns `init` (re-)appended.
  local head_version loom_lines
  head_version="$(git -C "$target_path" show HEAD:.gitignore 2>/dev/null)"

  loom_lines="$(grep -vxF -f <(printf '%s\n' "$head_version") "$postinit_snapshot" 2>/dev/null || true)"
  [[ -z "$loom_lines" ]] && return 0

  local line
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    if ! grep -qxF -- "$line" "$gitignore" 2>/dev/null; then
      # Ensure a trailing newline before appending (command substitution strips
      # the newline, so a non-empty result means the file did NOT end in \n).
      if [[ -s "$gitignore" && -n "$(tail -c1 "$gitignore" 2>/dev/null)" ]]; then
        printf '\n' >>"$gitignore"
      fi
      printf '%s\n' "$line" >>"$gitignore"
    fi
  done <<<"$loom_lines"
}

# Issue #3663: re-apply the current Loom-owned CLAUDE.md marker block after a
# --quick reinstall stash pop that was performed against a HEAD-reset CLAUDE.md.
#
# This generalizes the #3588 .gitignore treatment (HEAD-reset-before-pop, then
# reapply) to CLAUDE.md, whose Loom content is a marker-delimited block
# (`<!-- BEGIN LOOM ORCHESTRATION -->` … `<!-- END LOOM ORCHESTRATION -->`)
# rather than a set of appended lines. The reinstall restores CLAUDE.md to its
# committed HEAD state before popping so the user's stashed hunk applies cleanly
# (see the pop block below). That reset reverts the Loom block to its old
# committed content, so we splice the freshly written block back in here.
#
# We rebuild the file as: everything BEFORE the begin marker (from the popped
# working copy — the user's restored content) + the block (begin…end inclusive)
# taken from the post-init snapshot (the daemon's authoritative fresh block) +
# everything AFTER the end marker (again from the popped working copy). Only the
# delimited Loom region is replaced; every line the user's pop restored outside
# the markers (e.g. their own `REPO-SKILLS` block) is left byte-for-byte intact.
# Derived from the snapshot, never hard-coded, mirroring
# reapply_loom_gitignore_patterns's "trust the daemon's output" property.
# Idempotent: splicing an already-current block is a no-op. If either file lacks
# both markers there is no delimited region to reconcile, so the popped file is
# left as-is.
reapply_loom_claude_md_block() {
  local target_path="$1"
  local postinit_snapshot="$2"
  local claude_md="$target_path/CLAUDE.md"
  local begin="<!-- BEGIN LOOM ORCHESTRATION -->"
  local end="<!-- END LOOM ORCHESTRATION -->"

  [[ -f "$postinit_snapshot" && -f "$claude_md" ]] || return 0

  # Both the snapshot and the popped file must carry the marker block, else
  # there is no delimited Loom region to splice — leave the popped file alone.
  grep -qF "$begin" "$postinit_snapshot" && grep -qF "$end" "$postinit_snapshot" || return 0
  grep -qF "$begin" "$claude_md" && grep -qF "$end" "$claude_md" || return 0

  local tmp
  tmp="$(mktemp 2>/dev/null || true)"
  [[ -n "$tmp" ]] || return 0

  # Pass 1 (snapshot): capture the begin…end block into `block`.
  # Pass 2 (popped file): print user content up to begin, emit the captured
  # block once, then resume printing user content after end.
  if awk -v b="$begin" -v e="$end" '
    FNR==NR {
      if (index($0, b)) grab=1
      if (grab) block = block $0 ORS
      if (index($0, e)) grab=0
      next
    }
    {
      if (index($0, b)) { printf "%s", block; skip=1 }
      if (!skip) print
      if (index($0, e)) skip=0
    }
  ' "$postinit_snapshot" "$claude_md" >"$tmp" 2>/dev/null && [[ -s "$tmp" ]]; then
    cat "$tmp" >"$claude_md" 2>/dev/null || true
  fi
  rm -f "$tmp" 2>/dev/null || true
  return 0
}

# Issue #3663: emit the Loom marker block (begin…end inclusive) from stdin.
# Used to decide whether a user's stashed CLAUDE.md edit lands INSIDE the Loom
# block (in which case a HEAD-reset+reapply would clobber it) or entirely
# outside it (safe to reset+reapply). Empty output means no block was found.
_emit_loom_claude_block() {
  awk '
    index($0, "<!-- BEGIN LOOM ORCHESTRATION -->") { inblk=1 }
    inblk { print }
    index($0, "<!-- END LOOM ORCHESTRATION -->") { inblk=0 }
  '
}

# Determine Loom repository root
LOOM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Machine-level loom-daemon provisioning helper (#3922). Sourced so the Quick
# Install path (which runs `loom-daemon init` directly, without delegating to
# scripts/install-loom.sh) can also drop the built binary on PATH. The Full
# Install path `exec`s scripts/install-loom.sh, which sources this helper itself.
# shellcheck source=scripts/install/provision-daemon.sh
source "$LOOM_ROOT/scripts/install/provision-daemon.sh"

# Guard-hook provisioning helpers (Epic #3835 Phase 5 / #4262, gap closed in
# #4401). Sourced for the SAME reason as provision-daemon.sh above: the Quick
# Install path runs `loom-daemon init` directly instead of delegating to
# scripts/install-loom.sh, so it must invoke these itself or the target ends up
# with no guard-hook execution path at all. Provides `provision_loom_hooks`
# (user-scope ~/.claude/settings.json wiring, mirroring
# scripts/install-loom.sh:1128) and `ensure_project_hook_wiring` (the
# project-level fallback for a repo that still carries `.loom/hooks/` copies).
# shellcheck source=scripts/install/provision-hooks.sh
source "$LOOM_ROOT/scripts/install/provision-hooks.sh"

# Show banner
echo ""
header "╔═══════════════════════════════════════════════════════════╗"
header "║                    Loom Setup v1.0                        ║"
header "║        AI-Powered Development Orchestration               ║"
header "╚═══════════════════════════════════════════════════════════╝"
echo ""

# Parse flags
NON_INTERACTIVE=false
INSTALL_TYPE=""
# Explicit acknowledgement that a reinstall over an existing (or legacy)
# .loom/ installation is destructive (it uninstalls before reinstalling).
# Required in addition to --quick/--yes/--full when an existing install is
# detected -- see the reinstall gate below and issue #4188.
CONFIRM_REINSTALL=false
# Source/target override flags accepted by scripts/install-loom.sh. The top-level
# wrapper does not act on them (its source guard runs only in the delegated
# installer), but it must accept and forward them so the flags it suggests
# actually work. See issue #3650.
SOURCE_OVERRIDE_FLAGS=()
while [[ "${1:-}" == -* ]]; do
  case "$1" in
    -y|--yes)
      NON_INTERACTIVE=true
      shift
      ;;
    --quick)
      # Check for conflicting --full flag
      if [[ "$INSTALL_TYPE" == "2" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="1"
      NON_INTERACTIVE=true  # --quick implies non-interactive
      shift
      ;;
    --full)
      # Check for conflicting --quick flag
      if [[ "$INSTALL_TYPE" == "1" ]]; then
        error "Cannot specify both --quick and --full"
      fi
      INSTALL_TYPE="2"
      NON_INTERACTIVE=true  # --full implies non-interactive
      shift
      ;;
    --confirm-reinstall)
      CONFIRM_REINSTALL=true
      shift
      ;;
    --allow-non-main-source|--allow-stale-target)
      # Pass-through: accepted here so the wrapper's own suggestion works, then
      # forwarded to scripts/install-loom.sh at the Full-Install delegation execs.
      SOURCE_OVERRIDE_FLAGS+=("$1")
      shift
      ;;
    -h|--help)
      echo "Usage: ./install.sh [OPTIONS] [TARGET_PATH]"
      echo ""
      echo "Options:"
      echo "  -y, --yes                  Non-interactive mode (skip confirmation prompts)"
      echo "  --quick                    Quick Install - direct install without GitHub workflow"
      echo "  --full                     Full Install - creates issue, worktree, and PR"
      echo "  --confirm-reinstall        Acknowledge a destructive reinstall over an existing"
      echo "                             .loom/ install. Required alongside --quick/--yes/--full"
      echo "                             when the target already has Loom installed -- without"
      echo "                             it, non-interactive runs stop and ask you to inventory"
      echo "                             customizations first (interactive runs still get a"
      echo "                             y/N prompt instead). If you only want to bring an"
      echo "                             existing install's surfaces up to date -- not replace"
      echo "                             the payload -- use the non-destructive"
      echo "                             .loom/scripts/resync-installed.sh in the target repo"
      echo "                             instead; --confirm-reinstall uninstalls before"
      echo "                             reinstalling."
      echo "  --allow-non-main-source    Permit installing from a non-main / detached-HEAD"
      echo "                             Loom source (forwarded to scripts/install-loom.sh)"
      echo "  --allow-stale-target       Permit installing over a newer/stale target"
      echo "                             (forwarded to scripts/install-loom.sh)"
      echo "  -h, --help                 Show this help message"
      echo ""
      echo "Examples:"
      echo "  ./install.sh --quick ~/projects/my-app"
      echo "  ./install.sh --full /path/to/team-project"
      echo "  ./install.sh -y ~/projects/my-app  # Non-interactive, defaults to quick install"
      echo "  ./install.sh --quick --confirm-reinstall ~/projects/my-app  # Reinstall over an existing install"
      echo "  ./install.sh --yes --allow-non-main-source /path/to/target  # Install from a non-main source"
      exit 0
      ;;
    *)
      error "Unknown flag: $1"
      ;;
  esac
done

# Early validation for --full: requires gh CLI (for GitHub repos)
# Gitea repos use the API directly and don't need gh CLI
if [[ "$INSTALL_TYPE" == "2" ]] && ! command -v gh &> /dev/null; then
  warning "GitHub CLI (gh) not found. Required for GitHub repos.\n       Install: brew install gh\n       For Gitea repos, set GITEA_TOKEN instead.\n       Or use --quick for installation without forge integration"
fi

# Get target path from argument or prompt
TARGET_PATH="${1:-}"

if [[ -z "$TARGET_PATH" ]]; then
  echo "Enter the path to the repository where you want to install Loom:"
  echo -e "${CYAN}Example: ~/GitHub/my-project or /Users/you/code/my-app${NC}"
  echo ""
  read -r -p "Repository path: " TARGET_PATH
  echo ""
fi

# Expand tilde if present
TARGET_PATH="${TARGET_PATH/#\~/$HOME}"

# Validate target path exists
if [[ ! -d "$TARGET_PATH" ]]; then
  error "Directory does not exist: $TARGET_PATH"
fi

# Resolve to absolute path
TARGET_PATH="$(cd "$TARGET_PATH" && pwd 2>/dev/null)" || \
  error "Cannot access directory: $TARGET_PATH"

info "Target repository: $TARGET_PATH"
echo ""

# Check if it's a git repository (worktree-safe: a linked worktree's .git is a file)
if ! git -C "$TARGET_PATH" rev-parse --git-dir >/dev/null 2>&1; then
  warning "$TARGET_PATH is not a git repository."
  echo ""
  echo "Would you like to initialize git and optionally set up GitHub?"
  echo ""
  echo "This will:"
  echo "  1. Run 'git init' in the directory"
  echo "  2. Create a sensible .gitignore file"
  echo "  3. Create an initial commit"
  echo "  4. Optionally create a GitHub repository and set up remote"
  echo ""
  read -r -p "Initialize git repository? [y/N] " -n 1 INIT_GIT
  echo ""

  if [[ ! $INIT_GIT =~ ^[Yy]$ ]]; then
    error "Cannot proceed without a git repository.\n       Run 'git init' manually or choose a different directory."
  fi

  # Initialize git
  info "Initializing git repository..."
  cd "$TARGET_PATH"
  git init --quiet || error "Failed to initialize git repository"
  success "Git repository initialized"

  # Create basic .gitignore if it doesn't exist
  if [[ ! -f "$TARGET_PATH/.gitignore" ]]; then
    info "Creating .gitignore..."
    cat > "$TARGET_PATH/.gitignore" << 'GITIGNORE'
# Dependencies
node_modules/
vendor/

# Build outputs
dist/
build/
target/
*.o
*.a
*.so
*.dylib

# IDE/Editor
.idea/
.vscode/
*.swp
*.swo
*~

# OS files
.DS_Store
Thumbs.db

# Environment files
.env
.env.local
.env.*.local

# Logs
*.log
logs/

# Loom (will be added by installation)
# .loom/state.json
# .loom/worktrees/
# .loom/*.log
GITIGNORE
    success "Created .gitignore"
  fi

  # Create initial commit
  info "Creating initial commit..."
  git add -A
  git commit -m "Initial commit" --quiet || error "Failed to create initial commit"
  success "Initial commit created"
  echo ""

  # Offer remote repository creation
  if command -v gh &> /dev/null; then
    echo "Would you like to create a GitHub repository for this project?"
    echo "(For Gitea, create the repository manually and add the remote)"
    echo ""
    read -r -p "Create GitHub repository? [y/N] " -n 1 CREATE_REPO
    echo ""

    if [[ $CREATE_REPO =~ ^[Yy]$ ]]; then
      # Check GitHub authentication
      if ! gh auth status &> /dev/null; then
        warning "GitHub CLI is not authenticated"
        info "Please authenticate with GitHub:"
        echo ""
        gh auth login || error "GitHub authentication failed"
        echo ""
      fi

      # Prompt for repository visibility
      echo "Repository visibility:"
      echo "  1. Private (default)"
      echo "  2. Public"
      read -r -p "Choose visibility [1/2]: " -n 1 VISIBILITY
      echo ""

      VISIBILITY_FLAG="--private"
      if [[ "$VISIBILITY" == "2" ]]; then
        VISIBILITY_FLAG="--public"
      fi

      # Get directory name for repo name suggestion
      DIR_NAME=$(basename "$TARGET_PATH")
      read -r -p "Repository name [$DIR_NAME]: " REPO_NAME
      REPO_NAME="${REPO_NAME:-$DIR_NAME}"

      info "Creating GitHub repository: $REPO_NAME..."
      if gh repo create "$REPO_NAME" $VISIBILITY_FLAG --source="$TARGET_PATH" --push; then
        success "Repository created and pushed"
      else
        warning "Failed to create repository. Continuing with local git only."
        info "You can create the repository later with: gh repo create"
      fi
      echo ""
    fi
  else
    info "GitHub CLI (gh) not found - skipping remote repository creation"
    info "For GitHub: install gh CLI (brew install gh)"
    info "For Gitea: create the repo manually and run: git remote add origin <url>"
    echo ""
  fi
fi

success "Valid git repository detected"
echo ""

# ============================================================================
# Check Required Dependencies
# ============================================================================
header "Checking System Dependencies"
echo ""

MISSING_DEPS=()
INSTALL_INSTRUCTIONS=""

# Check for Git (should always be present if we got this far, but verify)
if command -v git &> /dev/null; then
  success "git: $(git --version | head -1)"
else
  MISSING_DEPS+=("git")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • git: brew install git"
fi

# Check for Node.js
if command -v node &> /dev/null; then
  success "node: $(node --version)"
else
  MISSING_DEPS+=("node")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • Node.js: brew install node"
fi

# Check for pnpm
if command -v pnpm &> /dev/null; then
  success "pnpm: $(pnpm --version)"
else
  MISSING_DEPS+=("pnpm")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • pnpm: npm install -g pnpm"
fi

# Check for Cargo (Rust toolchain)
if command -v cargo &> /dev/null; then
  success "cargo: $(cargo --version | head -1)"
else
  MISSING_DEPS+=("cargo")
  INSTALL_INSTRUCTIONS="${INSTALL_INSTRUCTIONS}\n  • Rust/Cargo: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

# Check for GitHub CLI (optional, needed for Full Install with GitHub repos)
if command -v gh &> /dev/null; then
  success "gh: $(gh --version | head -1)"
else
  warning "gh (GitHub CLI) not found - needed for Full Install with GitHub repos"
  info "  Install with: brew install gh"
  info "  For Gitea repos, gh is not required (set GITEA_TOKEN instead)"
fi

echo ""

# If any required dependencies are missing, prompt the user
if [[ ${#MISSING_DEPS[@]} -gt 0 ]]; then
  echo ""
  error_no_exit() {
    echo -e "${RED}✗ Missing required dependencies: ${MISSING_DEPS[*]}${NC}"
  }
  error_no_exit

  echo ""
  info "Please install the missing dependencies:"
  echo -e "$INSTALL_INSTRUCTIONS"
  echo ""

  # Issue #4888: this gate used to `read` unconditionally, even under
  # --yes/--quick/--full. On a non-interactive run stdin is not a terminal, so
  # the read returns immediately at EOF with an empty reply, which then fell
  # into the "exit to install" default below and aborted -- after printing a
  # prompt nobody was there to answer, and (when stdin is a pipe rather than
  # /dev/null) after silently eating a byte of whatever else was on it. Honor
  # NON_INTERACTIVE explicitly instead: same outcome (a missing required
  # dependency is still fatal), but as a direct, greppable error rather than a
  # phantom prompt.
  if [[ "$NON_INTERACTIVE" == true ]]; then
    error "Missing required dependencies: ${MISSING_DEPS[*]} -- cannot continue a non-interactive install.\n       Install them (see above) and re-run, or drop --yes/--quick/--full to get an interactive prompt that lets you continue anyway."
  fi

  read -r -p "Exit to install dependencies? [Y/n] " -n 1 INSTALL_DEPS
  echo ""
  if [[ ! $INSTALL_DEPS =~ ^[Nn]$ ]]; then
    info "Please install the missing dependencies and run this script again."
    exit 1
  fi

  warning "Continuing without all dependencies may cause build failures"
  echo ""
fi

# Check if this is the Loom source repository (self-installation)
is_loom_source_repo() {
  local path="$1"
  # Check for marker file
  [[ -f "$path/.loom-source" ]] && return 0
  # Check for Loom-specific directory structure
  [[ -d "$path/loom-daemon" && -d "$path/loom-api" && -d "$path/defaults" ]] && return 0
  return 1
}

if is_loom_source_repo "$TARGET_PATH"; then
  echo ""
  header "╔═══════════════════════════════════════════════════════════╗"
  header "║              Loom Source Repository Detected              ║"
  header "╚═══════════════════════════════════════════════════════════╝"
  echo ""
  info "This appears to be the Loom source repository itself."
  info "Self-installation runs in validation-only mode to prevent data loss."
  echo ""
  info "The Loom repo's .loom/ directory IS the source of truth for defaults."
  info "Installing would overwrite rich content with minimal templates."
  echo ""
  read -r -p "Run validation to check configuration? [Y/n] " -n 1 VALIDATE_REPLY
  echo ""
  if [[ $VALIDATE_REPLY =~ ^[Nn]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
  FORCE_FLAG=""
  SELF_INSTALL=true
elif [[ -d "$TARGET_PATH/.loom" ]]; then
  warning "Loom appears to be already installed in this repository"
  echo ""
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    info "Reinstall will uninstall the existing installation first, then perform"
    info "a fresh Quick Install."
  else
    info "Reinstall will upgrade the existing installation in an isolated worktree"
    info "and open a PR with the changes -- your working directory is not touched"
    info "until you merge it."
  fi
  echo ""

  if [[ "$NON_INTERACTIVE" != true ]]; then
    read -r -p "Proceed with reinstall? [y/N] " -n 1 REINSTALL_CONFIRM
    echo ""
    if [[ ! $REINSTALL_CONFIRM =~ ^[Yy]$ ]]; then
      info "Installation cancelled"
      exit 0
    fi
  elif [[ "$CONFIRM_REINSTALL" == true ]]; then
    info "Non-interactive mode: proceeding with reinstall (--confirm-reinstall acknowledged)"
  else
    # Issue #4188: --quick / --yes / --full previously set NON_INTERACTIVE=true
    # and fell straight through the reinstall warning above into a destructive
    # uninstall-then-reinstall. A non-interactive run over an existing (or
    # legacy) install now MUST pass --confirm-reinstall explicitly -- it
    # cannot silently cross this boundary just because it also passed
    # --quick/--yes/--full.
    error "Existing Loom installation detected at $TARGET_PATH/.loom -- refusing to run a non-interactive reinstall without explicit acknowledgement.\n       Reinstalling uninstalls the existing Loom payload before writing the new version; inventory and back up any project-owned Loom hooks, scripts, and agent configuration first.\n       If you only want to bring the existing install up to date -- not replace it -- run the non-destructive '$TARGET_PATH/.loom/scripts/resync-installed.sh' instead; it copies forward the latest hooks/scripts/roles/docs without uninstalling anything.\n       Re-run with --confirm-reinstall once you have done so, or omit --quick/--yes/--full to get an interactive y/N prompt instead."
  fi

  # Issue #4888: the chained uninstall below (`uninstall-loom.sh --yes --local`)
  # mutates $TARGET_PATH's MAIN checkout directly -- it stages deletions and
  # strips the Loom sections out of CLAUDE.md / .gitignore in place. That is
  # necessary and safe for a --quick reinstall (the fresh install runs
  # synchronously in that same working tree right after -- see the
  # INSTALL_TYPE=="1" branch below, which unstages/restores as part of its own
  # completion). It is NOT safe for the non-quick (Full Install) path: that
  # path `exec`s into scripts/install-loom.sh, which does all of its real work
  # in an isolated worktree branched from `origin/main` (see
  # scripts/install/create-worktree.sh) and opens a PR -- it never reads or
  # needs a pre-uninstalled main checkout. Once `exec` replaces this process,
  # nothing here can roll back a downstream failure (a trap in *this* script
  # will never fire), so any failure inside the delegated install-loom.sh run
  # left the chained uninstall's staged deletions stranded on $TARGET_PATH's
  # main checkout with no automatic recovery (the reported half-uninstalled
  # target). `loom-daemon init` already performs a careful merge/upgrade of an
  # existing install (including legacy-content migration) from inside that
  # worktree, so the Full Install path needs no pre-mutation at all --
  # install-loom.sh's own idempotency check detects the older installed
  # version and proceeds with a normal upgrade (see the delegation site below).
  if [[ "$INSTALL_TYPE" == "1" ]]; then
    # Issue #3545: for a --quick reinstall, guard uncommitted user changes across
    # the uninstall→reinstall cycle (mirrors the stash guard in the sibling
    # scripts/install-loom.sh --clean path). The uninstall runs `git add` in the
    # target tree and the reinstall reconciles the index afterwards; stashing
    # first keeps a user's pre-existing staged/working changes from being caught
    # up in either step.
    #
    # Issue #3597: scope the stash to Loom-owned paths. The original unscoped
    # `git stash push` swept sibling installers' uncommitted tracked changes
    # (.anvil/*, .claude/skills/repo/*, non-Loom CLAUDE.md sections, …) into the
    # stash and left a half-old/half-new hybrid tree. Restrict the stash to the
    # dirty ∩ (Loom ownership set + .gitignore) intersection so sibling changes
    # are never touched. Empty intersection → no stash at all.
    REINSTALL_STASHED_USER_CHANGES=false
    # shellcheck source=scripts/install/stash-scope.sh
    source "$LOOM_ROOT/scripts/install/stash-scope.sh"
    REINSTALL_OWNED_DIRTY=()
    while IFS= read -r _owned_path; do
      [[ -n "$_owned_path" ]] && REINSTALL_OWNED_DIRTY+=("$_owned_path")
    done < <(_emit_loom_owned_dirty_paths "$LOOM_ROOT" "$TARGET_PATH")

    if [[ ${#REINSTALL_OWNED_DIRTY[@]} -gt 0 ]]; then
      info "Stashing uncommitted Loom-owned changes before reinstall..."
      if git -C "$TARGET_PATH" stash push \
           -m "loom-install: preserving user changes before --quick reinstall" \
           -- "${REINSTALL_OWNED_DIRTY[@]}" 2>/dev/null; then
        REINSTALL_STASHED_USER_CHANGES=true
        REINSTALL_STASH_REF="$(git -C "$TARGET_PATH" stash list 2>/dev/null | head -1)"
        success "Loom-owned changes stashed → ${REINSTALL_STASH_REF:-stash@{0}}"
        info "  Stashed ${#REINSTALL_OWNED_DIRTY[@]} Loom-owned path(s): ${REINSTALL_OWNED_DIRTY[*]}"
        info "  Recover manually with: git -C \"$TARGET_PATH\" stash pop"
      else
        warning "Failed to stash user changes - continuing without stash"
        warning "Uncommitted changes may appear alongside the reinstall diff"
      fi
    fi

    # Issue #3598: snapshot the committed .loom/config.json before the chained
    # uninstall deletes it. `config.json` is listed in uninstall-loom.sh's
    # RUNTIME_ARTIFACTS and is removed from disk, but it is consumer configuration
    # (e.g. a load-bearing `worktree.root` override), not a runtime artifact.
    # Restoring the snapshot before `loom-daemon init` (below) lets the daemon's
    # merge-aware config copy preserve consumer keys instead of regenerating the
    # file from the template. Mirrors the #3588 .gitignore snapshot pattern.
    # Standalone uninstall behavior is intentionally unchanged.
    REINSTALL_CONFIG_SNAPSHOT=""
    if [[ -f "$TARGET_PATH/.loom/config.json" ]]; then
      REINSTALL_CONFIG_SNAPSHOT="$(mktemp 2>/dev/null || true)"
      if [[ -n "$REINSTALL_CONFIG_SNAPSHOT" ]]; then
        cp "$TARGET_PATH/.loom/config.json" "$REINSTALL_CONFIG_SNAPSHOT" 2>/dev/null || \
          REINSTALL_CONFIG_SNAPSHOT=""
      fi
    fi

    # Uninstall existing installation (local mode, no separate PR)
    info "Uninstalling existing Loom installation..."
    "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
      error "Uninstall failed - aborting reinstall"
    echo ""
    success "Existing installation removed"
    echo ""

    info "Running fresh Quick Install..."
    echo ""

    # Check if loom-daemon is built AND up to date with the current source
    # tree (issue #4188 -- a bare existence check reuses a binary built from
    # an older commit after `git pull` landed a newer one).
    if loom_daemon_binary_stale "$LOOM_ROOT"; then
      # Issue #4897: before paying for a `cargo build` (and its
      # `target/.cargo-lock` wait), check whether the ALREADY-INSTALLED
      # machine-level binary was already rebuilt from this exact commit --
      # e.g. by a fleet-mate's install/sweep moments ago -- and reuse it
      # instead of rebuilding.
      if loom_daemon_dest_binary_current "$LOOM_ROOT"; then
        info "loom-daemon already current at ${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}/loom-daemon (matches source HEAD) -- skipping rebuild"
      else
        if [[ -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
          warning "loom-daemon binary is stale (source tree updated since last build)"
        else
          warning "loom-daemon binary not found"
        fi
        info "Building loom-daemon (this may take a minute)..."
        run_daemon_build_with_progress "$LOOM_ROOT" || error "Failed to build loom-daemon"
        echo ""
      fi
    fi

    # Export LOOM_VERSION / LOOM_COMMIT so the daemon's template substituter
    # fills CLAUDE.md correctly (issue #3502).
    prepare_loom_metadata_env "$LOOM_ROOT"

    # Issue #3598: restore the snapshotted config.json before init so the
    # daemon's merge-aware config copy sees the consumer's committed values
    # (e.g. worktree.root) and preserves them in the merged result.
    if [[ -n "$REINSTALL_CONFIG_SNAPSHOT" && -f "$REINSTALL_CONFIG_SNAPSHOT" ]]; then
      mkdir -p "$TARGET_PATH/.loom"
      cp "$REINSTALL_CONFIG_SNAPSHOT" "$TARGET_PATH/.loom/config.json" 2>/dev/null || true
    fi

    # Run loom-daemon init
    "$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
      error "Installation failed"

    # Clean up the config snapshot now that init has merged it into place.
    [[ -n "$REINSTALL_CONFIG_SNAPSHOT" ]] && rm -f "$REINSTALL_CONFIG_SNAPSHOT" 2>/dev/null || true

    # Install hooks and CLI wrapper (not handled by loom-daemon init)
    install_hooks_and_cli "$LOOM_ROOT" "$TARGET_PATH"
    # Emit skill-routes.json, install-metadata.json, loom-source-path (#3502).
    finalize_quick_install "$LOOM_ROOT" "$TARGET_PATH"
    verify_install "$TARGET_PATH"

    # Provision a machine-level loom-daemon binary (#3922) so the consumer's
    # loom-daemon-start.sh resolves it via `command -v loom-daemon` post-install.
    provision_machine_daemon "$LOOM_ROOT/target/release/loom-daemon" || true

    # Guard-hook wiring (#4401). MUST run after install_hooks_and_cli (which
    # writes the .loom/hooks/ copies the project-level entries point at) and
    # BEFORE the git-index reconcile below, so the reconcile sees the final
    # .claude/settings.json content. On this reinstall path the chained uninstall
    # above stripped every project-level `.loom/hooks/` entry and `init` re-added
    # none (the 0.16.0 defaults carry no `hooks` block) — this call is what
    # restores a working guard-hook execution path instead of leaving zero.
    wire_quick_install_guard_hooks "$TARGET_PATH"

    # Regenerate the checksum manifest now that ALL settings.json mutations for
    # this install path are complete (issue #5279 — see the function's own
    # comment for why this must run after wire_quick_install_guard_hooks).
    regenerate_manifest_after_hook_wiring "$TARGET_PATH"

    # Issue #3545: reconcile the git index after the uninstall→reinstall cycle.
    # The chained uninstall staged the deletion of every prior Loom file (now
    # scoped to Loom-managed paths — see scripts/uninstall-loom.sh), then
    # `loom-daemon init --force` rewrote those files to disk WITHOUT touching
    # the index. Left as-is, `git status` shows ~150 paired staged-`D` /
    # untracked-`??` entries instead of the real version-upgrade diff. Unstage
    # the uninstall's staged deletions so the working tree reflects only the
    # actual old→new file changes.
    #
    # Issue #3597: scope the unstage to Loom-owned paths so user-staged
    # non-Loom changes (sibling installers, unrelated work) stay staged. The
    # uninstall only stages Loom-managed paths (#3450), so the dirty ∩
    # ownership intersection is exactly the set of staged deletions to undo.
    info "Reconciling git index after reinstall..."
    RECONCILE_PATHS=()
    while IFS= read -r _owned_path; do
      [[ -n "$_owned_path" ]] && RECONCILE_PATHS+=("$_owned_path")
    done < <(_emit_loom_owned_dirty_paths "$LOOM_ROOT" "$TARGET_PATH")
    if [[ ${#RECONCILE_PATHS[@]} -gt 0 ]]; then
      git -C "$TARGET_PATH" restore --staged -- "${RECONCILE_PATHS[@]}" 2>/dev/null || \
        git -C "$TARGET_PATH" reset -q HEAD -- "${RECONCILE_PATHS[@]}" 2>/dev/null || true
    fi

    # Issue #3611: reconcile GENERATED install-time artifacts that the ownership-
    # scoped pass above misses. `.loom/install-metadata.json` is written by
    # finalize_quick_install, NOT shipped in defaults/, so it is absent from the
    # manifest-derived ownership set that scopes RECONCILE_PATHS. The chained
    # uninstall staged its deletion (uninstall-loom.sh REMOVE_FILES → git add -A),
    # and finalize then rewrote it on disk as an UNTRACKED file — leaving a
    # `D` staged-deletion + `??` untracked pair. Committed as-is, that untracks
    # the very file verify_install and the upgrade detector depend on. Explicitly
    # unstage the staged deletion so the rewritten file reappears as a tracked
    # modification (` M`), never `D`+`??`. Guarded by a staged-diff check so it is
    # a no-op when the file was never staged for deletion. (`.loom/loom-source-path`
    # has the same generated-at-install shape but is gitignored → untracked → no
    # staged deletion, so it needs no reconcile; `.loom/config/skill-routes.json`
    # ships in defaults/config and is already covered by RECONCILE_PATHS.)
    for _generated_tracked in ".loom/install-metadata.json"; do
      if git -C "$TARGET_PATH" diff --staged --name-only -- "$_generated_tracked" 2>/dev/null \
           | grep -qxF "$_generated_tracked"; then
        git -C "$TARGET_PATH" restore --staged -- "$_generated_tracked" 2>/dev/null || \
          git -C "$TARGET_PATH" reset -q HEAD -- "$_generated_tracked" 2>/dev/null || true
      fi
    done

    # Restore any user changes stashed before the uninstall (see above).
    #
    # Issue #3588: the uninstall→init round-trip rewrites .gitignore
    # non-reversibly — the uninstall strips Loom patterns from mid-block and
    # collapses blank lines (scripts/uninstall-loom.sh), then `init` re-appends
    # the patterns at end-of-file (loom-daemon update_gitignore). That moves
    # lines relative to HEAD, so a stashed .gitignore hunk — recorded against
    # the committed context — no longer has a matching 3-way base on disk and
    # `git stash pop` conflicts. Previously the pop was silenced with
    # `2>/dev/null`: the conflict was hidden, the stash silently kept, and the
    # user's uncommitted .gitignore edit stranded (data-loss risk).
    #
    # Fix: before popping, restore .gitignore to its committed HEAD state so the
    # pop's 3-way base matches the stash base and the user's hunk applies
    # cleanly; then re-append the current Loom ephemeral patterns (append-only,
    # idempotent). If the pop still fails for any reason, surface the real
    # conflict output and a working recovery path instead of hiding it.
    if [[ "$REINSTALL_STASHED_USER_CHANGES" == "true" ]]; then
      info "Restoring stashed user changes..."

      # Issue #3663: generalize the #3588 .gitignore HEAD-reset-then-reapply to
      # every Loom-owned dirty file that carries a well-defined Loom-vs-user
      # split — today `.gitignore` (Loom patterns appended at EOF) and
      # `CLAUDE.md` (a marker-delimited Loom block with user content around it).
      # For each such path tracked at HEAD, snapshot the post-init on-disk
      # version (which carries the freshly written Loom content) and reset the
      # working copy to HEAD so the pop's 3-way base lines up with the committed
      # context and the user's stashed hunk applies cleanly. After a successful
      # pop we re-apply only the Loom portion from the snapshot (append for
      # `.gitignore`, marker-block splice for `CLAUDE.md`), leaving everything
      # the user's pop restored untouched.
      #
      # HEAD-reset is deliberately scoped to files with a reapply strategy. A
      # fully Loom-owned file (a role `.md`, `config.json`) has no partial
      # reapply, so resetting it would silently drop the reinstall's update —
      # those fall through to a plain pop, which surfaces a genuine conflict
      # (named below) instead of resetting-and-losing. Untracked/newly created
      # files have no HEAD base to restore to and the plain pop already applies
      # them cleanly, so they are skipped too.
      REINSTALL_RESET_PATHS=()
      REINSTALL_RESET_SNAPSHOTS=()
      REINSTALL_RESET_STRATEGIES=()
      for _owned_path in ${REINSTALL_OWNED_DIRTY[@]+"${REINSTALL_OWNED_DIRTY[@]}"}; do
        _reset_strategy=""
        case "$_owned_path" in
          .gitignore) _reset_strategy="gitignore" ;;
          CLAUDE.md)  _reset_strategy="claude_md" ;;
          *) continue ;;
        esac
        git -C "$TARGET_PATH" cat-file -e "HEAD:$_owned_path" 2>/dev/null || continue

        # Issue #3663: CLAUDE.md's reapply replaces the ENTIRE marker block, so
        # the reset+reapply path is only safe when (a) HEAD already carries a
        # Loom block to splice and (b) the user's stashed edits are OUTSIDE that
        # block. When the user edited INSIDE the block, reset+reapply would
        # silently clobber their in-block edit — so skip the reset and let the
        # plain pop's 3-way merge surface a genuine conflict (named below),
        # keeping the edit in the stash. When HEAD has no block at all, `init`'s
        # freshly appended block already survives an out-of-block pop unchanged,
        # so there is nothing to splice. Detect both by comparing the stashed
        # (user) block region against HEAD's block region.
        if [[ "$_reset_strategy" == "claude_md" ]]; then
          _head_block="$(git -C "$TARGET_PATH" show HEAD:CLAUDE.md 2>/dev/null | _emit_loom_claude_block)" || true
          [[ -n "$_head_block" ]] || continue
          _stashed_block="$(git -C "$TARGET_PATH" show 'stash@{0}:CLAUDE.md' 2>/dev/null | _emit_loom_claude_block)" || true
          [[ "$_stashed_block" == "$_head_block" ]] || continue
        fi

        _reset_snap="$(mktemp 2>/dev/null || true)"
        [[ -n "$_reset_snap" ]] || continue
        if cp "$TARGET_PATH/$_owned_path" "$_reset_snap" 2>/dev/null && \
           git -C "$TARGET_PATH" checkout HEAD -- "$_owned_path" 2>/dev/null; then
          REINSTALL_RESET_PATHS+=("$_owned_path")
          REINSTALL_RESET_SNAPSHOTS+=("$_reset_snap")
          REINSTALL_RESET_STRATEGIES+=("$_reset_strategy")
        else
          rm -f "$_reset_snap" 2>/dev/null || true
        fi
      done

      # Issue #3611: pop with `--index` so a caller's pre-existing staged/
      # unstaged split is reproduced. A plain `git stash pop` re-applies EVERY
      # stashed hunk to the working tree as *unstaged* — a caller who had a
      # `.gitignore` edit STAGED before the reinstall got it back unstaged, and
      # any careful partial staging in flight was silently flattened. `--index`
      # reinstates the index tree the stash recorded at push time, so staged
      # hunks come back staged and unstaged hunks stay unstaged. The `.gitignore`
      # HEAD-reset above provides a clean 3-way base so the index restore lines
      # up; `reapply_loom_gitignore_patterns` (below) then appends Loom ephemeral
      # patterns to the WORKING TREE ONLY (never the staged copy — they are not
      # the caller's change). `--index` is stricter than a plain pop: if it
      # cannot reinstate the index cleanly (a genuine conflict) it fails, and we
      # fall through to the conflict-surfacing branch below rather than silently
      # degrading to an unstaged pop that would drop the staged split.
      #
      # Capture the pop in an `if` condition so the assignment is exempt from
      # `set -e`. A plain top-level `VAR="$(cmd)"` assignment inherits the
      # command-substitution exit status, so a conflicting `git stash pop`
      # (non-zero) would trip `set -euo pipefail` on the assignment itself and
      # abort the installer before the conflict-surfacing branch below ever
      # runs (issue #3588 / PR review).
      if REINSTALL_POP_OUTPUT="$(git -C "$TARGET_PATH" stash pop --index 2>&1)"; then
        REINSTALL_POP_STATUS=0
      else
        REINSTALL_POP_STATUS=$?
      fi

      if [[ $REINSTALL_POP_STATUS -eq 0 ]]; then
        # Pop succeeded. Each file we reset to HEAD now carries the user's hunk
        # but its OLD committed Loom content — re-apply the fresh Loom portion
        # from that file's post-init snapshot (append for .gitignore, marker-
        # block splice for CLAUDE.md).
        _reset_i=0
        while [[ $_reset_i -lt ${#REINSTALL_RESET_PATHS[@]} ]]; do
          case "${REINSTALL_RESET_STRATEGIES[$_reset_i]}" in
            gitignore)
              reapply_loom_gitignore_patterns "$TARGET_PATH" "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}"
              ;;
            claude_md)
              reapply_loom_claude_md_block "$TARGET_PATH" "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}"
              ;;
          esac
          _reset_i=$((_reset_i + 1))
        done
        success "User changes restored"
      else
        # Genuine conflict (e.g. the user also edited the same lines a Loom-owned
        # file that `init` rewrote occupies). Roll every reset file back to its
        # post-init snapshot so the tree is not left half-reset, then surface the
        # real conflict — naming the specific file(s) that conflicted — and a
        # concrete recovery path. Do NOT abort — the reinstall itself succeeded;
        # only the user-change restore needs manual attention.
        _reset_i=0
        while [[ $_reset_i -lt ${#REINSTALL_RESET_PATHS[@]} ]]; do
          cp "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}" \
            "$TARGET_PATH/${REINSTALL_RESET_PATHS[$_reset_i]}" 2>/dev/null || true
          _reset_i=$((_reset_i + 1))
        done
        # Issue #3663: name the file(s) that actually conflicted rather than a
        # generic "recover by hand". Prefer git's own unmerged set; fall back to
        # the guarded Loom-owned dirty set when git reports none.
        REINSTALL_CONFLICT_FILES="$(git -C "$TARGET_PATH" diff --name-only --diff-filter=U 2>/dev/null | paste -sd' ' - 2>/dev/null || true)"
        if [[ -z "$REINSTALL_CONFLICT_FILES" ]]; then
          REINSTALL_CONFLICT_FILES="${REINSTALL_OWNED_DIRTY[*]-}"
        fi
        REINSTALL_STASH_REF="$(git -C "$TARGET_PATH" stash list 2>/dev/null | head -1 | cut -d: -f1)"
        [[ -z "$REINSTALL_STASH_REF" ]] && REINSTALL_STASH_REF="stash@{0}"
        warning "Failed to restore stashed user changes automatically"
        echo ""
        [[ -n "$REINSTALL_CONFLICT_FILES" ]] && \
          echo "  Conflicting file(s): $REINSTALL_CONFLICT_FILES" && echo ""
        echo "  git stash pop --index reported:"
        printf '%s\n' "$REINSTALL_POP_OUTPUT" | sed 's/^/    /'
        echo ""
        echo "  Note: the restore preserves your original staged/unstaged split"
        echo "  (git stash pop --index). That split could not be reproduced"
        echo "  automatically here, so recover by hand to keep it intact."
        echo "  Your changes are preserved in the stash ($REINSTALL_STASH_REF)."
        echo "  A plain 'git stash pop' will conflict the same way, so recover by hand:"
        echo "    cd $TARGET_PATH"
        echo "    git stash show -p $REINSTALL_STASH_REF              # inspect the stashed diff"
        echo "    git stash show -p $REINSTALL_STASH_REF | git apply --3way   # or reconcile by hand"
        echo "    git stash drop $REINSTALL_STASH_REF                 # once you've reconciled"
      fi

      _reset_i=0
      while [[ $_reset_i -lt ${#REINSTALL_RESET_SNAPSHOTS[@]} ]]; do
        rm -f "${REINSTALL_RESET_SNAPSHOTS[$_reset_i]}" 2>/dev/null || true
        _reset_i=$((_reset_i + 1))
      done
    fi

    echo ""
    success "Quick reinstallation complete!"
    exit 0
  fi

  # Default: delegate to Full Install (creates worktree + PR). Issue #4888: no
  # uninstall runs against $TARGET_PATH's main checkout above for this path --
  # the delegated install-loom.sh does all of its work in an isolated worktree
  # (scripts/install/create-worktree.sh branches from `origin/main`) and never
  # touches the live main checkout, so there is nothing to roll back on
  # failure. install-loom.sh's own idempotency check will detect the existing
  # (older) installed version from $TARGET_PATH/.loom/install-metadata.json
  # and proceed with a normal merge-mode upgrade inside its worktree -- the
  # same careful preserve/migrate logic (loom-daemon init) that a plain
  # `install-loom.sh` upgrade already relies on. Deliberately NOT passing
  # --force here: that flag also flips FORCE_AUTO_MERGE on for the resulting
  # PR (scripts/install-loom.sh's create-pr.sh call), which is a bigger
  # behavior change (skips human review) than this fix's scope.
  info "Running fresh install via Full Install workflow..."
  echo ""
  INSTALL_FLAGS=()
  if [[ "$NON_INTERACTIVE" == true ]]; then
    INSTALL_FLAGS+=(--yes)
  fi
  exec "$LOOM_ROOT/scripts/install-loom.sh" ${INSTALL_FLAGS[@]+"${INSTALL_FLAGS[@]}"} ${SOURCE_OVERRIDE_FLAGS[@]+"${SOURCE_OVERRIDE_FLAGS[@]}"} "$TARGET_PATH"
else
  FORCE_FLAG=""
  SELF_INSTALL=false
fi

echo ""
header "What Will Be Installed"
echo ""
info "Configuration (committed to git):"
echo "  • .loom/config.json         - Terminal and role configuration"
echo "  • .loom/roles/*.md          - Agent role definitions (8 roles)"
echo "  • .loom/scripts/            - Helper scripts (worktree.sh, etc.)"
echo ""
info "Documentation (committed to git):"
echo "  • CLAUDE.md                 - AI context for Claude Code (~11KB)"
echo ""
info "Tooling (committed to git):"
echo "  • .claude/commands/loom/*.md - Slash commands for Claude Code"
echo "  • .github/labels.yml        - Workflow label definitions"
echo "  • .github/ISSUE_TEMPLATE/   - Issue templates"
echo ""
info "Gitignored (local only):"
echo "  • .loom/state.json          - Runtime terminal state"
echo "  • .loom/worktrees/          - Git worktrees for isolated work"
echo "  • .loom/*.log               - Application logs"
echo ""
warning "Modifications:"
echo "  • .gitignore will be updated with Loom patterns"
echo ""
info "GitHub Changes (if using Full Install):"
echo "  • Creates GitHub labels for workflow coordination"
echo "  • Creates tracking issue with 'loom:building' label"
echo "  • Creates pull request with 'loom:review-requested' label"
echo ""
if [[ "$NON_INTERACTIVE" != true ]]; then
  read -r -p "Proceed with installation? [y/N] " -n 1 PROCEED
  echo ""
  if [[ ! $PROCEED =~ ^[Yy]$ ]]; then
    info "Installation cancelled"
    exit 0
  fi
else
  info "Non-interactive mode: proceeding with installation"
fi

# Determine installation method
if [[ -n "$INSTALL_TYPE" ]]; then
  # Installation type was specified via --quick or --full flag
  METHOD="$INSTALL_TYPE"
  if [[ "$METHOD" == "1" ]]; then
    info "Using Quick Install (via --quick flag)"
  else
    info "Using Full Install (via --full flag)"
  fi
elif [[ "$NON_INTERACTIVE" == true ]]; then
  # Non-interactive mode without explicit type defaults to quick install
  METHOD="1"
  info "Non-interactive mode: defaulting to Quick Install"
else
  # Interactive mode: show options and prompt
  echo ""
  header "Installation Options"
  echo ""
  echo "1. Quick Install (Direct)"
  echo "   - Fast installation using loom-daemon init"
  echo "   - No GitHub issue or PR created"
  echo "   - Good for personal projects or quick testing"
  echo ""
  echo "2. Full Install (Workflow)"
  echo "   - Creates GitHub issue to track installation"
  echo "   - Uses git worktree for clean separation"
  echo "   - Syncs labels and creates PR for review"
  echo "   - Recommended for team projects"
  echo ""

  # Retry loop for method selection (up to 3 attempts)
  METHOD=""
  for attempt in 1 2 3; do
    read -r -p "Choose installation method [1/2]: " -n 1 METHOD
    echo ""

    if [[ "$METHOD" == "1" || "$METHOD" == "2" ]]; then
      break
    fi

    if [[ $attempt -lt 3 ]]; then
      warning "Invalid choice '$METHOD'. Please enter 1 or 2."
      echo ""
    else
      error "Invalid choice after 3 attempts. Please run again and select 1 or 2."
    fi
  done
fi

echo ""

case "$METHOD" in
  1)
    info "Running Quick Install..."
    echo ""

    # Check if loom-daemon is built AND up to date with the current source
    # tree (issue #4188 -- a bare existence check reuses a binary built from
    # an older commit after `git pull` landed a newer one).
    if loom_daemon_binary_stale "$LOOM_ROOT"; then
      # Issue #4897: before paying for a `cargo build` (and its
      # `target/.cargo-lock` wait), check whether the ALREADY-INSTALLED
      # machine-level binary was already rebuilt from this exact commit --
      # e.g. by a fleet-mate's install/sweep moments ago -- and reuse it
      # instead of rebuilding.
      if loom_daemon_dest_binary_current "$LOOM_ROOT"; then
        info "loom-daemon already current at ${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}/loom-daemon (matches source HEAD) -- skipping rebuild"
      else
        if [[ -f "$LOOM_ROOT/target/release/loom-daemon" ]]; then
          warning "loom-daemon binary is stale (source tree updated since last build)"
        else
          warning "loom-daemon binary not found"
        fi
        info "Building loom-daemon (this may take a minute)..."
        run_daemon_build_with_progress "$LOOM_ROOT" || error "Failed to build loom-daemon"
        echo ""
      fi
    fi

    # Export LOOM_VERSION / LOOM_COMMIT so the daemon's template substituter
    # fills CLAUDE.md correctly (issue #3502).
    prepare_loom_metadata_env "$LOOM_ROOT"

    # Handle --clean: run local uninstall first, then fresh install
    if [[ "$FORCE_FLAG" == "--clean" ]]; then
      info "Running local uninstall before fresh install..."
      "$LOOM_ROOT/scripts/uninstall-loom.sh" --yes --local "$TARGET_PATH" || \
        error "Uninstall failed - aborting clean install"
      echo ""
      info "Uninstall complete, proceeding with fresh install..."
      "$LOOM_ROOT/target/release/loom-daemon" init --force --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    else
      # Run loom-daemon init
      "$LOOM_ROOT/target/release/loom-daemon" init $FORCE_FLAG --defaults "$LOOM_ROOT/defaults" "$TARGET_PATH" || \
        error "Installation failed"
    fi

    # Install hooks and CLI wrapper (not handled by loom-daemon init).
    # Force-overwrite existing hooks only under --clean (a deliberate fresh
    # install); otherwise preserve a downstream-tuned hook (#3625).
    _HOOK_FORCE=false
    [[ "$FORCE_FLAG" == "--clean" ]] && _HOOK_FORCE=true
    install_hooks_and_cli "$LOOM_ROOT" "$TARGET_PATH" "$_HOOK_FORCE"
    # Emit skill-routes.json, install-metadata.json, loom-source-path (#3502).
    finalize_quick_install "$LOOM_ROOT" "$TARGET_PATH"
    verify_install "$TARGET_PATH"

    # Provision a machine-level loom-daemon binary (#3922) so the consumer's
    # loom-daemon-start.sh resolves it via `command -v loom-daemon` post-install.
    provision_machine_daemon "$LOOM_ROOT/target/release/loom-daemon" || true

    # Guard-hook wiring (#4401) — same call as the --confirm-reinstall branch
    # above. Without it a brand-new `--quick` install ends up with .loom/hooks/
    # copies that nothing references (the 0.16.0 defaults settings.json has no
    # `hooks` block) and no user-scope wiring at all: zero guard coverage.
    wire_quick_install_guard_hooks "$TARGET_PATH"

    # Regenerate the checksum manifest now that ALL settings.json mutations for
    # this install path are complete (issue #5279 — see the function's own
    # comment for why this must run after wire_quick_install_guard_hooks).
    regenerate_manifest_after_hook_wiring "$TARGET_PATH"

    echo ""
    success "Quick installation complete!"
    ;;

  2)
    info "Running Full Install with Workflow..."
    echo ""

    # Detect forge type from remote URL
    cd "$TARGET_PATH"
    _ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
    _DETECTED_FORGE="github"
    if [[ -n "$_ORIGIN_URL" ]] && [[ ! "$_ORIGIN_URL" =~ github\.com ]]; then
      _DETECTED_FORGE="gitea"
    fi

    # Check prerequisites based on detected forge
    if [[ "$_DETECTED_FORGE" == "github" ]]; then
      if ! command -v gh &> /dev/null; then
        error "GitHub CLI (gh) is required for GitHub repos\n       Install: brew install gh\n       For Gitea repos, set GITEA_TOKEN instead"
      fi

      # Check GitHub authentication
      if ! gh auth status &> /dev/null; then
        warning "GitHub CLI is not authenticated"
        info "Please authenticate with GitHub:"
        echo ""
        gh auth login || error "GitHub authentication failed"
        echo ""
      fi
      success "GitHub CLI is authenticated"
    else
      # Gitea forge
      if [[ -z "${GITEA_TOKEN:-${FORGE_TOKEN:-}}" ]]; then
        warning "Gitea detected but no API token found"
        info "Set GITEA_TOKEN or FORGE_TOKEN environment variable"
        info "Create a token at: <your-gitea-instance>/user/settings/applications"
      else
        success "Gitea API token configured"
      fi
    fi
    echo ""

    # Show repository info
    REPO_NAME="unknown"
    if [[ "$_DETECTED_FORGE" == "github" ]]; then
      REPO_INFO=$(gh repo view --json nameWithOwner,description 2>/dev/null || echo "{}")
      REPO_NAME=$(echo "$REPO_INFO" | jq -r '.nameWithOwner // "unknown"' 2>/dev/null || echo "unknown")
    elif [[ -n "$_ORIGIN_URL" ]]; then
      REPO_NAME=$(echo "$_ORIGIN_URL" | sed -E 's/\.git$//; s#^.*[:/]([^/]+/[^/]+)$#\1#' || echo "unknown")
    fi

    if [[ "$REPO_NAME" != "unknown" ]]; then
      info "Target repository: $REPO_NAME (${_DETECTED_FORGE})"
    else
      warning "Could not detect remote repository. This may be a local-only repo."
      read -r -p "Continue anyway? [y/N] " -n 1 CONTINUE_LOCAL
      echo ""
      if [[ ! $CONTINUE_LOCAL =~ ^[Yy]$ ]]; then
        info "Installation cancelled"
        exit 0
      fi
    fi
    echo ""

    # Run the full installation workflow
    exec "$LOOM_ROOT/scripts/install-loom.sh" $FORCE_FLAG ${SOURCE_OVERRIDE_FLAGS[@]+"${SOURCE_OVERRIDE_FLAGS[@]}"} "$TARGET_PATH"
    ;;
esac

echo ""
header "═══════════════════════════════════════════════════════════"
echo ""
