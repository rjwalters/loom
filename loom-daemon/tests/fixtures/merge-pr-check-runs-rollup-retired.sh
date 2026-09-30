# merge-pr-check-runs-rollup-retired.sh — the frozen, byte-for-byte copy of
# the check-runs rollup read inside merge-pr.sh's
# `_wait_for_checks_then_sync_merge` poll loop (the three `jq` filters that
# derived `failing`, `pending` and `total_count` from `$runs_raw`), as it stood
# immediately before the #8191 slice that moved it to `loom-daemon merge-pr
# check-runs-rollup` (loom-daemon/src/merge_pr/check_runs_rollup.rs).
#
# The retired logic was an inline block inside a larger function rather than
# a function of its own, so the block's lines are wrapped (untouched) in the
# `_retired_check_runs_rollup` function below. The harness passes the payload
# as $1 and receives the three values NUL-terminated on stdout, in the same
# order the port frames them. Sourced, never executed.
_retired_check_runs_rollup() {
  local runs_raw="$1"
  # ---- frozen block (merge-pr.sh, pre-port) ----
    # Failing (terminal non-success) and pending (not yet completed) check names.
    local failing pending total_count
    failing="$(echo "$runs_raw" | \
      jq -r '[.check_runs[] | select(.conclusion == "failure" or .conclusion == "timed_out" or .conclusion == "cancelled" or .conclusion == "action_required") | .name] | unique | .[]' 2>/dev/null || true)"
    pending="$(echo "$runs_raw" | \
      jq -r '[.check_runs[] | select(.status != "completed") | .name] | unique | .[]' 2>/dev/null || true)"
    total_count="$(echo "$runs_raw" | jq -r '.total_count // 0' 2>/dev/null || echo 0)"
    [[ "$total_count" =~ ^[0-9]+$ ]] || total_count=0
  # ---- end frozen block ----
  printf '%s\0%s\0%s\0' "$failing" "$pending" "$total_count"
}
