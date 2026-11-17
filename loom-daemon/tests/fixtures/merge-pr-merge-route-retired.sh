# merge-pr-merge-route-retired.sh — the frozen copy of merge-pr.sh's
# synchronous merge-retry loop (`MAX_MERGE_RETRIES=…` through its `done`), as
# it stood immediately before the #8191 slice that ported its per-attempt route
# (405 merge-in-progress, stale-base sync-and-retry, terminal refusals) to
# `loom-daemon merge-pr merge-route` (loom-daemon/src/merge_pr/merge_route.rs).
#
# Sourced, never executed. The loop text below is VERBATIM apart from a
# two-space re-indent; the only edit is the wrapping function, so the differential
# (loom-daemon/tests/merge_pr_merge_route_differential.rs) can drive it with
# the same recording stubs (forge_merge_pr, forge_get_pr_nocache,
# forge_update_branch, sleep, info/success/warning/error, ...) it gives the
# live loop extracted from merge-pr.sh, and compare the two traces.
#
# #12063 slice 2 edited the oracle in two places, to the new contract rather
# than the old text: a moved base is a plain sleep-and-retry (no branch update,
# no head re-read, no rework marker), and a head mismatch is always the exit-3
# `error_head_moved` (no self-sync retry). Everything else is still verbatim.

_retired_merge_loop() {
  MAX_MERGE_RETRIES=3; MERGE_RETRY_DELAY=5

  for MERGE_ATTEMPT in $(seq 1 $MAX_MERGE_RETRIES); do
    MERGE_RESPONSE=$(forge_merge_pr "$REPO_NWO" "$PR_NUMBER" "$MERGE_PRECONDITION_SHA" "$REPO_MERGE_METHOD" 2>&1) && break  # Success, exit loop

    # Check if it merged despite error (race condition)
    RECHECK_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
    RECHECK=$(echo "$RECHECK_JSON" | jq -r '.merged // false')
    if [[ "$RECHECK" == "true" ]]; then
      warning "Merge reported error but PR is merged (race condition)"
      break
    fi

    # Classify the response ONCE, before any of the three routes below (#8191
    # slice). Placed after the merged-despite-error recheck above deliberately: that
    # one is a forge round-trip rather than a string test, and a PR that merged
    # underneath us has no route to choose. A helper failure here is reported as a
    # HELPER failure — never folded into the terminal "other" route, which would
    # make "could not classify" indistinguishable from "no marker matched".
    MERGE_RESPONSE_KIND="$(_classify_merge_response "$MERGE_RESPONSE")" || error "Merge blocked: PR #$PR_NUMBER's merge-response classifier could not run — '${LOOM_DAEMON_BIN:-loom-daemon} merge-pr classify-response' returned no LOOM-MERGE-RESPONSE verdict (missing binary, or one predating the subcommand). The routes it chooses between are not interchangeable: one retries after syncing the base, and one must NEVER retry a head that moved past the approved SHA (#5579). An unobtainable classification therefore refuses rather than guesses. This is a helper failure, NOT a merge verdict — nothing about this PR was rejected. The forge reported: $MERGE_RESPONSE. $(_mp_daemon_roll_hint merge-pr "$(command -v "${LOOM_DAEMON_BIN:-loom-daemon}" 2>/dev/null || true)")"

    # Check for "Merge already in progress" (HTTP 405)
    # This happens when auto-merge triggers at the same time as our merge attempt
    if [[ "$MERGE_RESPONSE_KIND" == "merge-in-progress" ]]; then
      info "Merge already in progress (HTTP 405), waiting for completion..."
      sleep 5
      RECHECK_JSON=$(forge_get_pr_nocache "$REPO_NWO" "$PR_NUMBER" "$GH" 2>/dev/null || echo '{}')
      RECHECK=$(echo "$RECHECK_JSON" | jq -r '.merged // false')
      if [[ "$RECHECK" == "true" ]]; then
        success "PR #$PR_NUMBER merged (concurrent merge completed)"
        break
      fi
      # Still not merged after wait - continue retry loop
      warning "Concurrent merge not yet complete, retrying..."
      continue
    fi

    # Head-SHA-mismatch (#5579): the PR's OWN head branch moved past
    # $MERGE_PRECONDITION_SHA — distinct from "Base branch was modified" below
    # (that means the BASE fell behind; this means the branch we're trying to
    # merge changed, most commonly a session pushing new commits mid-merge). Do
    # NOT retry-and-merge: retrying would either fail again (session still
    # pushing) or silently squash a different diff than the one Judge approved.
    # Exit 3 so the caller (Champion) re-queues instead of treating this as a
    # failure. See error_head_moved()/_classify_merge_response() above.
    # Since #8164, via _head_moved_or_resync(): a mismatch caused by this run's
    # own base-sync earns exactly one re-read-and-retry; anything else is the
    # same exit-3 re-queue as before.
    # This arm MUST precede the base-modified arm below; since #8191 that
    # precedence lives in the classifier's own ordered match, not in the order of
    # these two `if`s, so a reorder here cannot change which route is taken.
    if [[ "$MERGE_RESPONSE_KIND" == "head-mismatch" ]]; then
      error_head_moved "PR #$PR_NUMBER: $MERGE_RESPONSE" "$MERGE_PRECONDITION_SHA" ""
    fi

    # Check for stale branch error (base branch was modified)
    if [[ "$MERGE_RESPONSE_KIND" == "base-modified" ]]; then
      if [[ $MERGE_ATTEMPT -lt $MAX_MERGE_RETRIES ]]; then
        info "Branch is behind base branch, updating... (attempt $MERGE_ATTEMPT/$MAX_MERGE_RETRIES)"
        sleep "$MERGE_RETRY_DELAY"

        # Increase delay for next attempt (exponential backoff)
        MERGE_RETRY_DELAY=$((MERGE_RETRY_DELAY * 2))
        continue
      else
        error "Failed to merge PR #$PR_NUMBER after $MAX_MERGE_RETRIES attempts: Branch remains behind base branch"
      fi
    fi

    # Other merge errors - fail immediately
    error "Failed to merge PR #$PR_NUMBER: $MERGE_RESPONSE"
  done
}
