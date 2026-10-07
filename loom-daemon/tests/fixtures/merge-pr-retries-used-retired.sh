# merge-pr-retries-used-retired.sh — the frozen, byte-for-byte copy of the
# telemetry `retries_used` derivation in merge-pr.sh's mergeability gate
# (#6978), as it stood immediately before the #8191 slice that ported it to
# `loom-daemon merge-pr retries-used`
# (loom-daemon/src/merge_pr/retries_used.rs).
#
# Sourced, never executed. Wrapped in a function only so it can be called, with
# one added printf to expose the result; those two lines are the only edits.

_retired_retries_used() {
  local _MSM_REASON="$1" _MSM_RETRIES="$2" _MSM_RETRIES_USED
  # ---- frozen block (merge-pr.sh, pre-port) ----
  _MSM_RETRIES_USED="$_MSM_RETRIES"
  if [[ "$_MSM_REASON" =~ recheck\ \#([0-9]+) ]]; then
    _MSM_RETRIES_USED="${BASH_REMATCH[1]}"
  fi
  # ---- end frozen block ----
  printf '%s\n' "$_MSM_RETRIES_USED"
}
