#!/usr/bin/env bash
# merge-pr-revalidate-retired.sh — the frozen, byte-for-byte copy of
# merge-pr.sh's `_revalidate_merge_guards` (--auto's post-wait re-validation,
# #8410/#8896) as it stood immediately before the #8191 slice that moved its
# reads and decisions to `loom-daemon merge-pr revalidate`
# (loom-daemon/src/merge_pr/revalidate.rs).
#
# The function below is untouched, comments included. The harness
# (tests/merge_pr_revalidate_differential.rs) supplies the names it calls —
# forge_get_pr_nocache, error, error_head_moved, _check_loom_pr_label and
# _check_verdict_label_contradiction — as recording stubs, and runs it under
# merge-pr.sh's own `set -euo pipefail`, because what that option did to a
# `jq` failure inside this body is part of the behaviour being compared.
# Sourced, never executed.
# shellcheck disable=SC2034  # PR_LABELS is read by the stubbed _check_loom_pr_label
_revalidate_merge_guards() {
  local fresh fresh_sha
  fresh="$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')"
  # Merged underneath us while we waited — nothing left to guard.
  [[ "$(echo "$fresh" | jq -r '.merged // false')" == "true" ]] && return 0

  # Head moved during the wait (PR #8220's 07:12 force-push). The approval and
  # the check results this run validated describe a tree that is no longer the
  # head, so this is the #5579 "re-queue, not a failure" signal (exit 3), not a
  # merge we should complete against the new tree.
  # #8896: an unusable re-read (the `|| echo '{}'` fallback above, or any
  # payload with no head SHA in it) must SAY that. It used to fall through to
  # the loom:pr guard, which reported the genuine-absence wording ("does not
  # carry the `loom:pr` label") — failing closed, correctly, but sending the
  # operator to re-review a PR whose approval was never actually read. Nothing
  # about the verdict changes here: an unreadable response is evidence neither
  # that loom:pr is present nor that it is absent, so the merge still refuses.
  fresh_sha="$(echo "$fresh" | jq -r '.head.sha // empty')"; [[ -n "$fresh_sha" ]] || error "Merge blocked: could not re-read PR #$PR_NUMBER after --auto's settle-wait — the uncached re-read returned no usable payload (no head SHA), so neither the head nor the label set could be re-validated against current state. This is a forge read failure, NOT a missing \`loom:pr\` label: refusing to merge rather than treating an unreadable response as a verdict. Re-run once the forge API is healthy."
  if [[ -n "$fresh_sha" && -n "$MERGE_PRECONDITION_SHA" && "$fresh_sha" != "$MERGE_PRECONDITION_SHA" ]]; then
    error_head_moved "PR #$PR_NUMBER: head moved while --auto waited for this head's checks to settle (#8410)" \
      "$MERGE_PRECONDITION_SHA" "$fresh_sha"
  fi

  # Re-point the guards' input at the state just read, then re-run them. A
  # loom:verdict-stale revocation (#5686), a Judge re-claim, or a contradicting
  # verdict label (#8112) now blocks the merge exactly as it would have at
  # queue time. --allow-unapproved still overrides the loom:pr block, as it
  # does on the queue-time evaluation.
  PR_LABELS="$(echo "$fresh" | jq -r '.labels[]?.name // empty' 2>/dev/null || true)"
  _check_loom_pr_label
  _check_verdict_label_contradiction
}
