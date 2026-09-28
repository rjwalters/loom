#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's pre-merge merge-ordering guard (#3747 item 2,
# reshaped by #7982) — its child-discovery parsing, its branch-shape gate and
# every operator-facing message it renders — as they stood immediately before
# #8191 ported them to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_stacked_children_differential.rs` can keep
# comparing the port against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the pipelines out of the
# live merge-pr.sh stopped being possible the instant that file started
# delegating; reading them from git history would pin the test to a moving ref.
#
# What is frozen here is everything the guard DECIDED and everything it SAID.
# The three git calls it used to establish the pin (`cat-file -e`, `fetch`,
# `update-ref`) are NOT frozen: their behaviour is git's, both sides invoke the
# same binary with the same arguments, and the Rust port's own
# `a_resolvable_tip_is_pinned_to_the_ref_reconcile_stack_reads` /
# `an_unresolvable_tip_is_not_pinned_and_leaves_no_ref` tests exercise them
# against a real repository, which is the only way that chain can fail
# interestingly. What WAS defective-prone — and what a reader cannot verify by
# inspection — is which children were found and what the four messages said
# about them, so those are what this file preserves.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour. If the Rust should diverge from this further, that is a
# deliberate behaviour change belonging in its own issue, and this file should be
# left alone while the test's expectation is updated with a comment saying why.

# _retired_is_stackable_parent <branch> — prints `true`/`false`.
#
# Verbatim the guard's second gate: `[[ "$PR_BRANCH" =~
# ^feature/issue-([0-9]+)$ ]] || return 0`. The strict anchors are the point —
# `release-1` and `fix-bug-42` must classify as PR-style, not issue-style.
_retired_is_stackable_parent() {
  if [[ "$1" =~ ^feature/issue-([0-9]+)$ ]]; then printf 'true\n'; else printf 'false\n'; fi
}

# _retired_count <children-json-on-stdin> — the guard's `count`.
#
# Verbatim: `count="$(echo "$children_json" | jq 'length' 2>/dev/null || echo 0)"`
# followed by `[[ "$count" -gt 0 ]] || return 0`. The `|| echo 0` is why
# malformed forge output skipped the guard rather than failing it.
_retired_count() {
  local children_json
  children_json="$(cat)"
  echo "$children_json" | jq 'length' 2>/dev/null || echo 0
}

# _retired_has_open_children <children-json-on-stdin> — prints `true`/`false`.
#
# The guard's actual fire/skip PREDICATE, verbatim: the `[[ -n
# "$children_json" ]]` emptiness check, then `count`, then `[[ "$count" -gt 0
# ]] || return 0`. Kept alongside `_retired_count` because the two are not the
# same question — on empty input `jq` prints nothing at all (exit 0, so the
# `|| echo 0` never fires) and `count` is the EMPTY STRING, which `[[ -gt ]]`
# then evaluates as 0. Comparing the predicate is what says whether the guard
# fired; comparing `_retired_count` says what the messages interpolated.
_retired_has_open_children() {
  local children_json count
  children_json="$(cat)"
  [[ -n "$children_json" ]] || { printf 'false\n'; return 0; }
  count="$(echo "$children_json" | jq 'length' 2>/dev/null || echo 0)"
  if [[ "$count" -gt 0 ]]; then printf 'true\n'; else printf 'false\n'; fi
}

# _retired_child_list <children-json-on-stdin> — the guard's `child_list`.
#
# Verbatim: `jq -r '[.[].number | "#" + tostring] | join(", ")' 2>/dev/null ||
# echo ''`.
_retired_child_list() {
  local children_json
  children_json="$(cat)"
  echo "$children_json" \
    | jq -r '[.[].number | "#" + tostring] | join(", ")' 2>/dev/null || echo ''
}

# _retired_cmds <parent-branch> <children-json-on-stdin> — the guard's `cmds`.
#
# Verbatim: `jq -r --arg p "$PR_BRANCH" '.[] | "  ./.loom/scripts/reconcile-stack.sh " + (.number|tostring) + " " + $p' 2>/dev/null || echo "  ./.loom/scripts/reconcile-stack.sh <child-pr> $PR_BRANCH"`.
_retired_cmds() {
  local pr_branch="$1" children_json
  children_json="$(cat)"
  echo "$children_json" | jq -r --arg p "$pr_branch" '.[] | "  ./.loom/scripts/reconcile-stack.sh " + (.number|tostring) + " " + $p' 2>/dev/null || echo "  ./.loom/scripts/reconcile-stack.sh <child-pr> $pr_branch"
}

# The four messages, each taking the values the guard had in scope at the point
# it rendered them. Reproduced character for character, including the em dashes
# and the `$'\n'` joins.

# _retired_bypass_msg <count> <child_list> <pr_branch>
_retired_bypass_msg() {
  printf '%s\n' "Merge-ordering guard: --allow-stacked-children set; proceeding despite $1 open stacked child PR(s) ($2) targeting '$3' (operator asserts they are reconciled)"
}

# _retired_dry_run_msg <count> <child_list> <pr_branch> <pin>
_retired_dry_run_msg() {
  printf '%s\n' "[dry-run] $1 open stacked child PR(s) ($2) still target '$3'. A real run would pin the parent tip to $4 and proceed with a warning naming each child, or hard-block if the tip could not be pinned. No ref was written."
}

# _retired_pinned_msg <pr_number> <pr_branch> <count> <child_list> <pin> <pr_head_sha> <cmds>
_retired_pinned_msg() {
  printf '%s\n' "Merge-ordering guard: PR #$1's branch '$2' still has $3 open stacked child PR(s) ($4) targeting it. Pinned the parent tip to $5 ($6) so reconcile-stack.sh can still resolve '$2' after the merge deletes it (#3747 item 2, #7982). Proceeding with the merge — reconcile each child once this has landed:"$'\n'"$7"
}

# _retired_blocked_msg <pr_number> <pr_branch> <count> <child_list> <pin> <pr_head_sha> <cmds>
_retired_blocked_msg() {
  printf '%s\n' "Merge blocked: PR #$1's branch '$2' still has $3 open stacked child PR(s) ($4) targeting it, and its tip ($6) could not be pinned to $5 — a detached or unreadable parent. Merging now would race the repo's delete_branch_on_merge setting: GitHub deletes '$2' synchronously during the merge, before the child PR(s) can be rebased/retargeted onto the default branch — leaving reconcile-stack.sh's rebase unable to resolve the parent branch ref (#3747 item 2). Reconcile each child first (from a clean checkout), then re-run this merge:"$'\n'"$7"$'\n'"Or, if you have already verified/reconciled them, re-run with --allow-stacked-children to bypass this guard."
}
