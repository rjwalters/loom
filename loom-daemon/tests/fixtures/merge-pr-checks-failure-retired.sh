# merge-pr-checks-failure-retired.sh — the frozen, byte-for-byte copy of the
# failing-check overlap classification inside merge-pr.sh's
# `_wait_for_checks_then_sync_merge` poll loop, as it stood immediately
# before the #8191 slice that moved it to `loom-daemon merge-pr
# checks-failure` (loom-daemon/src/merge_pr/checks_failure.rs).
#
# The retired logic was an inline block inside a larger function rather than
# a function of its own, so the block's lines are wrapped (untouched) in the
# `_retired_classify_failing_checks` function below. The harness supplies
# `failing`/`required`/`pending` as positional args and defines `error`
# (prints `REQUIRED<TAB>message`, exit 1) and `info` BEFORE sourcing; falling
# off the end of the block means "keep waiting on the pending checks", which
# the wrapper reports as `PENDING`. Sourced, never executed.
_retired_classify_failing_checks() {
  local failing="$1" required="$2" pending="$3" PR_NUMBER="${4:-1}"
  # ---- frozen block (merge-pr.sh, pre-port) ----
      local overlap
      overlap="$(comm -12 \
        <(printf '%s\n' "$failing" | sort -u) \
        <(printf '%s\n' "$required" | sort -u))"
      if [[ -n "$overlap" ]]; then
        error "Cannot merge PR #$PR_NUMBER: a required status check has failed ($(printf '%s' "$overlap" | tr '\n' ' ')). Fix the check and re-run the merge."
      fi
      if [[ -z "$pending" ]]; then
        # Only informational (non-required) checks failing and nothing pending →
        # a synchronous merge is safe (matches the UNSTABLE #3486 fallback).
        info "PR #$PR_NUMBER: only informational (non-required) check(s) failing; proceeding to synchronous merge"
        return 0
      fi
      # Informational failures but other checks still running — fall through to
      # the pending wait below.
  # ---- end frozen block ----
  echo "PENDING"
  return 0
}
