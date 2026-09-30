# merge-pr-check-runs-rollup-retired.sh — the frozen, byte-for-byte copy of
# the check-runs rollup parse at the top of each `_wait_for_checks_then_sync_merge`
# poll in merge-pr.sh, as it stood immediately before the #8191 slice that
# moved it to `loom-daemon merge-pr check-runs-rollup`
# (loom-daemon/src/merge_pr/check_runs_rollup.rs).
#
# The retired logic was three inline `jq` pipelines plus bash's own
# `^[0-9]+$` gate inside a larger loop rather than a function of its own, so
# those lines are wrapped (untouched) in `_retired_parse_rollup` below. The
# harness supplies the rollup payload as $1 and reads back a rendering of
# EXACTLY the four things the rest of the poll went on to consume:
#
#   TOTAL<TAB><total_count>              fed to `[[ -gt 0 ]]` / the zero-row guard
#   ANY<TAB><-n failing>/<-n pending>    the two branch tests
#   LINES<TAB><printf|wc -l of pending>  narrated as "N check(s) still running"
#   FJOIN<TAB><failing, newlines as |>   the NUL frame sent to `checks-failure`
#   PJOIN<TAB><pending, newlines as |>
#
# The `-n` tests are rendered rather than the row COUNTS because they are what
# the loop branched on, and they are not the same question: a lone check-run
# named "" yields one empty line, which `$(…)` strips to the empty string, so
# `[[ -n "$pending" ]]` read it as "nothing pending". `tr '\n' '|'` is applied
# identically on both sides of the differential, so a name that itself
# contains `|` is rendered ambiguously by BOTH and the comparison still
# holds. Sourced, never executed.
_retired_parse_rollup() {
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
  local f_any=0 p_any=0
  [[ -z "$failing" ]] || f_any=1
  [[ -z "$pending" ]] || p_any=1
  printf 'TOTAL\t%s\n' "$total_count"
  printf 'ANY\t%s/%s\n' "$f_any" "$p_any"
  printf 'LINES\t%s\n' "$(printf '%s\n' "$pending" | wc -l | tr -d ' ')"
  printf 'FJOIN\t%s\n' "$(printf '%s' "$failing" | tr '\n' '|')"
  printf 'PJOIN\t%s\n' "$(printf '%s' "$pending" | tr '\n' '|')"
  return 0
}
