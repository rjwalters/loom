#!/usr/bin/env bash
# FROZEN COPY of worktree.sh's post-`git worktree add` finalization block as it
# stood immediately before #8195 slice 16 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/worktree_postadd_differential.rs` can keep comparing the
# Rust against the exact implementation it replaced, forever, rather than only
# at the moment of the port. Reading the blocks out of the live worktree.sh
# stopped being possible the instant that file started delegating, and reading
# them from git history would pin the test to a moving ref.
#
# WHAT IS FROZEN AND WHAT IS NOT
#
# The three blocks are byte-for-byte the retired code — the `core.hooksPath`
# guard (#3638), the `cargo-target-dir provision` export (#8458) and the
# post-worktree hook invocation — variable names and all. Three things are NOT
# frozen, and the comparison is scoped accordingly:
#
#   * `print_info`/`print_success`/`print_warning` are re-declared here rather
#     than sourced from the live script's colour block. They are the retired
#     definitions (`echo -e` plus the same escape sequences) — if the live
#     script ever changes them, this fixture keeps modelling the RETIRED
#     shell, which is what a differential must compare against.
#   * `$JSON_OUTPUT` is taken from the environment instead of the script's own
#     argument parsing, so the harness can drive both modes.
#   * `$_pwt_bin` is taken from the environment (`LOOM_FIXTURE_DAEMON_BIN`)
#     instead of `loom_locate_daemon_bin`. The retired line's resolution TIER
#     is one of the port's two deliberate changes and is argued in
#     `worktree_cli::postadd`'s module doc; freezing the resolver here would
#     make the fixture depend on what happens to be installed on the host
#     rather than on the binary under test, which is the #8176 stale-binary
#     trap. The RESOLVED value is passed in identically to both sides.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour. In particular the bare `git -C … config` below runs under
# `set -e` and aborts the whole script on failure; the port warns and
# continues. That divergence is argued in the port's module doc and pinned by
# the harness, and this file must keep aborting.
#
# Usage: worktree-postadd-retired.sh <repo-root> <worktree> <branch> <issue>

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

print_error() { echo -e "${RED}ERROR: $1${NC}" >&2; }
print_success() { echo -e "${GREEN}✓ $1${NC}"; }
print_info() { echo -e "${BLUE}ℹ $1${NC}"; }
print_warning() { echo -e "${YELLOW}⚠ $1${NC}"; }

WORKTREE_REPO_ROOT="$1"
MAIN_WORKSPACE_DIR="$1"
ABS_WORKTREE_PATH="$2"
BRANCH_NAME="$3"
ISSUE_NUMBER="$4"
JSON_OUTPUT="${JSON_OUTPUT:-false}"

# --- retired block 1: core.hooksPath (#3638) --------------------------------
# Set git hooks path so .githooks/ works in worktrees (no npx/husky needed).
# Only when the repo actually ships a .githooks/ dir — otherwise pointing
# core.hooksPath at a missing dir silently disables all hooks (git treats a
# nonexistent hooksPath as "no hooks"). $WORKTREE_REPO_ROOT is the main repo
# root captured at L824 (cwd is the main workspace here, not the worktree).
if [[ -d "$WORKTREE_REPO_ROOT/.githooks" ]]; then
    git -C "$ABS_WORKTREE_PATH" config core.hooksPath .githooks
fi

# --- retired block 2: the per-worktree cargo target dir (#8458) -------------
# #8458: give this worktree its own Cargo target dir under the otherwise
# shared root and record it in the `.loom-cargo-target-dir` marker, so the
# removal paths (`loom-daemon worktree-remove`, merge-pr.sh, `loom-daemon
# clean`, the reaper) can attribute and reclaim it. Off unless the repo opts
# in; a pure no-op on
# a host whose Cargo output is not redirected outside the worktree.
#
# Sets LOOM_WORKTREE_CARGO_TARGET_DIR for the post-worktree hook below —
# NOT CARGO_TARGET_DIR, which would make the hook's main-workspace binary
# lookup miss and reintroduce #6013/#6014's rebuild storm.
#
# Always `|| true`: the daemon binary may not be built yet (this runs at
# worktree creation, before the hook that seeds one), and a build-cache
# optimisation must never fail a worktree creation. Empty stdout means
# "no directory" — the subcommand exits 0 for every not-applicable case.
# `--report`'s stderr is deliberately NOT swallowed (stdout is the directory,
# which `--json` mode needs clean): it is the one operator-visible sign the
# scheme is on. Exporting an empty value is harmless — every consumer tests
# `-n` — so no second statement is needed to unset it.
_pwt_bin="${LOOM_FIXTURE_DAEMON_BIN:-}"
[[ -z "${_pwt_bin:-}" ]] || export LOOM_WORKTREE_CARGO_TARGET_DIR="$("$_pwt_bin" cargo-target-dir \
    provision --repo-root "$MAIN_WORKSPACE_DIR" --report "$ABS_WORKTREE_PATH" || true)"

# --- retired block 3: the project post-worktree hook ------------------------
# Run project-specific post-worktree hook if it exists
# This allows projects to add custom setup steps (e.g., pnpm install, lake exe cache get)
# The hook is stored in .loom/hooks/ which is NOT overwritten by Loom upgrades
# Note: MAIN_WORKSPACE_DIR is already set by the submodule section above
POST_WORKTREE_HOOK="$MAIN_WORKSPACE_DIR/.loom/hooks/post-worktree.sh"
if [[ -x "$POST_WORKTREE_HOOK" ]]; then
    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_info "Running project-specific post-worktree hook..."
    fi

    # Run the hook from the new worktree directory
    # Pass: worktree path, branch name, issue number
    if (cd "$ABS_WORKTREE_PATH" && "$POST_WORKTREE_HOOK" "$ABS_WORKTREE_PATH" "$BRANCH_NAME" "$ISSUE_NUMBER"); then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_success "Post-worktree hook completed"
        fi
    else
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_warning "Post-worktree hook failed (worktree still created)"
        fi
    fi
fi

exit 0
