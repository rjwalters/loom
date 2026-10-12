# merge-pr-poll-wait-retired.sh — the frozen copy of the two "keep polling"
# arms of merge-pr.sh's _wait_for_checks_then_sync_merge (the unfetchable
# check-runs arm and the pending-checks arm, #8896), as they stood immediately
# before the #8191 slice that ported their deadline-or-wait decision to
# `loom-daemon merge-pr poll-wait` (loom-daemon/src/merge_pr/poll_wait.rs).
#
# Sourced, never executed. Wrapped in a function so it can be driven; the
# only edits are: `date +%s` is the `$now` argument, `exit 5` is `RESULT=TIMEOUT`
# plus a return, `sleep` is `RESULT=WAIT`, and the three narration helpers
# record `<level> <text>` into $LEVEL/$TEXT instead of printing to the terminal.

warning() { LEVEL=warning; TEXT="$*"; }
info() { LEVEL=info; TEXT="$*"; }

# _retired_unfetchable_arm <now> <deadline> <timeout> <interval> <pr> <fetch_rc>
_retired_unfetchable_arm() {
  local now="$1" deadline="$2" LOOM_AUTO_MERGE_TIMEOUT="$3" LOOM_AUTO_MERGE_POLL_INTERVAL="$4" PR_NUMBER="$5" fetch_rc="$6"
  # ---- frozen block (merge-pr.sh, pre-port) ----
      if [[ "$now" -ge "$deadline" ]]; then
        warning "Timed out after ${LOOM_AUTO_MERGE_TIMEOUT}s waiting for check-runs to become fetchable for PR #$PR_NUMBER — exiting 5 (not merged, not a failure: re-queue). Re-run once the forge API is healthy, or raise LOOM_AUTO_MERGE_TIMEOUT."; RESULT=TIMEOUT; echo "$RESULT $LEVEL $TEXT"; return 0
      fi
      warning "Failed to fetch check-runs for PR #$PR_NUMBER (rc=$fetch_rc); treating as still-pending and continuing to poll"
      RESULT=WAIT
  # ---- end frozen block ----
  echo "$RESULT $LEVEL $TEXT"
}

# _retired_pending_arm <now> <deadline> <timeout> <interval> <pr> <pending>
_retired_pending_arm() {
  local now="$1" deadline="$2" LOOM_AUTO_MERGE_TIMEOUT="$3" LOOM_AUTO_MERGE_POLL_INTERVAL="$4" PR_NUMBER="$5" pending="$6"
  # ---- frozen block (merge-pr.sh, pre-port) ----
      local n; n="$(printf '%s\n' "$pending" | wc -l | tr -d ' ')"
      if [[ "$now" -ge "$deadline" ]]; then
        warning "Timed out after ${LOOM_AUTO_MERGE_TIMEOUT}s waiting for ${n} pending check(s) on PR #$PR_NUMBER to complete — exiting 5 (not merged, not a failure: re-queue). Re-run once CI settles, or raise LOOM_AUTO_MERGE_TIMEOUT."; RESULT=TIMEOUT; echo "$RESULT $LEVEL $TEXT"; return 0
      fi
      info "PR #$PR_NUMBER: ${n} check(s) still running; waiting ${LOOM_AUTO_MERGE_POLL_INTERVAL}s for CI (timeout ${LOOM_AUTO_MERGE_TIMEOUT}s)..."
      RESULT=WAIT
  # ---- end frozen block ----
  echo "$RESULT $LEVEL $TEXT"
}
