#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's post-merge worktree-cleanup TARGET PLAN — the
# `WT_ROOT_DIR` / `ISSUE_NUM` / `DEFAULT_WT_PATH` / `JUDGE_PR_WT_PATH`
# assignments and the `^feature/issue-([0-9]+)$` branch classification around
# them — as they stood immediately before #8191 ported the decision to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_cleanup_paths_differential.rs` can keep comparing
# the Rust against the exact implementation it replaced, forever, rather than
# only at the moment of the port. Reading it out of the live merge-pr.sh stopped
# being possible the instant that block started delegating.
#
# Unlike the other frozen fixtures in this directory, the retired code was NOT a
# function — it was inline top-level statements inside the
# `if [[ "$CLEANUP_WORKTREE" == "true" ]]` block. It is wrapped in
# `_cleanup_paths_retired` here so the harness can call it, and the ONLY other
# edit is the trailing `printf` that publishes the four values the harness
# compares. The statements themselves, their order, and their comments are
# verbatim; `$REPO_ROOT`, `$PR_BRANCH`, `$PR_NUMBER` and `loom_worktree_root`
# are the same names the live script had in scope.
#
# `loom_worktree_root` is NOT copied: the harness sources the real
# `defaults/scripts/lib/worktree-root.sh`, so the shell side keeps resolving the
# root through the very helper this port retires from merge-pr.sh — otherwise
# the comparison would be against a paraphrase of it.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
_cleanup_paths_retired() {
    # Strict pattern: only `feature/issue-<N>` matches. Trailing-number
    # heuristics would misclassify branches like `release-1`.
    # Resolve the worktree base through the shared helper so an overridden
    # root (#3530) is discovered here; defaults to $REPO_ROOT/.loom/worktrees.
    WT_ROOT_DIR="$(loom_worktree_root "$REPO_ROOT")"
    DEFAULT_WT_PATH=""
    JUDGE_PR_WT_PATH=""
    if [[ "$PR_BRANCH" =~ ^feature/issue-([0-9]+)$ ]]; then
      ISSUE_NUM="${BASH_REMATCH[1]}"
      DEFAULT_WT_PATH="$WT_ROOT_DIR/issue-$ISSUE_NUM"
      # #6264: a Judge (or Doctor) review of this same ordinary Loom-issue PR
      # may ALSO have created a co-existing pr-$PR_NUMBER worktree via
      # pr-worktree.sh — checked and removed independently below, alongside
      # (not instead of) the issue-$ISSUE_NUM path above.
      JUDGE_PR_WT_PATH="$WT_ROOT_DIR/pr-$PR_NUMBER"
    else
      # External-fork / ad-hoc branch — the doctor would have used a
      # `pr-<PR_NUMBER>` worktree if any.
      DEFAULT_WT_PATH="$WT_ROOT_DIR/pr-$PR_NUMBER"
    fi

    # Harness-only: publish what the retired block left in scope, in the same
    # field order the ported verb prints.
    printf 'LOOM-CLEANUP-PATHS\t%s\t%s\t%s\n' \
        "${ISSUE_NUM:-}" "$DEFAULT_WT_PATH" "$JUDGE_PR_WT_PATH"
}
