#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_check_ci_result` as it stood immediately
# before #12063 slice 1 removed it (the CI-run conclusion gate's caller,
# #10444 / #10567).
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# WHY IT EXISTS
#
# An installed merge-pr.sh from 0.19.763 up to 0.19.1077 predates the
# `required-checks` gate (#10688), so on its plain (non --auto) path this
# function was its only loom-side CI verification. A host can run such a
# script against a newer loom-daemon until its `.loom/scripts` resyncs, so the
# retired `merge-pr ci-result` verb must keep refusing a red head for it.
# tests/merge_pr_ci_result_retired_caller.rs drives this function against the
# real binary.
#
# WHAT IS VERBATIM, AND WHAT IS NOT
#
#   * VERBATIM, character for character: the `_check_ci_result` one-liner,
#     copied from merge-pr.sh at the parent of slice 1.
#   * RECONSTRUCTED: nothing. Its globals (FORGE_TYPE, PR_NUMBER, ...) and the
#     info / warning / error / _mp_daemon_roll_hint helpers are supplied by
#     the test.
_check_ci_result() { [[ "$FORGE_TYPE" == "github" ]] || return 0; local phase="${1:-}" sha="$PR_HEAD_SHA" bin="${LOOM_DAEMON_BIN:-loom-daemon}" msg rc n=0 why; [[ -z "$phase" ]] || sha="${MERGE_PRECONDITION_SHA:-$PR_HEAD_SHA}"; while :; do n=$((n + 1)); rc=0; msg="$("${LOOM_DAEMON_BIN:-loom-daemon}" merge-pr ci-result --pr "$PR_NUMBER" --repo "$REPO_NWO" --head-sha "$sha")" || rc=$?; [[ $rc -eq 2 && "$msg" == "ci-result could not query the forge"* && $n -lt 3 ]] || break; warning "CI-run conclusion gate: forge read failed for PR #$PR_NUMBER (attempt $n/3), retrying: $msg" >&2; sleep "${LOOM_CI_RESULT_RETRY_DELAY:-10}"; done; [[ $rc -eq 0 && "$msg" == "LOOM-CI-RESULT-CLEAN" ]] && return 0; [[ $rc -eq 0 && "$msg" == "LOOM-CI-RESULT-NO-CI-WORKFLOW "* ]] && { info "PR #$PR_NUMBER: ${msg#LOOM-CI-RESULT-NO-CI-WORKFLOW } — CI-run conclusion gate not applicable (explicit no-CI policy, #10567)"; return 0; }; if [[ $rc -eq 3 || ( $rc -eq 0 && "$msg" == "LOOM-CI-RESULT-UNVERIFIED "* ) ]]; then [[ "$AUTO_MERGE" != "true" || -n "$phase" ]] || { info "PR #$PR_NUMBER: no concluded CI run for head ${sha:0:12} yet; --auto re-checks it after the settle-wait (#10567)"; return 0; }; why="Merge held: PR #$PR_NUMBER head $sha — the CI-run conclusion gate (#10444) has no verdict for this exact head: ${msg#LOOM-CI-RESULT-UNVERIFIED }. Unknown is not success (#10567): nothing was merged and loom:pr / the approved SHA are untouched. Re-attempt on a later pass once this head's \`CI\` run concludes; if none will ever start, re-run CI on this head (gh run rerun --failed <run> / gh workflow run CI --ref <branch>) rather than merging around it."; [[ $rc -ne 0 ]] || why="$why This loom-daemon predates #10567's no-CI/not-run distinction (it answered UNVERIFIED with exit 0), so a repo with no \`CI\` workflow is held too: roll this host past the release carrying #10567 (${SCRIPT_DIR:-.loom/scripts}/cli/loom-daemon-update.sh --fetch)."; [[ "$DRY_RUN" != "true" ]] || { warning "[dry-run] Would HOLD merge of PR #$PR_NUMBER (exit 5): $why"; return 0; }; warning "$why" >&2; warning "Exiting 5: not merged this pass (#10567). Re-attempt on a later pass." >&2; exit 5; fi; if [[ $rc -ne 1 ]]; then why="What it reported: $msg"; [[ -n "$msg" ]] || why="It printed nothing on stdout (see its stderr above), so the binary is most likely missing or predates 'merge-pr ci-result'."; [[ "$msg" == "ci-result could not query the forge"* ]] || why="$why $(_mp_daemon_roll_hint merge-pr "$(command -v "$bin" 2>/dev/null || true)")"; msg="Merge blocked: PR #$PR_NUMBER head $sha — the CI-run conclusion gate (#10444) could not establish this head's CI result: '$bin merge-pr ci-result' exited $rc after $n attempt(s) without a verdict. An unavailable verifier is not permission to merge (#10567); loom:pr and the approved SHA are untouched, so re-run this merge once the cause is fixed. $why"; fi; [[ "$DRY_RUN" != "true" ]] || { warning "[dry-run] Would BLOCK merge of PR #$PR_NUMBER: $msg"; return 0; }; error "$msg"; }
