#!/usr/bin/env bash
# FROZEN COPY of the removal step at the end of merge-pr.sh's
# `_remove_loom_worktree` — #6372's `git worktree remove --force`, its single
# `git worktree prune` + retry, and the success / failure report — as it stood
# immediately before #8191 ported it to `loom-daemon merge-pr worktree-teardown`.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_worktree_teardown_differential.rs` can keep
# comparing the port against the exact implementation it replaced, forever,
# rather than only at the moment of the port.
#
# Verbatim: every line from `local remove_err=...` through the closing `fi` of
# the `if [[ "$removed" == "true" ]]` block, with the success arm's NON-removal
# steps (the ledger write, the "shell was inside it" hint, the --worktree-path
# branch delete and the cargo target reclaim) — which stayed in merge-pr.sh and
# are not this verb's — replaced by ONE added line, `echo LOOM-RETIRED-REMOVED`,
# so the harness can see which arm ran. Wrapped in a function, with
# `worktree_path` as its argument, only so it can be called. Nothing else is
# edited.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change belonging in its own issue; leave this file alone
# and update the test's expectation with a comment saying why.

_retired_worktree_teardown() {
  local worktree_path="$1"
  local remove_err="" removed=false pruned=false
  if remove_err="$(git -C "$REPO_ROOT" worktree remove "$worktree_path" --force 2>&1)"; then
    removed=true
  elif git -C "$REPO_ROOT" worktree prune >/dev/null 2>&1; then
    pruned=true
    if remove_err="$(git -C "$REPO_ROOT" worktree remove "$worktree_path" --force 2>&1)"; then
      removed=true
    fi
  fi

  if [[ "$removed" == "true" ]]; then
    if [[ "$pruned" == "true" ]]; then
      success "Worktree removed (after pruning a stale worktree registration)"
    else
      success "Worktree removed"
    fi
    echo LOOM-RETIRED-REMOVED
  else
    # Best-effort by design (#6372): the merge itself already succeeded and is
    # unaffected by cleanup failing, so this stays a warning rather than an
    # error() (which would exit 1 and misreport the merge as failed). But
    # unlike a bare "could not remove" with no context, name the actual git
    # failure and give an explicit remediation — matching the quality of the
    # existing partial-increment message elsewhere in this function.
    warning "Could not remove worktree at $worktree_path (best-effort cleanup — the merge itself already succeeded and is unaffected):"
    warning "$remove_err"
    warning "Remediation: git worktree prune && git -C \"$REPO_ROOT\" worktree remove \"$worktree_path\" --force"
    warning "If that still fails: rm -rf \"$worktree_path\" && git -C \"$REPO_ROOT\" worktree prune"
  fi
}
