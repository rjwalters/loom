#!/usr/bin/env bash
# worktree-existing-retired.sh — FROZEN copy of the shell this slice retired.
#
# `defaults/scripts/worktree.sh`'s "the worktree directory already exists" arm
# exactly as it stood immediately before #8195 slice 12 replaced it with a
# delegation to `loom-daemon worktree-existing`:
#
#   the registration probe    `git worktree list | grep -q "$WORKTREE_PATH"`
#   the working-tree reading  `git status --porcelain`, taken before the fetch
#   the drift check           `_worktree_upstream_check registered-worktree`
#   the staleness reference   `loom-daemon worktree-stale-ref`, with its
#                             pre-#8287 fallback assignments intact
#   the verdict               preserve vs. reset, its messages and their
#                             `$JSON_OUTPUT` gating, verbatim
#   the reset                 `git fetch` && `loom_worktree_reset_or_rescue`
#   the refusal               "Directory exists but is not a registered worktree"
#
# It exists for exactly one consumer:
# `loom-daemon/tests/worktree_existing_differential.rs`.
#
# WHY A FROZEN COPY RATHER THAN THE LIVE SOURCE
#
# The arm was inline in `worktree.sh`'s main body — not a function — and is gone
# from the tree entirely after the port, so there is nothing left to `source`.
# Reading it out of git history instead would pin the test to a moving ref.
#
# WHAT IS *NOT* FROZEN HERE, DELIBERATELY
#
# The three pieces the arm already delegated to `loom-daemon` before this slice
# — `worktree-upstream` (slice 9), `worktree-stale-ref` (#8354) and
# `worktree-reset` (slice 6) — are invoked here as subcommands of the same
# binary the port calls in-process. That is the point: they are NOT what slice
# 12 changed, so keeping them identical on both sides makes every difference the
# harness reports attributable to the arm's own control flow rather than to a
# re-implementation of something that was already ported.
#
# DO NOT "FIX" ANYTHING HERE — the defect IS the thing being compared against:
#
#   - `git worktree list | grep -q "$WORKTREE_PATH"` compares the porcelain's
#     symlink-RESOLVED paths against the unresolved path the script built by
#     concatenation, as an unanchored REGEX substring. It is wrong in both
#     directions (a live worktree under a symlinked root reads as unregistered;
#     `issue-4` reads as registered because `issue-44` is), and reproducing both
#     is why this file exists.
#
# The four `print_*` helpers and the `>&3` convention are copied verbatim from
# `worktree.sh` (fd 3 is the real stdout there; the caller opens it).
#
# Usage: worktree-existing-retired.sh
# Environment (all required unless noted):
#   WORKTREE_PATH WORKTREE_REPO_ROOT ISSUE_NUMBER BRANCH_NAME
#   DEFAULT_BRANCH BASE_REF BASE_DISPLAY
#   BASE_BRANCH   (optional, the `--base` override; empty when absent)
#   JSON_OUTPUT   (true|false, default false)
#   LOOM_DAEMON_SELF_BIN  the binary the three pre-existing delegations exec

# `set -e` and nothing else — `worktree.sh`'s own line 32. NOT `-u`/`pipefail`:
# the arm below relies on both absences (`${3:-}` style guards are hand-written
# there, and `git worktree list | grep -q` is a deliberate early-exit pipeline).
set -e

JSON_OUTPUT="${JSON_OUTPUT:-false}"
BASE_BRANCH="${BASE_BRANCH:-}"

# `worktree.sh` reaches this arm with cwd at the main workspace — it
# auto-navigates out of any worktree first — and the registration probe below
# is the one command that reads it.
cd "$WORKTREE_REPO_ROOT"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

print_error() {
    echo -e "${RED}ERROR: $1${NC}" >&2
}

print_success() {
    echo -e "${GREEN}✓ $1${NC}"
}

print_info() {
    echo -e "${BLUE}ℹ $1${NC}"
}

print_warning() {
    echo -e "${YELLOW}⚠ $1${NC}"
}

if [[ "$JSON_OUTPUT" == "true" ]]; then
    exec 3>&1 1>&2
else
    exec 3>&1
fi

# --- retired: write_loom_sentinel ------------------------------------------
write_loom_sentinel() {
    local wt="$1"
    cat > "$wt/.loom-managed" <<EOF
# Loom-managed worktree marker
# Created by .loom/scripts/worktree.sh
# Issue: $ISSUE_NUMBER
# Branch: $BRANCH_NAME
# Removing this file makes Loom treat the worktree as user-owned and refuse
# to clean it up automatically.
EOF
}

_WT_DAEMON_BIN="${LOOM_DAEMON_SELF_BIN:-}"

# --- retired: _worktree_upstream_check (the slice-9 delegation, verbatim) ---
_worktree_upstream_check() {
    [[ -n "${_WT_DAEMON_BIN:-}" ]] || return 0

    local _q="" _u=""
    [[ "$JSON_OUTPUT" == "true" ]] && _q="--quiet"
    [[ -n "${3:-}" ]] && _u="--uncommitted"
    # shellcheck disable=SC2086
    "$_WT_DAEMON_BIN" worktree-upstream --arm "$1" --repo "$2" \
        --branch "$BRANCH_NAME" --issue "$ISSUE_NUMBER" $_q $_u || true
    return 0
}

# --- retired: loom_worktree_reset_or_rescue (lib/worktree-race-rescue.sh) ---
#
# Copied rather than sourced: the library is still shipped and still delegates
# to the same `worktree-reset` subcommand, but sourcing it would make this
# fixture's behaviour depend on a file a LATER slice may change.
loom_worktree_reset_or_rescue() {
    local worktree_path="$1"
    local target_ref="$2"
    local rescue_label="${3:-loom-race-rescue}"

    if [[ -z "$_WT_DAEMON_BIN" ]] || ! "$_WT_DAEMON_BIN" worktree-reset --help >/dev/null 2>&1; then
        echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path — no loom-daemon with a 'worktree-reset' subcommand could be resolved to run the guard (this install predates #8195 slice 6, or is incomplete; re-run the Loom installer or resync .loom/); leaving the worktree untouched" >&2
        return 1
    fi

    "$_WT_DAEMON_BIN" worktree-reset \
        --worktree "$worktree_path" \
        --target-ref "$target_ref" \
        --rescue-label "$rescue_label" \
        --ignore-pid "$$" \
        --ignore-pid "${BASHPID:-$$}"
}

# ===========================================================================
# retired: the "worktree directory already exists" arm, verbatim
# ===========================================================================

# Check if it's registered with git
if git worktree list | grep -q "$WORKTREE_PATH"; then
    # The working-tree reading comes FIRST now (#8287): both the drift check
    # below and the staleness reference after it consume it, and the
    # reference also needs that check's `git fetch origin $BRANCH_NAME` to
    # have run so `origin/$BRANCH_NAME` is the branch's real pushed tip
    # rather than whatever this worktree last saw.
    local_uncommitted=$(git -C "$WORKTREE_PATH" status --porcelain 2>/dev/null) || local_uncommitted=""

    _worktree_upstream_check registered-worktree "$WORKTREE_PATH" "$local_uncommitted"

    stale_ref="$BASE_REF" stale_display="$BASE_DISPLAY" local_commits_ahead=$(git -C "$WORKTREE_PATH" rev-list --count "$BASE_REF..HEAD" 2>/dev/null || echo 0) local_commits_behind=$(git -C "$WORKTREE_PATH" rev-list --count "HEAD..$BASE_REF" 2>/dev/null || echo 0)
    [[ -z "${_WT_DAEMON_BIN:-}" ]] || read -r stale_ref stale_display local_commits_ahead local_commits_behind <<< "$("$_WT_DAEMON_BIN" worktree-stale-ref --worktree "$WORKTREE_PATH" --branch "$BRANCH_NAME" --default-branch "$DEFAULT_BRANCH" --base-ref "$BASE_REF" --base-display "$BASE_DISPLAY" 2>/dev/null || echo "$stale_ref $stale_display $local_commits_ahead $local_commits_behind")"

    if [[ "$local_commits_ahead" -gt 0 || -n "$local_uncommitted" ]]; then
        # Worktree has real work - preserve it
        write_loom_sentinel "$WORKTREE_PATH"
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_info "Worktree is registered with git"
            if [[ "$local_commits_ahead" -gt 0 ]]; then
                print_info "Worktree has $local_commits_ahead commit(s) ahead of $stale_display - preserving existing work"
            elif [[ -n "$local_uncommitted" ]]; then
                print_info "Worktree has uncommitted changes - preserving existing work"
            fi
            echo ""
            print_info "To use this worktree: cd $WORKTREE_PATH"
        fi
        exit 0
    else
        # Stale worktree: no commits ahead, no uncommitted changes
        if [[ "$JSON_OUTPUT" != "true" ]]; then
            print_warning "Stale worktree detected (0 commits ahead, $local_commits_behind behind $stale_display, no uncommitted changes)"
            print_info "Resetting worktree in place to $stale_display..."
        fi

        write_loom_sentinel "$WORKTREE_PATH"
        # `--` ends option parsing (#9106) — carried into this frozen copy so it
        # matches the arm as main last shipped it, not a pre-hardening snapshot.
        if git -C "$WORKTREE_PATH" fetch origin -- "${BASE_BRANCH:-$DEFAULT_BRANCH}" 2>/dev/null && \
           loom_worktree_reset_or_rescue "$WORKTREE_PATH" "$stale_ref" "issue-$ISSUE_NUMBER-stale-worktree-reset"; then
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_success "Stale worktree reset to $stale_display"
                echo ""
                print_info "To use this worktree: cd $WORKTREE_PATH"
            fi
            exit 0
        else
            if [[ "$JSON_OUTPUT" != "true" ]]; then
                print_warning "Could not reset stale worktree (continuing to use as-is)"
                echo ""
                print_info "To use this worktree: cd $WORKTREE_PATH"
            fi
            exit 0
        fi
    fi
else
    print_error "Directory exists but is not a registered worktree"
    echo ""
    print_info "To fix this:"
    echo "  1. Remove the directory: rm -rf $WORKTREE_PATH"
    echo "  2. Run again: pnpm worktree $ISSUE_NUMBER"
    exit 1
fi
