#!/usr/bin/env bash
# test-ci-wait-prompt-consistency.sh - Prompt-lint for issue #10574.
#
# Judge and Doctor must agree about CI waits:
#   - Judge takes one bounded, settled CI snapshot before review. Pending CI
#     and a moved head both use that result to release the claim.
#   - The longer foreground wait is available to an attended Judge.
#   - Doctor hands back to Judge on pending CI after a push; the bounded
#     --timeout 1200 foreground wait is only for an invocation that
#     explicitly requires the settled result.
#   - An unreadable required-context lookup is unknown, never green.
#
# Hermetic: reads files only - no forge, network, or loom-daemon binary.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
source "$SCRIPT_DIR/lib/assert.sh"

judge_section() {
    awk -v heading="$2" '
        $0 == heading { found = 1; next }
        found && /^### / { exit }
        found
    ' "$1"
}

check() {
    local J="$1" D="$2" where="$3"
    for f in "$J" "$D"; do
        [[ -f "$f" ]] || { echo -e "${RED}FAIL${NC}: missing $f" >&2; exit 1; }
    done

    # No-checks repo: no executable --timeout 0 wait-checks call remains.
    if grep -E 'wait-checks .*--timeout 0( |$)' "$J" | grep -vq '^\*\*Step 1'; then
        fail "$where judge: no wait-checks call uses --timeout 0"
    else
        pass "$where judge: no wait-checks call uses --timeout 0"
    fi
    local precheck snapshot snapshot_count pending release status paths
    precheck="$(grep -F '2b. **CI pre-check' "$J")"
    snapshot="$(grep -oE 'loom-daemon forge wait-checks [^|`]*' <<< "$precheck")"
    snapshot_count="$(grep -oF 'loom-daemon forge wait-checks' <<< "$precheck" | wc -l)"
    if [[ "$snapshot_count" -eq 1 && " $snapshot " == *' --timeout 20 '* && " $snapshot " == *' --settle-red '* ]]; then
        pass "$where judge: pre-check takes one bounded settled snapshot"
    else
        fail "$where judge: pre-check takes one bounded settled snapshot"
    fi
    assert_grep 'LOOM-CHECKS-TIMEOUT.*HEAD-MOVED.*When CI is Pending' <(printf '%s\n' "$precheck") \
        "$where judge: TIMEOUT and HEAD-MOVED route to pending-CI handback"

    pending="$(judge_section "$J" '### When CI is Pending')"
    assert_grep '([Uu]se|[Rr]euse) [^.!?]*(result|snapshot)' <(printf '%s\n' "$pending") \
        "$where judge: handback uses the triggering snapshot"
    assert_no_grep 'forge wait-checks|gh pr checks' <(printf '%s\n' "$pending") \
        "$where judge: handback makes no new CI poll"
    assert_contains "$pending" 'LOOM-CHECKS-HEAD-MOVED' "$where judge: a moved head uses the generic release"
    assert_contains "$pending" 'loom:review-requested' "$where judge: handback leaves the PR queued"
    release="$(awk '/^```bash$/ { code=1; next } code && /^```$/ { exit } code' <<< "$pending")"
    assert_grep '^[[:space:]]*gh pr edit .*--remove-label "?loom:reviewing"?([[:space:]]|$)' \
        <(printf '%s\n' "$release") "$where judge: generic handback removes the review claim label"
    assert_grep '^[[:space:]]*loom-daemon lease pr release .*--role judge .*--skip-reason ci-pending([[:space:]]|$)' \
        <(printf '%s\n' "$release") "$where judge: generic handback releases the lease as ci-pending"
    assert_no_grep 'post-verdict\.sh|--add-label "?loom:(pr|changes-requested)"?([[:space:]]|$)' \
        <(printf '%s\n' "$release") \
        "$where judge: generic handback writes no verdict"

    status="$(judge_section "$J" '### How to Check CI Status')"
    assert_grep 'LOOM-CHECKS-GREEN.*(approve|continue)' <(printf '%s\n' "$status") \
        "$where judge: GREEN allows review to proceed"
    assert_grep 'RED.*When CI Fails' <(printf '%s\n' "$status") \
        "$where judge: settled RED routes to CI failure handling"
    assert_grep 'ERROR.*gh pr checks.*once' <(printf '%s\n' "$status") \
        "$where judge: missing or error sentinel has a single-check fallback"

    paths="$(judge_section "$J" '### CRITICAL: Never End Your Turn on a Background CI Monitor')"
    assert_contains "$paths" 'class=noop reason=ci_pending' "$where judge: daemon handback ends with ci_pending"
    assert_grep 'Attended / manual single-PR invocation \(no .*step-judge-' <(printf '%s\n' "$paths") \
        "$where judge: foreground path is limited to attended single-PR invocations"
    assert_contains "$paths" 'forge wait-checks <PR_NUMBER> --timeout 1800 --settle-red' \
        "$where judge: bounded attended foreground wait is retained"

    # No-checks PRs (#13883): -NONE is not "safe to approve" on its own; Judge
    # runs the repo's documented test command itself, or the verdict records
    # "unverified by any test".
    if grep -qF '`LOOM-CHECKS-GREEN` / `-NONE` (no checks) - Safe to approve' "$J"; then
        fail "$where judge: -NONE must not share the -GREEN 'safe to approve' bullet"
    else
        pass "$where judge: -NONE must not share the -GREEN 'safe to approve' bullet"
    fi
    grep -qF -- 'LOOM-CHECKS-NONE` (no checks) - Continue the review' "$J" \
        && pass "$where judge: -NONE has its own continue-the-review bullet" \
        || fail "$where judge: -NONE has its own continue-the-review bullet"
    grep -qF 'No-Checks PRs (`LOOM-CHECKS-NONE`): no test ever ran' "$J" \
        && pass "$where judge: no-checks rule present (run the documented test)" \
        || fail "$where judge: no-checks rule present (run the documented test)"
    grep -qF 'unverified by any test' "$J" \
        && pass "$where judge: undocumented repo → verdict says 'unverified by any test'" \
        || fail "$where judge: undocumented repo → verdict says 'unverified by any test'"
    grep -qF 'unverified by any test: only documented command is `<cmd>` (full suite, out of bounds per #11690)' "$J" \
        && pass "$where judge: full-suite-only documented repo → verdict names the out-of-bounds documented command" \
        || fail "$where judge: full-suite-only documented repo → verdict names the out-of-bounds documented command"
    local fsrow
    fsrow="$(grep -F '| **Full suite / build gate** |' "$J")"
    grep -qF 'No-Checks PRs' <<< "$fsrow" \
        && pass "$where judge: Full-suite/build-gate row points -NONE at the no-checks fallback" \
        || fail "$where judge: Full-suite/build-gate row points -NONE at the no-checks fallback"

    # Unknown required contexts.
    grep -qF 'Unknown is never green.' "$J" && grep -qF 'required-contexts-unknown' "$J" \
        && pass "$where judge: unreadable required contexts are unknown, never green" \
        || fail "$where judge: unreadable required contexts are unknown, never green"
    grep -qF 'leaves the result *unknown*, never green' "$D" \
        && pass "$where doctor: unreadable required contexts are unknown" \
        || fail "$where doctor: unreadable required contexts are unknown"

    # Pending CI after a Doctor push: Step 10 hands back, does not point at the 1200s wait.
    local s10
    s10="$(grep -F '10. **Verify CI remotely**' "$D")"
    if grep -qF 'else CI Assessment Step 5' <<< "$s10"; then
        fail "$where doctor: step 10 no longer routes not-green to the 1200s wait"
    else
        pass "$where doctor: step 10 no longer routes not-green to the 1200s wait"
    fi
    grep -qF 'hand back to Judge in step 11' <<< "$s10" \
        && pass "$where doctor: step 10 hands pending CI back to Judge" \
        || fail "$where doctor: step 10 hands pending CI back to Judge"

    # CI Assessment Step 5: 1200 is not the unconditional call; attended path retained.
    local step5
    step5="$(awk '/^### Step 5: Verify Remote CI After Push/{f=1;next} /^### /{f=0} f' "$D")"
    if grep -E '^loom-daemon forge wait-checks .*--timeout 1200' <<< "$step5" | grep -q .; then
        fail "$where doctor: step 5 has no unconditional --timeout 1200"
    else
        pass "$where doctor: step 5 has no unconditional --timeout 1200"
    fi
    grep -qF 'explicitly requires the settled result' <<< "$step5" \
        && pass "$where doctor: step 5 keeps the explicit attended-wait path" \
        || fail "$where doctor: step 5 keeps the explicit attended-wait path"
    grep -qF -- '--timeout 1200 2>"$err"' "$D" \
        && pass "$where doctor: bounded foreground wait recipe retained" \
        || fail "$where doctor: bounded foreground wait recipe retained"
}

C="$REPO_ROOT/defaults/.claude/commands/loom"
S="$REPO_ROOT/defaults/.agents/skills"
check "$C/judge.md" "$C/doctor.md" "commands"
check "$S/loom-judge/SKILL.md" "$S/loom-doctor/SKILL.md" "skills"

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed"
exit 0
