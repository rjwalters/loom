#!/usr/bin/env bash
# Post-worktree hook: provide loom-daemon binary for the new worktree
#
# Called by worktree.sh after creating a new worktree.
# Arguments: $1=worktree_path  $2=branch_name  $3=issue_number
# Working directory: the new worktree
#
# Copies loom-daemon from the main workspace's target/release/ instead of
# rebuilding from scratch. This avoids cargo lock contention and minutes-long
# release builds that block parallel worktrees.
#
# Falls back to building only if the main workspace binary doesn't exist.
#
# Issue #6013: Cargo's build output is NOT necessarily `<repo>/target/` -- a
# `CARGO_TARGET_DIR` env var or `build.target-dir` in `~/.cargo/config.toml`
# redirects it wholesale (issue #5922). A hardcoded
# `$MAIN_WORKSPACE/target/release/loom-daemon` lookup is therefore always
# "missing" on such a host, so every worktree fell through to a full
# `cargo build --release -p loom-daemon`, reintroducing the lock-contention
# rebuild storm that #2291 originally fixed. Resolve the real target dir the
# same way `scripts/cargo-target-dir.sh` does (CARGO_TARGET_DIR env ->
# `cargo metadata` -> `<root>/target` fallback) instead of assuming the
# default layout.
#
# Issue #8458 (per-worktree target dirs): the SOURCE and the DESTINATION are now
# resolved by different rules, and conflating them is how this hook would break a
# second time.
#
#   * DESTINATION: `worktree.sh` may have provisioned this worktree its own
#     target dir (`<root>/wt/issue-N`), announced via
#     `LOOM_WORKTREE_CARGO_TARGET_DIR` and recorded in the
#     `.loom-cargo-target-dir` marker. Prefer either over resolution, because
#     neither `scripts/cargo-target-dir.sh` nor `cargo metadata` knows about the
#     marker -- they would resolve to the SHARED root and this hook would seed the
#     wrong directory.
#   * SOURCE: still `$MAIN_WORKSPACE`'s own resolved target dir, and it must be
#     resolved with a per-worktree `CARGO_TARGET_DIR` OUT OF THE WAY. The spawn
#     path exports that variable for the whole sweep, and since env beats config
#     in Cargo, honoring it for the MAIN workspace would point the lookup at the
#     brand-new (empty) per-worktree dir -- reporting the main binary "missing" on
#     every worktree creation and falling through to a full release build. That is
#     exactly #6013/#6014's rebuild storm, so the strip is narrowly conditional: a
#     genuinely session-global `CARGO_TARGET_DIR` (an operator redirecting ALL
#     builds, including the main workspace's) does NOT carry the per-worktree
#     shape and is still honored, unchanged.
#
# `tests/hooks/test-post-worktree-target-dir.sh` covers both, including the
# rebuild-storm regression under the per-worktree scheme.

set -euo pipefail

WORKTREE_PATH="${1:?worktree path required}"

# Only proceed if the worktree has a Cargo workspace with loom-daemon
if [[ ! -f "$WORKTREE_PATH/Cargo.toml" ]]; then
    exit 0
fi

if ! grep -q 'loom-daemon' "$WORKTREE_PATH/Cargo.toml" 2>/dev/null; then
    exit 0
fi

# Find the main workspace (parent of .loom/worktrees/) and its target-dir
# resolution helper (#5922).
MAIN_WORKSPACE="$(cd "$WORKTREE_PATH" && git rev-parse --git-common-dir 2>/dev/null | xargs dirname)"
CARGO_TARGET_DIR_SCRIPT="$MAIN_WORKSPACE/scripts/cargo-target-dir.sh"

# Resolve the real (possibly redirected) target dir for a given workspace
# root. Falls back to the pre-#6013 hardcoded assumption (`<root>/target`) if
# the helper script itself is missing (e.g. a partial checkout) -- mirrors
# cargo-target-dir.sh's own internal fallback, so this degrades exactly to
# the old behavior rather than breaking a worktree creation outright.
# `strip_env=1` runs the resolution with CARGO_TARGET_DIR removed from the
# environment entirely (`env -u`, not an assignment prefix: the helper is an
# external script and must not merely see an empty value it might still honor).
resolve_target_dir() {
    local workspace_root="$1" strip_env="${2:-0}"
    if [[ ! -x "$CARGO_TARGET_DIR_SCRIPT" ]]; then
        echo "$workspace_root/target"
        return 0
    fi
    if [[ "$strip_env" == "1" ]]; then
        env -u CARGO_TARGET_DIR "$CARGO_TARGET_DIR_SCRIPT" "$workspace_root" 2>/dev/null \
            || echo "$workspace_root/target"
    else
        "$CARGO_TARGET_DIR_SCRIPT" "$workspace_root" 2>/dev/null || echo "$workspace_root/target"
    fi
}

# Per-worktree scheme helpers (#8458). Source the installed lib when it is there,
# so `lib/cargo-target-dir.sh` stays the single authority. Both of its
# per-worktree predicates delegate to `loom-daemon cargo-target-dir`, so
# `lib/locate-daemon-bin.sh` (which defines the resolver they call) is sourced
# alongside it -- without it the lib's predicates would degrade to "no daemon"
# and silently answer "not per-worktree" for everything.
LOOM_SCRIPTS_LIB_DIR="$MAIN_WORKSPACE/.loom/scripts/lib"
[[ -d "$LOOM_SCRIPTS_LIB_DIR" ]] || LOOM_SCRIPTS_LIB_DIR="$MAIN_WORKSPACE/defaults/scripts/lib"
if [[ -f "$LOOM_SCRIPTS_LIB_DIR/locate-daemon-bin.sh" ]]; then
    # shellcheck source=/dev/null
    source "$LOOM_SCRIPTS_LIB_DIR/locate-daemon-bin.sh" || true
fi
if [[ -f "$LOOM_SCRIPTS_LIB_DIR/cargo-target-dir.sh" ]]; then
    # shellcheck source=/dev/null
    source "$LOOM_SCRIPTS_LIB_DIR/cargo-target-dir.sh" || true
fi

# Degraded twins for a partial checkout without the lib -- and for the case the
# lib is present but no `loom-daemon` binary resolves, where its own predicates
# correctly answer "no daemon, so nothing was ever provisioned". These are NOT an
# alternative resolution path: the per-worktree DIRECTORY is still whatever the
# lib/worktree.sh chose (it arrives by env var or marker, never derived here).
# They exist because the alternative -- silently skipping the strip below -- is
# the #6013/#6014 rebuild storm, which is strictly worse than a short structural
# check duplicated for the no-lib case. Both paths are covered by
# tests/hooks/test-post-worktree-target-dir.sh (test 5 with the lib, 5d without).
#
# The depth floor mirrors `per_worktree::is_attributable`'s
# `components().count() < 4`: `/wt/<name>` alone has no real root above it, and
# the shallow-path family must be unreachable through any of these twins.
_pw_fallback_is_per_worktree_target_dir() {
    local wt="${1%/}" candidate="${2%/}"
    [[ "$candidate" == /* ]] || return 1
    [[ "$candidate" == */wt/* ]] || return 1
    [[ "$(basename "$candidate")" == "$(basename "$wt")" ]] || return 1
    [[ "$(basename "$(dirname "$candidate")")" == "wt" ]] || return 1
    [[ "$(printf '%s' "${candidate#/}" | awk -F/ '{print NF}')" -ge 3 ]]
}
if ! declare -F loom_is_per_worktree_target_dir >/dev/null 2>&1; then
    loom_is_per_worktree_target_dir() { _pw_fallback_is_per_worktree_target_dir "$@"; }
fi
if ! declare -F loom_read_worktree_target_dir_marker >/dev/null 2>&1; then
    loom_read_worktree_target_dir_marker() {
        local wt="${1%/}" value
        [[ -f "$wt/Cargo.toml" && -s "$wt/.loom-cargo-target-dir" ]] || return 1
        value="$(head -n 1 "$wt/.loom-cargo-target-dir")"
        value="${value%/}"
        loom_is_per_worktree_target_dir "$wt" "$value" || return 1
        printf '%s\n' "$value"
    }
fi

# DESTINATION: what worktree.sh provisioned for THIS worktree, if anything.
if [[ -n "${LOOM_WORKTREE_CARGO_TARGET_DIR:-}" ]]; then
    WORKTREE_TARGET_DIR="${LOOM_WORKTREE_CARGO_TARGET_DIR%/}"
elif MARKED_TARGET_DIR="$(loom_read_worktree_target_dir_marker "$WORKTREE_PATH")"; then
    WORKTREE_TARGET_DIR="$MARKED_TARGET_DIR"
else
    WORKTREE_TARGET_DIR="$(resolve_target_dir "$WORKTREE_PATH")"
fi

# SOURCE: the main workspace's own target dir. A per-worktree CARGO_TARGET_DIR is
# stripped for this resolution only -- see the header for why honoring it here is
# #6013/#6014's rebuild storm.
# The structural fallback is OR'd in deliberately: it is the same rule, and the
# authoritative predicate answers "no" on a host where no `loom-daemon` binary
# resolves. Failing to strip is the rebuild storm; over-stripping is impossible,
# because both forms require the shape that only a provisioned per-worktree dir
# has.
if [[ -n "${CARGO_TARGET_DIR:-}" ]] \
    && { loom_is_per_worktree_target_dir "$WORKTREE_PATH" "$CARGO_TARGET_DIR" \
        || _pw_fallback_is_per_worktree_target_dir "$WORKTREE_PATH" "$CARGO_TARGET_DIR"; }; then
    MAIN_TARGET_DIR="$(resolve_target_dir "$MAIN_WORKSPACE" 1)"
else
    MAIN_TARGET_DIR="$(resolve_target_dir "$MAIN_WORKSPACE")"
fi

WORKTREE_BINARY="$WORKTREE_TARGET_DIR/release/loom-daemon"
MAIN_BINARY="$MAIN_TARGET_DIR/release/loom-daemon"

# Skip if the binary already exists (e.g., reusing an existing worktree, or a
# redirected CARGO_TARGET_DIR shared verbatim across worktrees)
if [[ -x "$WORKTREE_BINARY" ]]; then
    echo "  loom-daemon binary already exists, skipping"
    exit 0
fi

# Try to copy from main workspace first (instant, no cargo lock contention)
if [[ -x "$MAIN_BINARY" ]]; then
    mkdir -p "$(dirname "$WORKTREE_BINARY")"
    if cp "$MAIN_BINARY" "$WORKTREE_BINARY"; then
        echo "  loom-daemon copied from main workspace (skipped rebuild)"
        exit 0
    fi
fi

# Fallback: build if main binary doesn't exist and cargo is available
if ! command -v cargo &>/dev/null; then
    echo "  cargo not found and no main workspace binary, skipping loom-daemon setup"
    exit 0
fi

echo "  Building loom-daemon (release)..."
echo "  (main workspace binary not found at $MAIN_BINARY)"
# CARGO_TARGET_DIR is pinned to the resolved DESTINATION rather than left to
# cargo: under the per-worktree scheme (#8458) the marker, not the config
# hierarchy, is what says where this worktree builds, and a rebuild that landed in
# the shared root would both miss the binary check above on the next run and
# re-create the cross-worktree uplift collision this scheme exists to remove.
if CARGO_TARGET_DIR="$WORKTREE_TARGET_DIR" cargo build --release -p loom-daemon --manifest-path "$WORKTREE_PATH/Cargo.toml" 2>&1; then
    echo "  loom-daemon build complete"
else
    echo "  loom-daemon build failed (non-fatal, worktree still usable)"
fi

# Restore Cargo.lock — the build output is in target/ (gitignored),
# but cargo may update the lockfile which confuses shepherd diagnostics.
git -C "$WORKTREE_PATH" checkout -- Cargo.lock 2>/dev/null || true
