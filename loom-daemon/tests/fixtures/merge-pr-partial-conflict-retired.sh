#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_check_partial_increment_close_conflict`
# (#4569 / #4595 / #5234) as it stood immediately before #8191 ported its
# per-issue decision to Rust (`loom-daemon merge-pr partial-conflict`).
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_partial_conflict_differential.rs` can keep
# comparing the Rust against the exact implementation it replaced, forever,
# rather than only at the moment of the port. Reading the function out of the
# live merge-pr.sh stopped being possible the instant it started delegating.
#
# The function is copied WHOLE and VERBATIM. The harness, not an edit here,
# decides what is observed: it sources `merge-pr-refs-retired.sh` for the pure
# shell ref extractors this function called, and defines `gh`, `warning`,
# `_mp_refs` (the backticked-trailer advisory, a separate earlier port),
# `_pr_commit_messages` and `forge_pr_close_targets` as recording stubs.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.
_check_partial_increment_close_conflict() {
  [[ "$FORGE_TYPE" == "github" ]] || return 0

  local pr_body
  pr_body="$(echo "$PR_JSON" | jq -r '.body // ""')"
  [[ -n "$pr_body" ]] || return 0

  # bt_warn/bt_rc are declared here, not next to their own assignment below,
  # purely so that assignment can own a line: see the SC2046 note below.
  local partial_refs bt_warn bt_rc=0
  partial_refs="$(_partial_increment_refs "$pr_body")"

  # Backticked-trailer warning (#5690, ported to Rust #8831 —
  # cli/merge_pr_refs.rs's `backticks-partial-increment-warnings`, which
  # recomputes both declaration sets from $pr_body itself and diffs them, so
  # this call passes nothing but the PR number and dry-run state). Runs BEFORE
  # the early return below because the case it exists for is precisely the one
  # where $partial_refs is EMPTY — a trailer the author backticked, which
  # parses as no declaration at all. Pure text analysis, no forge calls, so
  # the common (non-partial-increment) path still costs zero extra requests.
  # (The statements below share one line deliberately — #8831 pays for the
  # daemon round trip inside the shell-budget ratchet's portable pool, and
  # this keeps that cost at net zero. The unquoted $(...) is intentional: it
  # expands to a single `--dry-run` token or nothing, never anything word
  # splitting could mis-tokenize — hence the SC2046 disable directly below.
  # That directive covers only the ONE statement that follows it, which is why
  # bt_warn/bt_rc are declared up with `local partial_refs` instead of leading
  # this line: as `local bt_warn bt_rc=0; bt_warn="$(...)"` the disable landed
  # on the declaration and the real finding leaked into CI (#8985). Moving the
  # declaration rather than adding a line keeps the budget at net zero too.)
  #
  # #8897: a BARE assignment (not `local var=$(...)`) with `2>/dev/null` and
  # `|| bt_rc=$?`, mirroring _mp_refs's own `out="$(...)" || rc=$?` pattern
  # above — so a daemon that answers `closing-refs` (checked already) but
  # rejects this newer MODE (unrecognized-subcommand exit) is detected here
  # instead of only printing _mp_refs's hardcoded closing-ref "Refusing..."
  # wording to the terminal (wrong mode, wrong PR, wrong version) while the
  # merge proceeds anyway (the `local var=$(...)` exit-status swallow that made
  # this call fail-open in practice all along). This call stays advisory-only:
  # a mode failure is reported as a skipped check, never as a refusal.
  # shellcheck disable=SC2046
  bt_warn="$(printf '%s\n' "$pr_body" | _mp_refs backticks-partial-increment-warnings --pr "$PR_NUMBER" $([[ "${DRY_RUN:-false}" == "true" ]] && echo --dry-run) 2>/dev/null)" || bt_rc=$?; if [[ $bt_rc -eq 0 ]]; then [[ -z "$bt_warn" ]] || warning "$bt_warn"; else warning "Skipped backticked-trailer advisory warning check: loom-daemon rejected 'merge-pr-refs backticks-partial-increment-warnings' (exit $bt_rc) -- most likely a daemon predating this mode. Not refusing; this check is advisory-only."; fi; [[ -n "$partial_refs" ]] || return 0

  # Closing references GitHub will honor on merge, from three unioned signals:
  #   1. the body's own closing keywords (quota-free regex);
  #   2. this PR's COMMIT MESSAGES (#4595) — quota-free REST, and the source of
  #      the squash commit message this script does not override;
  #   3. GitHub's authoritative closingIssuesReferences (best-effort — empty
  #      under GraphQL quota exhaustion, but when it does answer it also
  #      surfaces a Development-sidebar link that no text reveals).
  # The commit fetch happens only past the partial_refs early-return above, so
  # the common (non-partial-increment) path costs zero extra API calls.
  local body_close_refs commit_messages commit_close_refs graphql_close_refs close_refs
  body_close_refs="$(_body_closing_refs "$pr_body")"
  commit_messages="$(_pr_commit_messages)"
  commit_close_refs="$(printf '%s\n' "$commit_messages" | _closing_refs_stdin)"
  graphql_close_refs="$(forge_pr_close_targets "$PR_NUMBER" "$GH" 2>/dev/null || true)"
  close_refs="$(printf '%s\n%s\n%s\n' "$body_close_refs" "$commit_close_refs" "$graphql_close_refs" \
    | grep -E '^[0-9]+$' | sort -un || true)"

  local issue_num issue_json
  while IFS= read -r issue_num; do
    [[ -n "$issue_num" ]] || continue

    # Fresh (uncached) read — plain `gh api`, not $GH, mirroring
    # _reset_one_partial_issue's freshness discipline. Skip PRs that slipped
    # through the regex (the issues endpoint also returns PRs).
    issue_json="$(gh api "repos/$REPO_NWO/issues/$issue_num" 2>/dev/null || echo '{}')"
    if [[ "$(echo "$issue_json" | jq -r 'has("pull_request")')" == "true" ]]; then
      continue
    fi
    # Only an issue that is OPEN right now can be closed BY this merge; one that
    # is already closed was closed by something else and is not ours to revert.
    if [[ "$(echo "$issue_json" | jq -r '.state // ""')" != "open" ]]; then
      continue
    fi
    PARTIAL_OPEN_BEFORE_MERGE="${PARTIAL_OPEN_BEFORE_MERGE:+$PARTIAL_OPEN_BEFORE_MERGE }$issue_num"

    if ! grep -qx "$issue_num" <<<"$close_refs"; then
      continue
    fi
    PARTIAL_CONFLICT_ISSUES="${PARTIAL_CONFLICT_ISSUES:+$PARTIAL_CONFLICT_ISSUES }$issue_num"

    local body_offending commit_offending partial_offending dr=""
    # Match _check_no_open_stacked_children's dry-run contract: report the
    # would-be outcome without claiming a merge is happening.
    [[ "${DRY_RUN:-false}" == "true" ]] && dr="[dry-run] "
    body_offending="$(_closing_ref_snippets "$pr_body" "$issue_num")"
    commit_offending="$(_closing_ref_snippets "$commit_messages" "$issue_num")"
    # The declaration text itself (AC #4, #5234) — quoted alongside the closing
    # keyword below so an operator can see both sides and judge for themselves
    # whether the declaration was a real trailer or, e.g., prose that happened
    # to survive the structural anchor.
    partial_offending="$(_partial_increment_ref_snippets "$pr_body" "$issue_num")"

    # Name the source, because the operator remedy differs per source: edit the
    # PR body, reword/amend a commit, or unlink a Development-sidebar reference.
    if [[ -n "$body_offending" ]]; then
      warning "${dr}Partial-increment conflict (#4569): PR #$PR_NUMBER declares a NON-closing \`Part of\`/\`Contributes to\` reference to #$issue_num (\"$partial_offending\"), but its body ALSO carries a closing reference to #$issue_num (\"$body_offending\") — GitHub honors a closing keyword ANYWHERE in the body, so merging this PR WILL close #$issue_num against the declared intent."
      warning "  ${dr}merge-pr.sh would reopen #$issue_num immediately after the merge. To avoid the close/reopen flicker entirely, edit the PR body so no closing keyword is immediately followed by \`#$issue_num\` (e.g. write \`close the issue\` or \`close issue #$issue_num\` instead of \`close #$issue_num\`), then re-run this merge."
    elif [[ -n "$commit_offending" ]]; then
      warning "${dr}Partial-increment conflict (#4595): PR #$PR_NUMBER declares a NON-closing \`Part of\`/\`Contributes to\` reference to #$issue_num (\"$partial_offending\"), but a closing keyword in a commit message of this PR references #$issue_num (\"$commit_offending\") — this merge squashes without overriding the commit message, so GitHub composes the squash message from these commits and merging WILL close #$issue_num against the declared intent."
      warning "  ${dr}merge-pr.sh would reopen #$issue_num immediately after the merge. To avoid the close/reopen flicker entirely, reword the offending commit message (\`git commit --amend\` / \`git rebase -i\` + force-push) so no closing keyword is immediately followed by \`#$issue_num\`, then re-run this merge."
    else
      warning "${dr}Partial-increment conflict (#4569): PR #$PR_NUMBER declares a NON-closing \`Part of\`/\`Contributes to\` reference to #$issue_num (\"$partial_offending\"), but GitHub reports #$issue_num as a closing target of this PR (no closing keyword found in the body or commit messages — most likely a Development-sidebar link), so merging this PR WILL close #$issue_num against the declared intent."
      warning "  ${dr}merge-pr.sh would reopen #$issue_num immediately after the merge. To avoid the close/reopen flicker entirely, unlink #$issue_num from this PR's Development sidebar, then re-run this merge."
    fi
  done <<< "$partial_refs"

  return 0
}
