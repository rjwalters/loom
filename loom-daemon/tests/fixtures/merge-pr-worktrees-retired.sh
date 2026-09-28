#!/usr/bin/env bash
# FROZEN COPY — do not "fix", do not sync with merge-pr.sh.
#
# The three `awk` porcelain parsers of `defaults/scripts/merge-pr.sh` exactly as
# they stood immediately before #8191 slice ported them to
# `loom-daemon/src/merge_pr/worktrees.rs`. `merge_pr_worktrees_differential.rs`
# feeds these and the Rust the same corpus and requires byte-identical output.
#
# WHY A COPY AND NOT THE LIVE SCRIPT
#
# The live functions now delegate to `loom-daemon`, so reading them from
# `merge-pr.sh` would compare the port against itself and pass unconditionally —
# a differential that measures nothing, which is the failure mode
# `defaults/docs/verification-recipes.md` §6 names. Reading them out of git
# history instead would pin this test to a moving ref. A frozen copy is the only
# form that keeps saying something true after the shell is gone.
#
# WHAT WAS CHANGED FROM THE ORIGINAL, AND WHY IT IS NOT A DIVERGENCE
#
# Only the input source. The originals ran
# `git -C "$REPO_ROOT" worktree list --porcelain 2>/dev/null | awk …`; these
# read that same text from stdin. The `git` invocation did NOT move in the port
# — it is still in `merge-pr.sh`, unchanged — so excluding it here is what makes
# the comparison exactly the parser and nothing else. The `awk` programs below
# are byte-for-byte the originals, including the `!found` guards added by #3671
# and the `substr($0, 10)` path parsing added by #3717.

# _worktree_branch_for <target_abs_path>   (porcelain on stdin)
# Prints the branch short-name attached to that worktree path.
retired_worktree_branch_for() {
  awk -v p="$1" '
      /^worktree / { wt=substr($0, 10); br=""; next }
      /^branch /   { br=$2 }
      /^$/         { if (wt == p && br != "" && !found) { sub(/^refs\/heads\//, "", br); print br; found=1; exit } }
      END          { if (wt == p && br != "" && !found) { sub(/^refs\/heads\//, "", br); print br } }
    '
}

# _primary_worktree_path                   (porcelain on stdin)
# Prints the FIRST `worktree` entry's path — the primary/main working copy.
retired_primary_worktree_path() {
  awk '/^worktree / { print substr($0, 10); exit }'
}

# _find_worktree_by_branch <branch>        (porcelain on stdin)
# Prints the worktree path with that branch checked out.
retired_find_worktree_by_branch() {
  awk -v want="refs/heads/${1}" '
      /^worktree / { wt=substr($0, 10); br=""; next }
      /^branch /   { br=$2 }
      /^$/         { if (br == want && !found) { print wt; found=1; exit } }
      END          { if (br == want && !found) { print wt } }
    '
}
