# merge-pr-mergeable-recheck-retired.sh — the frozen, byte-for-byte copy of
# merge-pr.sh's `_recheck_mergeable_before_refusal` as it stood immediately
# before the #8191 slice that moved its terminal classification to
# `loom-daemon merge-pr mergeable-recheck`
# (loom-daemon/src/merge_pr/mergeable_recheck.rs).
#
# Reading it from the live merge-pr.sh stopped being possible the moment it
# began delegating, and reading it from git history would pin this test to a
# moving ref. This fixture is what keeps the differential a real comparison
# once the shell is gone (the same convention as
# merge-pr-refs-retired.sh). Sourced, never executed: the harness below
# defines the `forge_get_pr_nocache` stub BEFORE sourcing so the frozen loop
# replays a canned .mergeable sequence, exactly as the retained suite does.
#
# The function itself follows, untouched:
_recheck_mergeable_before_refusal() {
  local nwo="$1" pr_number="$2" gh_cmd="$3" base_ref="$4" head_ref="$5" repo_root="$6"
  local retries="${7:-3}" delay="${8:-3}"
  local attempt recheck_json recheck_mergeable

  for attempt in $(seq 1 "$retries"); do
    sleep "$delay"
    recheck_json="$(forge_get_pr_nocache "$nwo" "$pr_number" "$gh_cmd" 2>/dev/null || echo '{}')"
    recheck_mergeable="$(echo "$recheck_json" | jq -r '.mergeable // empty')"
    if [[ "$recheck_mergeable" == "true" ]]; then
      echo "merge:cached mergeable=false was stale; recheck #$attempt (post-backoff, uncached) now reports mergeable=true"
      return 0
    fi
  done

  # Still false/unknown after the backoff retries — corroborate with a local
  # git merge-tree check before conceding this is a genuine conflict.
  if [[ -z "$base_ref" ]] || [[ -z "$head_ref" ]]; then
    echo "refuse-stale:forge reports mergeable=false after $retries recheck(s); base/head ref unavailable for local corroboration"
    return 0
  fi

  if ! git -C "$repo_root" fetch -q origin "$base_ref" "$head_ref" 2>/dev/null; then
    echo "refuse-stale:forge reports mergeable=false after $retries recheck(s); could not fetch origin/$base_ref and origin/$head_ref for local corroboration"
    return 0
  fi

  if git -C "$repo_root" merge-tree --write-tree "origin/$base_ref" "origin/$head_ref" >/dev/null 2>&1; then
    echo "merge:forge reports mergeable=false after $retries recheck(s), but local 'git merge-tree' against origin/$base_ref is clean — proceeding (stale/false-negative cached state)"
    return 0
  fi

  echo "refuse-conflict:forge reports mergeable=false after $retries recheck(s), confirmed by local 'git merge-tree' against origin/$base_ref — this branch genuinely conflicts"
  return 0
}
