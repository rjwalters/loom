#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_reset_one_partial_issue` (#3667 / #4569) as it
# stood immediately before #8191 ported its decision to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_partial_reset_differential.rs` can keep
# comparing the Rust against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the function out of the
# live merge-pr.sh stopped being possible the instant it started delegating.
#
# The function is copied WHOLE and VERBATIM — including the forge mutations and
# comment text that stayed in the shell — so the harness, not an edit here,
# decides what is observed: it defines `gh`, `info`/`warning`/`success`, the
# `forge_gh_*_rl_safe` wrappers, `_post_premature_close_comment` and the two
# `_partial_ref_*` membership predicates as recording stubs before calling it.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
_reset_one_partial_issue() {
  local issue_num="$1"
  local issue_json issue_state issue_labels reopened=false

  # Fresh (uncached) read so we see the label state AS OF the merge, not as of
  # PR creation. Plain `gh api` is uncached; use it directly (not $GH, which may
  # be gh-cached) to avoid a stale cached view masking a fresh re-claim.
  issue_json="$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"

  # The GitHub issues endpoint also returns PRs (a PR is an issue with a
  # .pull_request member). Never mutate a PR that slipped through the regex.
  if [[ "$(echo "$issue_json" | jq -r 'has("pull_request")')" == "true" ]]; then
    return 0
  fi

  issue_state="$(echo "$issue_json" | jq -r '.state // ""')"
  if [[ "$issue_state" != "open" ]]; then
    # #4569: a partial-increment issue that was OPEN pre-merge and is closed now
    # was closed BY this merge. If the pre-merge guard recorded a closing
    # reference to it from this very PR (a stray `close #N` in prose, or a
    # Development-sidebar link), that close contradicts the PR's own declared
    # `Part of` / `Contributes to` intent — revert it, then fall through to the
    # normal label swap so the issue re-enters the ready queue.
    if _partial_ref_is_conflicted "$issue_num"; then
      warning "Partial-increment reset: issue #$issue_num was auto-closed by PR #$PR_NUMBER's merge despite its non-closing \`Part of\`/\`Contributes to\` reference (a closing reference to #$issue_num was detected pre-merge) — reopening (#4569)"
      # forge_gh_reopen_issue_rl_safe (#4856): falls back to a REST PATCH
      # (state=open) when `gh issue reopen`'s GraphQL mutation is rate-limited.
      if forge_gh_reopen_issue_rl_safe "$REPO_NWO" "$issue_num" 2>/dev/null; then
        success "Issue #$issue_num reopened (premature auto-close reverted)"
        reopened=true
        _post_premature_close_comment "$issue_num"
      else
        warning "Could not reopen issue #$issue_num after its premature auto-close — reopen manually: gh issue reopen $issue_num --repo $REPO_NWO"
        return 0
      fi
    elif _partial_ref_was_open_before_merge "$issue_num"; then
      # Open before the merge, closed after it, but this PR carries no closing
      # reference we can attribute it to. Could be a deliberate close by a human
      # or another agent in the same window, so do NOT revert it — just make the
      # coincidence loud enough to investigate.
      warning "Partial-increment reset: issue #$issue_num was open before PR #$PR_NUMBER merged and is now closed (state='${issue_state:-unknown}'), but no closing reference to it was detected on this PR — NOT reopening automatically (it may be a deliberate close). If this was a premature auto-close, reopen it with: gh issue reopen $issue_num --repo $REPO_NWO"
      return 0
    else
      info "Partial-increment reset: issue #$issue_num is not open (state='${issue_state:-unknown}') — skipping"
      return 0
    fi
  fi

  issue_labels="$(echo "$issue_json" | jq -r '.labels[]?.name' 2>/dev/null || true)"
  if ! printf '%s\n' "$issue_labels" | grep -qx 'loom:building'; then
    info "Partial-increment reset: issue #$issue_num is not loom:building — skipping (idempotent)"
    return 0
  fi

  info "Partial-increment reset: PR #$PR_NUMBER merged as a partial slice of #$issue_num; returning it to the ready queue"
  # forge_gh_swap_label_rl_safe (#4856): falls back to REST (DELETE the old
  # label, POST the new one) when `gh issue edit`'s GraphQL mutation is
  # rate-limited, rather than silently dropping the label swap.
  if forge_gh_swap_label_rl_safe "$REPO_NWO" "$issue_num" "loom:building" "loom:issue" 2>/dev/null; then
    success "Issue #$issue_num: loom:building -> loom:issue (partial increment; issue remains open)"
    local ts comment reopen_note=""
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    [[ "$reopened" == "true" ]] && reopen_note="
- **Reopened** this issue (GitHub had auto-closed it from a stray closing keyword in PR #$PR_NUMBER's body or one of its commit messages — see #4569)"
    comment="## Partial Increment Merged

PR #$PR_NUMBER merged with a non-closing \`Part of\` / \`Contributes to\` reference, so this issue remains **open** for further work.

**Action taken**:$reopen_note
- Removed \`loom:building\` label
- Added \`loom:issue\` label to return to the ready queue

This issue is now available for the next increment (a subsequent \`/loom:sweep\` will treat it as ready rather than in-flight).

---
*Reset by merge-pr.sh (#3667) at $ts*"
    # forge_gh_comment_rl_safe (#4856): falls back to the REST comments
    # endpoint on a GraphQL rate-limit rejection.
    forge_gh_comment_rl_safe "$REPO_NWO" "$issue_num" "$comment" 2>/dev/null || \
      warning "Could not post partial-increment comment on issue #$issue_num (label swap still applied)"
  else
    warning "Could not reset labels on issue #$issue_num (partial increment) — may need manual 'gh issue edit'"
  fi
}
