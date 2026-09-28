#!/usr/bin/env bash
# FROZEN COPY of worktree.sh's sparse-checkout family as it stood immediately
# before #8195 slice 10 ported it to `loom-daemon worktree-sparse`.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
# It exists so `tests/worktree_sparse_differential.rs` can keep comparing the
# Rust against the exact implementation it replaced, forever — reading the
# block out of the live worktree.sh stopped being possible the moment that file
# started delegating.
#
# WHAT IS FROZEN: every function body and both call-site blocks below are
# byte-for-byte the retired code (print_* included, so `echo -e` is modelled),
# copied from origin/main's worktree.sh. WHAT IS NOT: the harness plumbing at
# the bottom, which stands in for the script's own argument parsing —
#
#   worktree-sparse-retired.sh <create|reconfigure> <worktree-path> (--full | <paths...>)
#   env: JSON_OUTPUT, ISSUE_NUMBER, BRANCH_NAME, LOOM_WORKTREE_ALWAYS_INCLUDE
#   cwd: the main workspace, as it is in worktree.sh by this point
#
# For the create arm under --json the retired script spliced $CONE_JSON into
# its final document; the plumbing prints it alone on fd 3, which is exactly
# what the port's create arm prints on stdout.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour INCLUDING what is wrong with it: the silent `set -e` exit
# on a cone git rejects, the unescaped awk JSON builder, and the substring
# `git worktree list | grep -q` registration check. The port's module docs
# (`worktree_cli::sparse`) name each of those as a deliberate divergence, and
# the harness pins each one as a disagreement.

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

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

LOOM_WORKTREE_ALWAYS_INCLUDE_DEFAULT=(.claude .loom .githooks scripts)

apply_sparse_cone() {
    local wt_path="$1"
    shift
    local paths=("$@")

    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_info "Configuring sparse-checkout cone..."
    fi

    git -C "$wt_path" sparse-checkout init --cone >/dev/null 2>&1
    # `sparse-checkout set` replaces the cone (idempotent: same paths = no-op).
    git -C "$wt_path" sparse-checkout set "${paths[@]}" >/dev/null 2>&1
}
materialize_sparse_cone() {
    local wt_path="$1"
    git -C "$wt_path" checkout >/dev/null 2>&1 || true
}
disable_sparse_checkout() {
    local wt_path="$1"

    if [[ "$JSON_OUTPUT" != "true" ]]; then
        print_info "Disabling sparse-checkout (full mode)..."
    fi

    if git -C "$wt_path" sparse-checkout disable >/dev/null 2>&1; then
        :
    else
        # Fallback: manually unset per-worktree config keys.
        git -C "$wt_path" config --unset core.sparseCheckout 2>/dev/null || true
        git -C "$wt_path" config --unset core.sparseCheckoutCone 2>/dev/null || true
    fi
    # Re-materialize the full working tree.
    git -C "$wt_path" checkout >/dev/null 2>&1 || true
}
log_worktree_size() {
    local wt_path="$1"
    local label="${2:-Worktree size}"
    if [[ "$JSON_OUTPUT" == "true" ]]; then
        return 0
    fi
    local size
    size=$(du -sh "$wt_path" 2>/dev/null | awk '{print $1}')
    if [[ -n "$size" ]]; then
        print_info "$label: $size"
    fi
}

# ---- harness plumbing (NOT retired code) ----
ARM="$1"; WORKTREE_PATH="$2"; shift 2
SPARSE_MODE=false; FULL_MODE=false; SPARSE_PATHS=()
if [[ "${1:-}" == "--full" ]]; then FULL_MODE=true; else SPARSE_MODE=true; SPARSE_PATHS=("$@"); fi
if [[ "$JSON_OUTPUT" == "true" ]]; then
    exec 3>&1 1>&2
    trap '' PIPE
else
    exec 3>&1
fi
# ---- end plumbing ----

# Build the always-included safety set, allowing repo override via env var.
ALWAYS_INCLUDE=("${LOOM_WORKTREE_ALWAYS_INCLUDE_DEFAULT[@]}")
if [[ -n "${LOOM_WORKTREE_ALWAYS_INCLUDE:-}" ]]; then
    # Split on whitespace
    # shellcheck disable=SC2206
    EXTRA_INCLUDE=(${LOOM_WORKTREE_ALWAYS_INCLUDE})
    ALWAYS_INCLUDE+=("${EXTRA_INCLUDE[@]}")
fi

# ---- plumbing: select the arm ----
if [[ "$ARM" == "reconfigure" ]]; then
    if [[ "$SPARSE_MODE" == "true" || "$FULL_MODE" == "true" ]]; then
        if ! git worktree list | grep -q "$WORKTREE_PATH"; then
            if [[ "$JSON_OUTPUT" == "true" ]]; then
                echo '{"success": false, "error": "Directory exists but is not a registered worktree"}' >&3
            else
                print_error "Directory exists but is not a registered worktree: $WORKTREE_PATH"
            fi
            exit 1
        fi

        if [[ "$FULL_MODE" == "true" ]]; then
            disable_sparse_checkout "$WORKTREE_PATH"
            log_worktree_size "$WORKTREE_PATH" "Worktree size (full)"
            # Back-fill/refresh the Loom sentinel so re-config of an existing
            # (possibly sentinel-less) worktree stays cleanup-eligible (#3548).
            write_loom_sentinel "$WORKTREE_PATH"
            if [[ "$JSON_OUTPUT" == "true" ]]; then
                ABS_WT=$(cd "$WORKTREE_PATH" && pwd)
                echo '{"success": true, "worktreePath": "'"$ABS_WT"'", "branchName": "'"$BRANCH_NAME"'", "issueNumber": '"$ISSUE_NUMBER"', "sparse": false, "cone": []}' >&3
            else
                print_success "Worktree converted to full checkout"
                print_info "To use this worktree: cd $WORKTREE_PATH"
            fi
            exit 0
        fi

        # SPARSE_MODE
        CONE_PATHS=("${SPARSE_PATHS[@]}" "${ALWAYS_INCLUDE[@]}")
        apply_sparse_cone "$WORKTREE_PATH" "${CONE_PATHS[@]}"
        materialize_sparse_cone "$WORKTREE_PATH"
        log_worktree_size "$WORKTREE_PATH" "Worktree size (sparse)"
        # Back-fill/refresh the Loom sentinel so re-config of an existing
        # (possibly sentinel-less) worktree stays cleanup-eligible (#3548).
        write_loom_sentinel "$WORKTREE_PATH"
        if [[ "$JSON_OUTPUT" == "true" ]]; then
            ABS_WT=$(cd "$WORKTREE_PATH" && pwd)
            CONE_JSON=$(printf '%s\n' "${CONE_PATHS[@]}" | awk 'BEGIN{printf "["} {if(NR>1)printf ","; printf "\"%s\"", $0} END{printf "]"}')
            echo '{"success": true, "worktreePath": "'"$ABS_WT"'", "branchName": "'"$BRANCH_NAME"'", "issueNumber": '"$ISSUE_NUMBER"', "sparse": true, "cone": '"$CONE_JSON"'}' >&3
        else
            print_success "Sparse-checkout cone applied"
            print_info "To use this worktree: cd $WORKTREE_PATH"
        fi
        exit 0
    fi
else
    ABS_WORKTREE_PATH="$WORKTREE_PATH"
    SPARSE_CONE_PATHS=()
    if [[ "$SPARSE_MODE" == "true" ]]; then
        SPARSE_CONE_PATHS=("${SPARSE_PATHS[@]}" "${ALWAYS_INCLUDE[@]}")
        apply_sparse_cone "$ABS_WORKTREE_PATH" "${SPARSE_CONE_PATHS[@]}"
        materialize_sparse_cone "$ABS_WORKTREE_PATH"
        log_worktree_size "$ABS_WORKTREE_PATH" "Sparse worktree size"
    fi
    # (the retired final-document block's cone builder, verbatim:)
    if [[ "$SPARSE_MODE" == "true" ]]; then
            CONE_JSON=$(printf '%s\n' "${SPARSE_CONE_PATHS[@]}" | awk 'BEGIN{printf "["} {if(NR>1)printf ","; printf "\"%s\"", $0} END{printf "]"}')
    fi
    if [[ "$JSON_OUTPUT" == "true" ]]; then echo "$CONE_JSON" >&3; fi
fi
