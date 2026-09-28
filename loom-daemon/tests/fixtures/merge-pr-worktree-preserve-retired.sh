# merge-pr-worktree-preserve-retired.sh — the frozen, byte-for-byte copies of
# the three near-identical #4186/#6694/#6264 remove-vs-preserve blocks inside
# merge-pr.sh's post-merge worktree cleanup (the default Loom-convention path,
# the porcelain discovery fallback, and the co-existing Judge/Doctor review
# worktree), as they stood immediately before the #8191 slice that
# consolidated them into `loom-daemon merge-pr worktree-preserve`
# (loom-daemon/src/merge_pr/worktree_preserve.rs).
#
# The harness defines `info`, `warning`, `_remove_loom_worktree` (records
# which path it was called with), `_issue_is_closed_for_cleanup` and
# `branch_has_landed` (stubbed to the fixed verdict the case under test wants)
# BEFORE sourcing this file, and sets ISSUE_NUM, PR_NUMBER, PR_BRANCH,
# REPO_ROOT, DEFAULT_BRANCH_NAME, PR_HEAD_SHA, BRANCH_LANDED_VERDICT,
# BRANCH_LANDED_EVIDENCE. Sourced, never executed.

_retired_default_path() {
  local DEFAULT_WT_PATH="$1"
  # ---- frozen block (merge-pr.sh, pre-port, default-path call site) ----
      if [[ -n "${ISSUE_NUM:-}" ]] && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"; then
        if branch_has_landed "$PR_BRANCH" "$DEFAULT_BRANCH_NAME" "$PR_HEAD_SHA"; then
          info "Issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER (partial-increment case, #3667), but branch '$PR_BRANCH' has already landed (${BRANCH_LANDED_EVIDENCE}) — its content is already on the default branch, so the worktree holds nothing unmerged; removing it (#6694)"
          _remove_loom_worktree "$DEFAULT_WT_PATH"
        else
          warning "Preserving worktree at $DEFAULT_WT_PATH — issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER, its live state is not CLOSED, and branch '$PR_BRANCH' has not landed (${BRANCH_LANDED_VERDICT}/${BRANCH_LANDED_EVIDENCE}) — it carries content the default branch does not have"
          info "This may be the partial-increment case (#3667) awaiting a future closing merge, or an issue-state lookup failure — cleanup retries automatically on a merge that closes #$ISSUE_NUM. If #$ISSUE_NUM is a programme issue designed never to close (#6694), that retry never fires: remove manually with 'git -C \"$REPO_ROOT\" worktree remove \"$DEFAULT_WT_PATH\" --force && git -C \"$REPO_ROOT\" branch -D $PR_BRANCH'"
        fi
      else
        _remove_loom_worktree "$DEFAULT_WT_PATH"
      fi
  # ---- end frozen block ----
}

_retired_discovered_path() {
  local DISCOVERED_WT="$1"
  # ---- frozen block (merge-pr.sh, pre-port, discovered-path call site) ----
          if [[ -n "${ISSUE_NUM:-}" ]] && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"; then
            if branch_has_landed "$PR_BRANCH" "$DEFAULT_BRANCH_NAME" "$PR_HEAD_SHA"; then
              info "Issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER (partial-increment case, #3667), but branch '$PR_BRANCH' has already landed (${BRANCH_LANDED_EVIDENCE}) — its content is already on the default branch, so the discovered worktree holds nothing unmerged; removing it (#6694)"
              _remove_loom_worktree "$DISCOVERED_WT"
            else
              warning "Preserving discovered worktree at $DISCOVERED_WT — issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER, its live state is not CLOSED, and branch '$PR_BRANCH' has not landed (${BRANCH_LANDED_VERDICT}/${BRANCH_LANDED_EVIDENCE}) — it carries content the default branch does not have"
              info "This may be the partial-increment case (#3667) awaiting a future closing merge, or an issue-state lookup failure — cleanup retries automatically on a merge that closes #$ISSUE_NUM. If #$ISSUE_NUM is a programme issue designed never to close (#6694), that retry never fires: remove manually with 'git -C \"$REPO_ROOT\" worktree remove \"$DISCOVERED_WT\" --force && git -C \"$REPO_ROOT\" branch -D $PR_BRANCH'"
            fi
          else
            info "Discovered Loom-managed worktree at non-standard path: $DISCOVERED_WT"
            _remove_loom_worktree "$DISCOVERED_WT"
          fi
  # ---- end frozen block ----
}

_retired_judge_pr_path() {
  local JUDGE_PR_WT_PATH="$1"
  # ---- frozen block (merge-pr.sh, pre-port, judge-pr call site) ----
      if [[ -n "${ISSUE_NUM:-}" ]] && ! _issue_is_closed_for_cleanup "$ISSUE_NUM"; then
        if branch_has_landed "$PR_BRANCH" "$DEFAULT_BRANCH_NAME" "$PR_HEAD_SHA"; then
          info "Issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER (partial-increment case, #3667), but branch '$PR_BRANCH' has already landed (${BRANCH_LANDED_EVIDENCE}) — its content is already on the default branch, so the Judge/Doctor review worktree holds nothing unmerged; removing it (#6694)"
          _remove_loom_worktree "$JUDGE_PR_WT_PATH"
        else
          warning "Preserving Judge/Doctor review worktree at $JUDGE_PR_WT_PATH — issue #$ISSUE_NUM is not a close target of PR #$PR_NUMBER, its live state is not CLOSED, and branch '$PR_BRANCH' has not landed (${BRANCH_LANDED_VERDICT}/${BRANCH_LANDED_EVIDENCE}) — it carries content the default branch does not have"
          info "This may be the partial-increment case (#3667) awaiting a future closing merge, or an issue-state lookup failure — cleanup retries automatically on a merge that closes #$ISSUE_NUM. If #$ISSUE_NUM is a programme issue designed never to close (#6694), that retry never fires: remove manually with 'git -C \"$REPO_ROOT\" worktree remove \"$JUDGE_PR_WT_PATH\" --force && git -C \"$REPO_ROOT\" branch -D $PR_BRANCH'"
        fi
      else
        info "Found co-existing Judge/Doctor review worktree at $JUDGE_PR_WT_PATH (PR #$PR_NUMBER, alongside issue-$ISSUE_NUM handling above) — removing (#6264)"
        _remove_loom_worktree "$JUDGE_PR_WT_PATH"
      fi
  # ---- end frozen block ----
}
