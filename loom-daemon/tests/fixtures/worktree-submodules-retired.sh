#!/usr/bin/env bash
# FROZEN COPY of worktree.sh's submodule initialization as it stood
# immediately before #8195 slice 8 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/worktree_submodules_differential.rs` can keep comparing
# the Rust against the exact implementation it replaced, forever, rather than
# only at the moment of the port. Reading the block out of the live
# worktree.sh stopped being possible the instant that file started delegating,
# and reading it from git history would pin the test to a moving ref.
#
# WHAT IS FROZEN AND WHAT IS NOT
#
# The status pipeline, the `--reference` decision, the `timeout` wrappers and
# the `/tmp` failure flag are byte-for-byte the retired code, variable names
# and all. Two things are NOT frozen, and the comparison is scoped
# accordingly:
#
#   * `print_info`/`print_success`/`print_warning` are re-declared here rather
#     than sourced from the live script's colour block. They are the retired
#     definitions (`echo -e` plus the same escape sequences) — if the live
#     script ever changes them, this fixture keeps modelling the RETIRED
#     shell, which is what a differential must compare against.
#   * `$JSON_OUTPUT` is taken from the environment instead of the script's
#     own argument parsing, so the harness can drive both modes.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour, INCLUDING the behaviour that is wrong:
#
#   * `awk '{print $2}'` truncates a submodule path at its first space;
#   * `MAIN_GIT_DIR` is captured as git's RELATIVE `.git` answer and then
#     tested from inside the worktree, so the `--reference` arm never fires;
#   * the failure flag is a `$$`-keyed path in world-writable `/tmp`;
#   * `timeout(1)` is assumed present, which a stock macOS does not have.
#
# If the Rust should diverge from this, that is a deliberate behaviour change
# that belongs in the port's own module docs (it does — see
# `worktree_cli::submodules` "The one divergence"), and this file should be
# left alone while the test's expectation is updated with a comment saying
# why.
#
# Usage:  worktree-submodules-retired.sh <main-workspace-dir> <abs-worktree-path>
# Env:    JSON_OUTPUT=true|false, LOOM_SUBMODULE_TIMEOUT=<secs>

set -e

MAIN_WORKSPACE_DIR="$1"
ABS_WORKTREE_PATH="$2"
JSON_OUTPUT="${JSON_OUTPUT:-false}"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

cd "$MAIN_WORKSPACE_DIR"

# ─── BEGIN frozen block (worktree.sh, pre-#8195-slice-8) ────────────────────
    MAIN_GIT_DIR=$(git rev-parse --git-common-dir 2>/dev/null)
    UNINIT_SUBMODULES=$(cd "$ABS_WORKTREE_PATH" && git submodule status 2>/dev/null | grep '^-' | wc -l | tr -d ' ')
    SUBMODULE_TIMEOUT="${LOOM_SUBMODULE_TIMEOUT:-300}"

    if [[ "$UNINIT_SUBMODULES" -gt 0 ]]; then
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_info "Initializing $UNINIT_SUBMODULES submodule(s) with shared objects..."
        fi

        cd "$ABS_WORKTREE_PATH"

        # Process each uninitialized submodule
        git submodule status | grep '^-' | awk '{print $2}' | while read -r submod_path; do
            ref_path="$MAIN_GIT_DIR/modules/$submod_path"

            if [[ -d "$ref_path" ]]; then
                # Use reference to share objects with main workspace (fast, no network)
                if ! timeout "$SUBMODULE_TIMEOUT" git submodule update --init --recursive --reference "$ref_path" -- "$submod_path"; then
                    echo "SUBMODULE_FAILED" > /tmp/loom-submodule-status-$$
                fi
            else
                # No reference available, initialize normally (may need network)
                if ! timeout "$SUBMODULE_TIMEOUT" git submodule update --init --recursive -- "$submod_path"; then
                    echo "SUBMODULE_FAILED" > /tmp/loom-submodule-status-$$
                fi
            fi
        done

        # Check if any submodule failed
        if [[ -f "/tmp/loom-submodule-status-$$" ]]; then
            rm -f "/tmp/loom-submodule-status-$$"
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Some submodules failed to initialize (worktree still created)"
                print_info "See stderr above for the underlying git error."
                print_info "You may need to run: git submodule update --init --recursive"
            fi
        else
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_success "Submodules initialized with shared objects"
            fi
        fi

        # Return to original directory
        cd - > /dev/null
    fi
# ─── END frozen block ───────────────────────────────────────────────────────

exit 0
