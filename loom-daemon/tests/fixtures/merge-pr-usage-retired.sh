#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `show_help` as it stood immediately before
# #8191's slice moved its text to `loom-daemon merge-pr usage`
# (loom-daemon/src/merge_pr/usage.rs + usage.txt).
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# WHAT IS VERBATIM, AND WHAT IS NOT
#
# Per defaults/docs/verification-recipes.md §6 ("say explicitly WHICH
# implementation it models"):
#
#   * VERBATIM, character for character: the whole `show_help` function —
#     its name, its four-space indent, the UNQUOTED `cat << EOF` heredoc and
#     every body line, copied from merge-pr.sh at the parent of this slice's
#     commit.
#   * RECONSTRUCTED: nothing. Only this header was added. The retired caller
#     (`if [[ $1 == --help || $1 == -h ]]; then show_help; exit 0; fi`) is
#     not copied: tests/merge_pr_usage_differential.rs calls `show_help`
#     directly and compares its stdout with the port's.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
# Function to show help
show_help() {
    cat << EOF
Loom PR Merge - Worktree-safe merge using forge API (GitHub or Gitea)

Usage: ./.loom/scripts/merge-pr.sh <pr-number> [options]

Merges a PR via the forge API (not 'gh pr merge') to avoid
"already used by worktree" errors when merging from inside a worktree.

Supports both GitHub and Gitea forges. Forge detection is automatic
(see forge-helpers.sh for details).

Options:
  --no-cleanup-worktree  Skip local worktree AND local branch cleanup
                         after merge
  --cleanup-worktree     (no-op, worktree cleanup is now the default)
  --worktree-path <dir>  Explicit worktree path to clean up. Bypasses the
                         .loom-managed sentinel guard (caller asserts
                         responsibility — this is the documented opt-in
                         for removing non-Loom worktrees). Also deletes
                         the matching local branch via 'git branch -d'
                         (Git refuses on unmerged commits).
  --dry-run              Show what would happen without merging
  --auto                 Wait (bounded) for this head's checks to settle, then
                         merge in THIS process — immediately if they already
                         are. NEVER arms the forge's server-side auto-merge
                         queue, which re-reads neither the loom:pr label nor
                         the non-required test suites once armed (#8410).
  --allow-stacked-children
                         Bypass the pre-merge merge-ordering guard's remaining
                         hard-block path. By default (#7982) the guard pins
                         the parent's tip to refs/loom/parent/<branch> and
                         WARNS instead of blocking when open stacked CHILD PRs
                         target the parent branch (feature/issue-N) — see
                         #3747 item 2. It still hard-blocks only when the tip
                         could not be pinned; this flag skips past that.
                         Operator asserts responsibility, mirroring
                         --worktree-path.
  --allow-red-tree       Proceed past a failing repo-declared merge.treeChecks
                         gate (checks run on base + PR head, #10026). Warns and
                         records an audit PR comment. Operator asserts
                         responsibility.
  --allow-unapproved     Bypass the pre-merge loom:pr review-signal guard.
                         By default the script refuses to merge (exit 1) a
                         PR whose current head does not carry the loom:pr
                         label — the only forge-visible signal Judge
                         reviewed that head (it may have been cleared by a
                         staleness guard, e.g. after a Doctor rebase). This
                         flag bypasses that block; the operator asserts
                         responsibility, mirroring --allow-stacked-children.
                         The bypass is always logged as a warning and, on a
                         real (non-dry-run) merge, best-effort recorded as a
                         PR comment audit trail too.
  --redate-stale-checks  On an #8248 freshness block, re-run the stale checks in
                         place and merge once fresh (#8914), else push a tree-
                         identical no-op commit and exit 4 (#8508) — never a
                         bypass; a repeat push block escalates to loom:operator.
  --merge-method M       Request squash|merge|rebase instead of auto-detect; validated via loom-daemon against the repo's actually-allowed strategies — fails rather than silently falling back to squash if disallowed (#8845).
  --no-cleanup-primary   Skip automatic primary-checkout branch cleanup (#5015).
                         When the merged branch is checked out in the PRIMARY
                         repo checkout (not a worktree), the script normally
                         auto-checks-out the default branch and force-deletes
                         it there ONLY when provably safe (clean tree, no
                         stash entries, tip matches the merged PR head SHA).
                         Pass this to always print manual instructions instead.
  --cleanup-primary      (no-op, primary-checkout cleanup is the default)
  -h, --help             Show this help and exit

By default, the local worktree AND the local branch it held are cleaned up
after a successful merge (#4100). Pass --no-cleanup-worktree to skip both
(e.g., when other terminals may have their CWD inside the worktree, or you
want to keep a branch with unpushed commits).

Cleanup is restricted to Loom-managed worktrees (those under
.loom/worktrees/issue-N that contain a .loom-managed sentinel file written
by worktree.sh). User-provisioned worktrees at other paths are never
removed by the default code path. Set LOOM_PRESERVE_WORKTREE=1 to disable
cleanup unconditionally for a session.

Local branch deletion (#4100): every cleanup path — including the case
where no worktree exists at all — attempts to delete the merged PR's local
branch. Safety is determined by comparing the local branch tip to the
merged PR's head SHA (not 'git branch --merged', which is always false for
a squash merge): a matching tip uses 'git branch -D'; a non-matching tip
(unpushed local work) falls back to 'git branch -d', which keeps the
branch and reports it instead of force-deleting. The branch currently
checked out (main worktree or any other) is never deleted, and the repo's
default branch is never a delete target.

Primary-checkout auto-cleanup (#5015): the one exception to "checked out
branches are never deleted" is when the branch is checked out in the repo's
PRIMARY checkout specifically (not a linked worktree) AND it is provably
safe — the tip-matches-head safety check above passed, the primary
checkout's working tree is clean, and it has no stash entries. In that case
the script checks out the default branch there and force-deletes the merged
branch automatically instead of just printing instructions. Pass
--no-cleanup-primary to always print the manual instructions instead.

When --worktree-path <dir> is passed explicitly, the operator is taking
responsibility for the cleanup decision: the sentinel guard is bypassed
for that one path. The path is validated against 'git worktree list'
and rejected if it is not a worktree of this repository.

Discovery fallback: if neither .loom/worktrees/issue-N/ nor
.loom/worktrees/pr-<PR_NUMBER>/ exists, the script walks
'git worktree list --porcelain' looking for a worktree whose branch
matches the merged PR head branch. It NEVER auto-removes a discovered
user-owned worktree; it only logs the path and suggests re-running with
--worktree-path <found-path>.

Precedence (highest wins):
  1. LOOM_PRESERVE_WORKTREE=1     (always skip cleanup)
  2. --no-cleanup-worktree        (always skip cleanup; warns if combined
                                  with --worktree-path)
  3. --worktree-path <dir>        (explicit path; bypasses sentinel)
  4. default: .loom/worktrees/issue-N or pr-N + sentinel guard

Exit codes:
  0 = merged (or --help) · 1 = failed
  3 = PR head moved past the SHA this attempt gated on (#5579) · 5 = --auto's bounded settle-wait expired before CI finished (#8896), or no concluded CI run for this exact head yet (#10567) — neither is a failure; retry later
  4 = stale required checks re-running in place (#8914) or re-dated by a push (#8508) under --redate-stale-checks — not a failure; retry later
  6 = deferred behind another PR's chain-head merge lock (#10167; --auto or LOOM_CHAIN_LOCK_GUARD=1; LOOM_CHAIN_LOCK_OVERRIDE=1 bypasses) — nothing written; retry later

Examples:
  ./.loom/scripts/merge-pr.sh 123
    Merges PR #123 (squash), deletes remote branch, cleans up worktree

  ./.loom/scripts/merge-pr.sh 123 --dry-run
    Shows what would happen without merging

  ./.loom/scripts/merge-pr.sh 123 --auto
    Waits (bounded by LOOM_AUTO_MERGE_TIMEOUT) for PR #123's checks to
    settle, then merges it here — never via a server-side merge queue

  ./.loom/scripts/merge-pr.sh 123 --no-cleanup-worktree
    Merges PR but leaves the local worktree in place

  ./.loom/scripts/merge-pr.sh 123 --worktree-path ../adhoc-wt
    Merges PR #123 and removes the worktree at ../adhoc-wt plus its
    matching local branch (bypasses the .loom-managed sentinel guard).

  ./.loom/scripts/merge-pr.sh 123 --no-cleanup-primary
    Merges PR but always prints manual instructions instead of
    auto-cleaning a branch checked out in the primary repo checkout.

  ./.loom/scripts/merge-pr.sh 123 --allow-unapproved
    Merges PR #123 even though it does not carry loom:pr (no forge-visible
    Judge review signal for the current head). Logs a warning and posts a
    PR comment recording the override.
EOF
}
