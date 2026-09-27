#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_strip_one_closed_issue_building_label` (#6199)
# as it stood immediately before #8191 ported its decision to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_closed_building_differential.rs` can keep
# comparing the Rust against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the function out of the
# live merge-pr.sh stopped being possible the instant it started delegating.
#
# The function is copied WHOLE and VERBATIM — including the forge mutation and
# log text that stayed in the shell — so the harness, not an edit here, decides
# what is observed: it defines `gh`, `success`/`warning` and
# `forge_gh_remove_label_rl_safe` as recording stubs before calling it.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
_strip_one_closed_issue_building_label() {
  local issue_num="$1"
  local issue_json issue_state issue_labels

  # Fresh (uncached) read, mirroring _reset_one_partial_issue's freshness
  # discipline: we need the label/state AS OF right now, not as of PR
  # creation or the GraphQL closingIssuesReferences snapshot.
  issue_json="$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"

  # A PR is also an "issue" on this endpoint (has a .pull_request member).
  if [[ "$(echo "$issue_json" | jq -r 'has("pull_request")')" == "true" ]]; then
    return 0
  fi

  issue_state="$(echo "$issue_json" | jq -r '.state // ""')"
  if [[ "$issue_state" != "closed" ]]; then
    # Not (or no longer) closed — either a #4569 revert just reopened it, the
    # forge's close hadn't landed yet when we read it, or it was never
    # actually closed. Leave the label; a later merge or the standalone
    # cleanup script will catch it once it genuinely closes.
    return 0
  fi

  issue_labels="$(echo "$issue_json" | jq -r '.labels[]?.name' 2>/dev/null || true)"
  if ! printf '%s\n' "$issue_labels" | grep -qx 'loom:building'; then
    return 0
  fi

  if forge_gh_remove_label_rl_safe "$REPO_NWO" "$issue_num" "loom:building" 2>/dev/null; then
    success "Issue #$issue_num: removed stale loom:building label (closed by this merge, #6199)"
  else
    warning "Could not remove loom:building from closed issue #$issue_num — may need manual: gh issue edit $issue_num --repo $REPO_NWO --remove-label loom:building"
  fi
}
