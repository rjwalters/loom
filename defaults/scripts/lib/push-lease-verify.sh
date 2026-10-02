#!/usr/bin/env bash
# push-lease-verify.sh - Make `git push --force-with-lease` tell the truth in
# both directions: it can report a rejection for an update that landed (#6695,
# `push_landed_despite_rejection` below) and it can report success for an
# update that destroyed someone else's commit (#9487, `push_lease_*` below).
#
# ============================================================================
# HALF 2 (#9487): the lease must be PINNED, because the implicit one is a
# shared, locally-mutable ref
# ============================================================================
#
# `git push --force-with-lease` with no `=<ref>:<expect>` value compares the
# remote head against the LOCAL remote-tracking ref `refs/remotes/<remote>/
# <branch>`. In a Loom clone that ref is **shared by every linked worktree**
# (`.loom/worktrees/issue-N` all point at one object store and one set of
# remote-tracking refs), exactly like `refs/stash` is shared (#4821/#5754).
#
# So a sibling agent that pushes and then fetches — or merely fetches —
# fast-forwards *your* lease value to the commit *they* just published. The
# bare lease is then satisfied by construction and your push is accepted,
# silently deleting their commit. It compares against a ref somebody else
# updated, not against truth. This happened live on PR #9483 (2026-09-29):
# two Doctors on one PR, the second's bare-lease push overwrote the first's
# already-Judge-approved commit, with no error and no conflict signal.
#
# The fix is to pin the expected value to the remote head **the pushing work
# is actually based on** — captured BEFORE the work/rebase, never re-read just
# before the push (re-reading is how the stale-ref race gets laundered into a
# "fresh" value: a fetch immediately before the push pins the sibling's commit
# and the clobber proceeds):
#
#   source ".../lib/push-lease-verify.sh"
#   basis="$(push_lease_live_tip origin "$branch")" || exit 1   # before working
#   ... rebase / amend / commit ...
#   push_lease_require_incorporated "$basis" "$branch" || exit 1
#   git push "$(push_lease_pin_flag "$branch" "$basis")" origin "$branch"
#
# Pinning also fails CLOSED: an expected value git cannot resolve locally (a
# sibling's commit we never fetched) makes the push rejected, not accepted —
# the opposite of the bare flag's behaviour in the same situation.
#
# ============================================================================
# HALF 1 (#6695): a reported rejection is not always a real one
# ============================================================================
#
# Background: Git LFS's pre-push hook can race the lease re-check on a
# branch with pending LFS objects. The hook uploads LFS objects and the ref
# update proceeds on the remote, while the client-side lease comparison
# that produced the printed rejection
#
#   ! [rejected]  <branch> -> <branch> (stale info)
#   error: failed to push some refs to '...'
#   remote rejected ... is at <new-sha> but expected <old-sha>
#
# was evaluated against a different (already-stale) view. The net effect
# observed live: `git push --force-with-lease` prints a rejection and exits
# non-zero, yet the ref update actually landed — confirmed via
# `git ls-remote` and the `origin/<branch>` reflog. Both occurrences were on
# branches with LFS objects to upload; a push on a branch with no LFS
# objects did not exhibit it.
#
# A caller that trusts the exit status / stderr text alone will wrongly
# conclude the push failed, and any of the "safe" responses it might take —
# retry the push, re-rebase, report failure upstream — are wrong against a
# ref that already moved.
#
# Usage:
#   source ".../lib/push-lease-verify.sh"
#   if ! run git push --force-with-lease; then
#       if push_landed_despite_rejection origin "$branch" "$expected_sha"; then
#           warn "PUSH-LEASE-RACE-DETECTED: ..."
#       else
#           err "... genuinely rejected ..."
#       fi
#   fi
#
# Optionally pass a `git -C <dir>`-style command prefix as trailing args when
# the push ran against a branch checked out in a worktree other than the
# caller's own cwd:
#   push_landed_despite_rejection origin "$branch" "$expected_sha" git -C "$worktree"

# push_landed_despite_rejection <remote> <branch> <expected-local-sha> [git-cmd...]
#
# Queries the LIVE remote ref (never a local remote-tracking ref, which can
# be stale) for <branch> and compares it against <expected-local-sha> — the
# sha the failed push was trying to publish. Returns 0 (landed despite the
# reported rejection) when they match, 1 (genuinely rejected, or the remote
# state could not be determined) otherwise.
push_landed_despite_rejection() {
    local remote="$1" branch="$2" expected_sha="$3"
    shift 3
    local -a git_cmd=("$@")
    if [[ ${#git_cmd[@]} -eq 0 ]]; then
        git_cmd=(git)
    fi

    [[ -n "$expected_sha" ]] || return 1

    local remote_sha
    remote_sha="$("${git_cmd[@]}" ls-remote "$remote" "refs/heads/$branch" 2>/dev/null | cut -f1)"

    [[ -n "$remote_sha" && "$remote_sha" == "$expected_sha" ]]
}

# push_lease_live_tip <remote> <branch> [git-cmd...]
#
# Print the LIVE remote head of <branch> — `git ls-remote`, never the local
# remote-tracking ref, which a sibling worktree's fetch can advance (#9487).
# Call this at the point the caller READS the branch state it is about to
# rewrite (before the rebase/amend), and keep the value: it is the lease pin.
#
# Exit 0 + a sha      -> the branch exists on the remote at that sha.
# Exit 0 + no output  -> the remote answered and the branch does not exist.
# Exit 1              -> the remote could not be queried; the caller must NOT
#                        fall back to the bare flag (that is the bug).
push_lease_live_tip() {
    local remote="$1" branch="$2"
    shift 2
    local -a git_cmd=("$@")
    if [[ ${#git_cmd[@]} -eq 0 ]]; then
        git_cmd=(git)
    fi

    local out
    out="$("${git_cmd[@]}" ls-remote "$remote" "refs/heads/$branch" 2>/dev/null)" || return 1
    printf '%s' "$out" | awk 'NR==1 { print $1 }'
}

# push_lease_pin_flag <branch> <expected-oid>
#
# Print the pinned `--force-with-lease=<branch>:<expected-oid>` argument.
#
# Refuses (exit 1, nothing printed) on an empty <expected-oid>: there is
# deliberately NO bare-flag fallback here, because the bare flag is precisely
# the unsafe form this helper exists to replace. A caller without a pin must
# fetch and re-derive one, or refuse to push.
push_lease_pin_flag() {
    local branch="$1" expected_oid="$2"
    [[ -n "$branch" && -n "$expected_oid" ]] || return 1
    printf '%s' "--force-with-lease=$branch:$expected_oid"
}

# push_lease_require_incorporated <pinned-oid> <local-ref> [git-cmd...]
#
# Assert this checkout has INCORPORATED the remote head it is about to
# replace: <pinned-oid> must be <local-ref> itself or an ancestor of it.
#
# The pinned lease stops a push that would overwrite a commit published after
# the pin was taken. This covers the other direction — a commit published
# BEFORE the pin was taken that this checkout never merged/rebased onto (so
# the pin is accurate and the push would still delete it). An unresolvable
# <pinned-oid> (a sibling's commit never fetched here) also fails, which is
# the correct answer: an object we do not have is one we have not incorporated.
#
# Exit 0 = safe to publish, exit 1 = refuse.
push_lease_require_incorporated() {
    local pinned_oid="$1" local_ref="$2"
    shift 2
    local -a git_cmd=("$@")
    if [[ ${#git_cmd[@]} -eq 0 ]]; then
        git_cmd=(git)
    fi

    [[ -n "$pinned_oid" ]] || return 1
    # A branch that does not exist on the remote yet has nothing to lose.
    [[ "$pinned_oid" != "0000000000000000000000000000000000000000" ]] || return 0

    "${git_cmd[@]}" merge-base --is-ancestor "$pinned_oid" "$local_ref" 2>/dev/null
}
