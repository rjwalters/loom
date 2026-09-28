#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's pre-merge version-policy guard
# (_check_defaults_version_bump_collision, #7827 + the #8284 oracle choice), as
# it stood immediately before #8191 ported it to `loom-daemon merge-pr
# version-policy`.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_version_policy_differential.rs` can keep
# comparing the port against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the function out of the
# live merge-pr.sh stopped being possible the instant that file started
# delegating; reading it from git history would pin the test to a moving ref.
#
# The caller supplies REPO_ROOT, DEFAULT_BRANCH_NAME, PR_BRANCH, PR_HEAD_SHA,
# PR_NUMBER and DRY_RUN, plus `warning`/`error` shims. DO NOT "fix" anything
# here: its value is being a faithful record of the retired behaviour.
# shellcheck disable=SC2034,SC2154

_check_defaults_version_bump_collision() {
  local checker_rel="defaults/scripts/check-defaults-version-bump.sh" check_script="$REPO_ROOT/defaults/scripts/check-defaults-version-bump.sh" current_main_sha="" merge_base="" head_checker=""
  [[ -x "$check_script" ]] || return 0
  [[ -n "${DEFAULT_BRANCH_NAME:-}" ]] || return 0
  [[ -n "${PR_HEAD_SHA:-}" ]] || return 0
  [[ -n "${PR_BRANCH:-}" ]] || return 0

  # Best-effort fetch of the default branch's current tip and this PR's own
  # branch. A failure here (offline, transient forge issue) means the guard
  # cannot see anything fresher than what's already local — skip rather than
  # block on stale/missing data. `|| return 0` (not `|| true`) keeps this a
  # single early-exit instead of proceeding with a possibly-stale fetch.
  git -C "$REPO_ROOT" fetch --quiet origin "$DEFAULT_BRANCH_NAME" "$PR_BRANCH" 2>/dev/null || return 0

  current_main_sha="$(git -C "$REPO_ROOT" rev-parse --verify --quiet "origin/$DEFAULT_BRANCH_NAME" 2>/dev/null || true)"
  [[ -n "$current_main_sha" ]] || return 0

  # The PR head commit must be reachable locally post-fetch (it will be,
  # having just fetched PR_BRANCH above) — guards a fork-PR or
  # already-deleted-branch edge case where it might not resolve.
  git -C "$REPO_ROOT" rev-parse --verify --quiet "${PR_HEAD_SHA}^{commit}" >/dev/null 2>&1 || return 0

  # The checker's shallow-history fallback compares raw tips. That cannot
  # establish who changed a version; refuse to label it a confirmed edit.
  # The merge base is also what scopes the machinery-touch test below to this
  # PR's own commits, so it is captured rather than discarded.
  if ! merge_base="$(git -C "$REPO_ROOT" merge-base "$current_main_sha" "$PR_HEAD_SHA" 2>/dev/null)"; then
    warning "Version policy guard: PR ancestry unavailable; skipping unverified comparison."
    return 0
  fi

  local check_output check_rc=0 checker_ref="'$DEFAULT_BRANCH_NAME' ($current_main_sha)"

  # Does this PR's own diff change the version-policy machinery? If so the
  # PR head's checker is the oracle, matching CI (see the header above).
  if [[ -n "$(git -C "$REPO_ROOT" diff --name-only "$merge_base" "$PR_HEAD_SHA" -- "$checker_rel" defaults/scripts/version-check-gate.sh scripts/version.sh 2>/dev/null)" ]]; then
    head_checker="$(mktemp "${TMPDIR:-/tmp}/loom-version-policy-checker.XXXXXX")"
    if git -C "$REPO_ROOT" show "$PR_HEAD_SHA:$checker_rel" >"$head_checker" 2>/dev/null && [[ -s "$head_checker" ]] && chmod +x "$head_checker"; then
      check_script="$head_checker"; checker_ref="the PR head ($PR_HEAD_SHA)"
    else rm -f "$head_checker"; head_checker=""; fi
    warning "Version policy guard: this PR's own commits change the version-policy machinery, so the guard evaluates the checker from $checker_ref — the ref CI's defaults-version-bump-check job evaluates (#8284). A head lookup that fails falls back to '$DEFAULT_BRANCH_NAME''s copy, never to skipping the check."
  fi

  check_output=$(cd "$REPO_ROOT" && "$check_script" --forbid-bump --base "$current_main_sha" --head "$PR_HEAD_SHA" 2>&1) || check_rc=$?
  [[ -z "$head_checker" ]] || rm -f "$head_checker"

  [[ "$check_rc" -ne 0 ]] || return 0

  # A non-zero, non-1 exit (bad usage, unresolved ref) is a guard-internal
  # problem, not a confirmed version edit — report and skip rather than block a
  # merge on a guard fault.
  if [[ "$check_rc" -ne 1 ]]; then
    warning "Version policy guard: check-defaults-version-bump.sh (from $checker_ref) exited $check_rc against current '$DEFAULT_BRANCH_NAME' ($current_main_sha) — skipping (not a confirmed version edit):"$'\n'"$check_output"; return 0
  fi

  local msg="Merge blocked: PR #$PR_NUMBER hand-edits a version-bearing value (#7827).

$check_output

Revert the version-value changes authored by this PR, preserving its other changes,
then rerun CI and review. Version bumps are applied automatically by the merge workflow
(#7743); a no-surface-change marker cannot waive this policy. (Checker from $checker_ref.)"

  # --dry-run still runs the guard and REPORTS the would-be block, but honors
  # the dry-run contract (never exits 1) — same shape as the guard above.
  if [[ "$DRY_RUN" == "true" ]]; then
    warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: forbidden version edit relative to '$DEFAULT_BRANCH_NAME' ($current_main_sha), per the checker from $checker_ref."; return 0
  fi

  error "$msg"
}
