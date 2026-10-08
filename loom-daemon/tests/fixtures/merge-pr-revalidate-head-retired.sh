# merge-pr-revalidate-head-retired.sh — the frozen copy of the decision half of
# merge-pr.sh's `_revalidate_merge_guards` (#8410/#8896), as it stood
# immediately before the #8191 slice that ported it to
# `loom-daemon merge-pr revalidate-head`
# (loom-daemon/src/merge_pr/revalidate_head.rs).
#
# Sourced, never executed. Wrapped in a function only so it can be called, with
# the `error`/`error_head_moved`/`return` exits replaced by one printf each that
# names the verdict (and the fresh SHA / labels, which the Rust verb prints);
# the jq filters and the test expressions are byte-for-byte the originals.

_retired_revalidate() {
  local fresh="$1" MERGE_PRECONDITION_SHA="$2" fresh_sha
  # ---- frozen block (merge-pr.sh, pre-port) ----
  [[ "$(echo "$fresh" | jq -r '.merged // false')" == "true" ]] && { echo "LOOM-REVALIDATE MERGED"; return 0; }
  fresh_sha="$(echo "$fresh" | jq -r '.head.sha // empty')"; [[ -n "$fresh_sha" ]] || { echo "LOOM-REVALIDATE NO-HEAD"; return 0; }
  if [[ -n "$fresh_sha" && -n "$MERGE_PRECONDITION_SHA" && "$fresh_sha" != "$MERGE_PRECONDITION_SHA" ]]; then
    echo "LOOM-REVALIDATE MOVED $fresh_sha"; return 0
  fi
  PR_LABELS="$(echo "$fresh" | jq -r '.labels[]?.name // empty' 2>/dev/null || true)"
  # ---- end frozen block ----
  echo "LOOM-REVALIDATE CLEAR"
  [[ -z "$PR_LABELS" ]] || printf '%s\n' "$PR_LABELS"
}
