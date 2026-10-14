# merge-pr-discovered-worktree-retired.sh — the frozen, byte-for-byte copy of
# the discovery-fallback classification in merge-pr.sh's post-merge worktree
# cleanup (primary checkout / .loom-managed / user-owned, #4171/#3334), as it
# stood immediately before the #8191 slice that ported it to
# `loom-daemon merge-pr discovered-worktree`
# (loom-daemon/src/merge_pr/discovered_worktree.rs).
#
# The harness defines `info` / `warning` (printing `INFO<TAB>…` /
# `WARNING<TAB>…`), `_is_primary_worktree_path` (driven by $PRIMARY=true|false)
# and `_worktree_cleanup_decide` (printing `DECIDE`) BEFORE sourcing this file.
# Sourced, never executed. Wrapped in a function only so it can be called; the
# function header is the one edit to the frozen text.

_retired_discovered_worktree() {
  local DISCOVERED_WT="$1" PR_BRANCH="$2"
  # ---- frozen block (merge-pr.sh, pre-port) ----
        if _is_primary_worktree_path "$DISCOVERED_WT"; then
          info "PR branch '$PR_BRANCH' is checked out in the primary repository checkout ($DISCOVERED_WT) — not a removable worktree."
        elif [[ -f "$DISCOVERED_WT/.loom-managed" ]]; then
          _worktree_cleanup_decide discovered "$DISCOVERED_WT"
        else
          warning "Discovered worktree for branch '$PR_BRANCH' at: $DISCOVERED_WT"
          warning "Worktree lacks .loom-managed sentinel — not removing (user-owned)."
          warning "To clean it up, re-run with: --worktree-path '$DISCOVERED_WT'"
          warning "Or manually: git worktree remove '$DISCOVERED_WT'"
        fi
  # ---- end frozen block ----
}
