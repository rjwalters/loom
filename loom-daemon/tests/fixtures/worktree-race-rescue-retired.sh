#!/usr/bin/env bash
# worktree-race-rescue-retired.sh — FROZEN copy of the shell this slice retired.
#
# This is `loom_worktree_has_live_process` + `loom_worktree_reset_or_rescue`
# exactly as they stood in `defaults/scripts/lib/worktree-race-rescue.sh`
# immediately before #8195 slice 6 replaced the bodies with a delegation to
# `loom-daemon worktree-reset`. It exists for exactly one consumer:
# `loom-daemon/tests/worktree_reset_differential.rs`.
#
# WHY A FROZEN COPY RATHER THAN THE LIVE SOURCE
#
# The retained suite (`test-worktree-race-rescue.sh`) sources the live lib and
# calls the function directly — which is what keeps those 25 assertions valid
# against the port, but also means the live file no longer contains an
# implementation to compare AGAINST. Reading it out of git history instead
# would pin the test to a moving ref.
#
# DO NOT "FIX" ANYTHING HERE. A bug in this file is the point: it is the
# behaviour the port is being compared against, including the parts that are
# only correct because #7463 corrected them (`-F pf` rather than `-F pt`, and
# the recursive `+D` rather than `+d`).
#
# Nothing is stubbed. Unlike the cleanup fixture, these two functions call no
# helper from worktree.sh at all — every message is a bare `echo … >&2`, which
# is exactly why this family was separable into its own lib in the first place.
#
# Usage: worktree-race-rescue-retired.sh <worktree> <target-ref> [<label>]
#        (run from a cwd OUTSIDE the worktree, the way worktree.sh calls it)

set -uo pipefail

# ===== BEGIN FROZEN BODY =====================================================
loom_worktree_has_live_process() {
    local worktree_path="$1"

    local worktree_real
    worktree_real="$(cd "$worktree_path" 2>/dev/null && pwd -P)" || {
        # Nothing to protect if the directory cannot even be entered; the
        # caller's own `git -C` will report the real problem. Mirrors
        # `worktree_in_use()`'s `!wt.exists() ⇒ empty`.
        return 1
    }

    local active_pids=""
    if [[ "$(uname -s 2>/dev/null)" == "Linux" && -d /proc/self ]]; then
        # Linux: /proc/<pid>/cwd symlinks. An unreadable entry (another
        # user's process, or one that exited mid-scan) is skipped, never an
        # error — same as the Rust walk.
        local proc_dir pid cwd
        for proc_dir in /proc/[0-9]*; do
            pid="${proc_dir#/proc/}"
            [[ "$pid" == "$$" || "$pid" == "${BASHPID:-}" ]] && continue
            cwd="$(readlink "$proc_dir/cwd" 2>/dev/null)" || continue
            if [[ "$cwd" == "$worktree_real" || "$cwd" == "$worktree_real"/* ]]; then
                active_pids+="$pid"$'\n'
            fi
        done
    elif command -v lsof >/dev/null 2>&1; then
        local lsof_output
        # Not gated on lsof's exit status (lsof exits 1 for "no matches",
        # and on macOS even for some genuine matches — #7488); the awk
        # parse below treats empty/garbled stdout as "no matches".
        lsof_output="$(lsof +D "$worktree_real" -F pf 2>/dev/null || true)"
        active_pids="$(printf '%s\n' "$lsof_output" | awk '/^p/{pid=substr($0,2)} /^fcwd/{print pid}' | grep -v -x "$$" || true)"
    else
        echo "loom_worktree_has_live_process: no process probe available on this host (no /proc, no lsof) -- cannot verify liveness of $worktree_path; contributing no evidence (matches worktree_in_use()'s unknown-is-empty contract)" >&2
        return 1
    fi

    [[ -n "${active_pids//[[:space:]]/}" ]]
}

# loom_worktree_reset_or_rescue <worktree_path> <target_ref> [<rescue_label>]
#
# Re-checks the worktree's commits-ahead and tracked-diff state immediately
# before the destructive reset; if either shows work that was not there at
# the caller's earlier staleness check, rescues or refuses instead of
# discarding it. Also refuses outright — before touching git at all — if a
# live process still has the worktree open (see
# `loom_worktree_has_live_process` above, #7463): git-level signals are a
# point-in-time snapshot, so a live writer is evidence more tracked edits
# may already be in flight that such a snapshot cannot see yet.
#
# Returns:
#   0  reset succeeded — the worktree was clean/stale, or its foreign
#      tracked changes were rescued to a patch file first
#   1  refused to reset — a live process still holds the worktree open, the
#      worktree gained real commits since the staleness check, or its
#      foreign tracked changes could not be captured to a patch file (reset
#      was NOT attempted in any of these cases; the worktree is unchanged
#      from before this call)
#   2  the reset itself failed (bad ref, git error) — any rescue that
#      happened above already succeeded; only the reset step failed
loom_worktree_reset_or_rescue() {
    local worktree_path="$1"
    local target_ref="$2"
    local rescue_label="${3:-loom-race-rescue}"

    if loom_worktree_has_live_process "$worktree_path"; then
        echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path — a live process still has it open (cwd inside the worktree); leaving it untouched instead of discarding its in-progress tracked edits" >&2
        return 1
    fi

    local ahead
    ahead="$(git -C "$worktree_path" rev-list --count "${target_ref}..HEAD" 2>/dev/null)" || ahead="0"
    if [[ "$ahead" != "0" ]]; then
        echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path to $target_ref — it gained $ahead commit(s) ahead since the staleness check; leaving it untouched instead of discarding them" >&2
        return 1
    fi

    # `git diff HEAD --quiet` exits 1 when tracked content differs from HEAD
    # (staged or unstaged), 0 when it does not, and >1 on a genuine git
    # error. This mirrors exactly what `git reset --hard` is about to
    # discard — untracked files are deliberately excluded (see header:
    # `reset --hard` never touches them, so there is nothing to rescue).
    local diff_check_status=0
    git -C "$worktree_path" diff HEAD --quiet 2>/dev/null || diff_check_status=$?

    if [[ "$diff_check_status" -gt 1 ]]; then
        echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path — could not determine its tracked-diff state against HEAD (git diff exit $diff_check_status)" >&2
        return 1
    fi

    if [[ "$diff_check_status" -eq 1 ]]; then
        local rescue_dir="$worktree_path/.snapshots"
        if ! mkdir -p "$rescue_dir" 2>/dev/null; then
            echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path — could not create rescue directory $rescue_dir for its foreign tracked changes" >&2
            return 1
        fi

        local patch_path
        patch_path="$rescue_dir/${rescue_label}-$(date -u +%Y%m%dT%H%M%SZ).patch"

        local write_status=0
        git -C "$worktree_path" diff HEAD > "$patch_path" 2>/dev/null || write_status=$?
        if [[ "$write_status" -ne 0 || ! -s "$patch_path" ]]; then
            rm -f "$patch_path" 2>/dev/null || true
            echo "loom_worktree_reset_or_rescue: refusing to reset $worktree_path — failed writing its foreign tracked changes to a rescue patch" >&2
            return 1
        fi

        echo "loom_worktree_reset_or_rescue: rescued foreign tracked changes in $worktree_path to $patch_path before resetting to $target_ref (replay with: git apply $patch_path)" >&2
    fi

    if git -C "$worktree_path" reset --hard "$target_ref" >/dev/null 2>&1; then
        return 0
    fi
    return 2
}
# ===== END FROZEN BODY =======================================================

loom_worktree_reset_or_rescue "$@"
exit $?
