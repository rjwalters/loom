#!/usr/bin/env bash
# FROZEN COPY — do not "fix", do not sync with merge-pr.sh.
#
# `defaults/scripts/merge-pr.sh`'s post-merge stacked-child reconcile decisions
# exactly as they stood immediately before #8191 slice ported them to
# `loom-daemon/src/merge_pr/reconcile.rs`. `merge_pr_reconcile_differential.rs`
# feeds this and the Rust the same corpus and requires identical answers.
#
# WHY A COPY AND NOT THE LIVE SCRIPT
#
# The live functions now delegate to `loom-daemon merge-pr reconcile-plan` /
# `merge-pr reconcile-child`, so extracting them from `merge-pr.sh` would compare
# the port against itself and pass unconditionally — the "measured nothing"
# failure `defaults/docs/verification-recipes.md` §6 names. Reading them out of
# git history instead would pin this test to a moving ref. A frozen copy is the
# only form that keeps saying something true once the shell is gone.
#
# WHAT THIS MODELS, AND WHAT IT DOES NOT — READ THIS BEFORE TRUSTING IT
#
# Per the same recipe ("If the test needs its own copy of the pattern, the
# parser, or the ordering rule, say explicitly WHICH implementation it models"):
# this models the **retired** implementation. Three of the four helpers below are
# *reconstructions* — the retired code was inline, not a function — and the
# distinction matters, so it is spelled out precisely:
#
#   * VERBATIM, character for character:
#       - `retired_child_issue`'s `[[ "$1" =~ ^feature/issue-([0-9]+)$ ]]` and
#         its `BASH_REMATCH[1]` capture. This is the predicate the retired
#         script wrote out TWICE (parent gate at the top of
#         `_auto_reconcile_stacked_children`, child derivation at the top of
#         `_reconcile_one_stacked_child`); one copy is reproduced here because
#         both copies were byte-identical.
#       - `retired_building`'s `printf '%s\n' "$issue_labels" | grep -qx
#         'loom:building'` — note `-qx`, i.e. whole-line, and case-SENSITIVE.
#       - `retired_count`'s `jq 'length'` and `retired_rows`' `jq -r '.[] |
#         "\(.number)\t\(.headRefName)"'`, including both `2>/dev/null` and both
#         `|| …` fallbacks (`|| echo 0` and `|| true`), which are the layers that
#         made every failure look like "no children".
#       - `retired_comment`'s heredoc body, including the em dash, every
#         backtick, the fenced block and the `*Deferred by merge-pr.sh (#3747)
#         at $ts*` attribution line, with NO trailing newline — the retired
#         `comment="…"` ended at its closing quote.
#
#   * RECONSTRUCTED: the composition. The retired script ran the parent gate,
#     then `jq 'length'`, then `[[ "$count" -gt 0 ]]`, then the row extraction,
#     then per row the label read and the `grep -qx`. Those were straight-line
#     statements inside two functions, interleaved with `gh` calls and `info`
#     output. They are re-exposed here as four argument-taking helpers so a
#     differential can drive each decision without a forge.
#
#   * OUT OF SCOPE: everything with an effect — the `gh pr list` discovery, the
#     `gh api` label fetch, the `reconcile-stack.sh` invocation, the comment
#     POST, and all `info`/`warning`/`success` narration. Those stayed in
#     `merge-pr.sh` and are covered by `test-merge-pr-auto-reconcile.sh`'s
#     T1-T9, which drive the live script.
#
# `LC_ALL=C` is pinned by the harness, not here, so a caller cannot forget it.

set -uo pipefail

# VERBATIM (both copies were identical). Echoes the issue number, or nothing.
retired_child_issue() {
  local child_branch="$1"
  local child_issue=""
  if [[ "$child_branch" =~ ^feature/issue-([0-9]+)$ ]]; then
    child_issue="${BASH_REMATCH[1]}"
  fi
  printf '%s' "$child_issue"
}

# VERBATIM: the parent gate was `[[ "$PR_BRANCH" =~ ^feature/issue-([0-9]+)$ ]]
# || return 0` — the same regex, its capture unused. Exits 0 when stacked.
retired_is_stacked() {
  [[ "$1" =~ ^feature/issue-([0-9]+)$ ]]
}

# VERBATIM `jq`, including the `2>/dev/null || echo 0` fallback.
retired_count() {
  echo "$1" | jq 'length' 2>/dev/null || echo 0
}

# VERBATIM `jq`, including the `2>/dev/null || true` fallback. Emits one
# `<number>\t<headRefName>` row per element.
retired_rows() {
  echo "$1" | jq -r '.[] | "\(.number)\t\(.headRefName)"' 2>/dev/null || true
}

# VERBATIM: the `grep -qx` claim test. Exits 0 when the child issue is building.
# The retired caller reached this only when $child_issue was non-empty; an empty
# $issue_labels (a failed lookup) reached it as the empty string.
retired_building() {
  local issue_labels="$1"
  printf '%s\n' "$issue_labels" | grep -qx 'loom:building'
}

# VERBATIM heredoc body. Args mirror the retired locals in the order the text
# interpolates them.
retired_comment() {
  local parent_branch="$1" child_issue="$2" child_pr="$3" ts="$4"
  local comment="## Stacked parent merged — reconciliation deferred

Parent branch \`$parent_branch\` squash-merged, but this child's issue #$child_issue is still \`loom:building\` — a Builder likely has this branch checked out. Auto-reconciliation was **skipped** to avoid racing that in-progress work with an out-of-band \`git rebase --onto\` + \`push --force-with-lease\`.

**What happens next**: once issue #$child_issue is no longer \`loom:building\`, a subsequent parent-merge-triggered pass will reconcile this PR automatically. You can also reconcile it by hand now (from a clean checkout, only once the Builder has finished):

\`\`\`
./.loom/scripts/reconcile-stack.sh $child_pr $parent_branch
\`\`\`

---
*Deferred by merge-pr.sh (#3747) at $ts*"
  printf '%s' "$comment"
}

# RECONSTRUCTED composition: the whole plan the retired straight-line code
# produced for one parent branch + rollup, as the records the port emits.
#   NOT-STACKED
#   COUNT <n>\nCHILD <pr>\t<branch>\t<issue>…
# A count of 0 (or a non-integer count, which `[[ -gt ]]` rejected) was the
# retired `return 0` with no rows.
retired_plan() {
  local parent_branch="$1" rollup="$2"
  retired_is_stacked "$parent_branch" || { printf 'NOT-STACKED\n'; return 0; }
  local count
  count="$(retired_count "$rollup")"
  if ! [[ "$count" -gt 0 ]] 2>/dev/null; then printf 'COUNT 0\n'; return 0; fi
  printf 'COUNT %s\n' "$count"
  local child_pr child_branch
  while IFS=$'\t' read -r child_pr child_branch; do
    [[ -n "$child_pr" ]] || continue
    printf 'CHILD %s\t%s\t%s\n' "$child_pr" "$child_branch" "$(retired_child_issue "$child_branch")"
  done <<< "$(retired_rows "$rollup")"
}

# RECONSTRUCTED composition: the route one child took. The retired code read
# labels only when $child_issue was non-empty, so an empty issue short-circuited
# to the reconcile branch without consulting them at all.
retired_route() {
  local child_issue="$1" issue_labels="$2"
  if [[ -n "$child_issue" ]] && retired_building "$issue_labels"; then
    printf 'defer\n'
  else
    printf 'reconcile\n'
  fi
}
