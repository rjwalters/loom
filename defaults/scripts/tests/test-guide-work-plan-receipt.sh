#!/usr/bin/env bash
# test-guide-work-plan-receipt.sh - Regression test for issue #14204
#
# update_work_plan() reported its outcome only through `echo` + exit status
# (1 = current / debounced / provenance-refused, 0 = spliced), and
# create_docs_pr()'s docs-PR body was a fixed heredoc claiming
# "WORK_PLAN.md: Regenerated backlog snapshot from current GitHub label
# state" no matter what had actually happened. A debounced or
# provenance-refused tick therefore filed PRs whose body asserted a
# regeneration that never occurred, and any "unchanged / matches live
# labels" claim in a docs PR was free text written by the LLM with no
# machine-recorded receipt to contradict it.
#
# The #14204 fix makes the outcome machine-recorded:
#   - update_work_plan() classifies each of its return points into the global
#     WORK_PLAN_RECEIPT (`written` / `current` /
#     `debounced (elapsed <X>s < window <Y>s)` / `provenance-refused` /
#     `write-failed`), and fingerprints the rendered body into
#     WORK_PLAN_RENDER_HASH (first 12 hex of its sha256).
#   - The tick initializes WORK_PLAN_RECEIPT="NOT RUN" before calling, so a
#     tick that never reached the function is visible rather than silent.
#   - create_docs_pr() renders them verbatim as the PR body's
#     `- **WORK_PLAN check**: <receipt> (render-hash <hash>, <UTC time>)`
#     line — the body's ONLY plan-status claim — replacing the fixed bullet.
#
# Verifies that:
#   1. guide.md's update_work_plan() sets WORK_PLAN_RECEIPT at each of its
#      outcome branches and computes the render hash.
#   2. The NOT RUN default exists (documented tick initialization) and
#      create_docs_pr() falls back to NOT RUN / none when the tick never ran
#      the check.
#   3. create_docs_pr() renders the `WORK_PLAN check:` line with the
#      receipt, the render hash and a UTC timestamp, spliced into a body
#      template that no longer carries the old free-text status bullet.
#   4. The role text carries the no-free-text rule (Step 5 prose + the
#      phase's Constraints list).
#   5. THE RECEIPTS, executed rather than grepped: the exact assignment and
#      splice lines extracted VERBATIM from guide.md, evaluated in a sandbox
#      against fixture values, produce each classification — including NOT
#      RUN for a never-called tick.
#   6. The render-hash line computes the real sha256 prefix of the body.
#   7. The generated SKILL.md mirror is in sync (carries the receipt).
#
# Hermetic: greps + sandboxed eval of extracted lines. No forge, network,
# or `gh` calls.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# guide.md is shipped (installed at .claude/commands/loom/guide.md), so
# resolve it the way each layout actually lays it out: the installed path
# first (consumer repos, and Loom's own dogfooded checkout), falling back
# to the defaults/ source-tree path (a bare source checkout with no
# .claude/commands/loom/ copy yet). See issue #6194 / #6241.
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Local assert_eq kept: takes (actual, expected, msg); shared assert_eq is (expected, actual, msg).
assert_eq() {
    local actual="$1" expected="$2" msg="$3"
    if [[ "$actual" == "$expected" ]]; then pass "$msg"; else fail "$msg (got '$actual', expected '$expected')"; fi
}

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found at $GUIDE_MD"
    exit 1
fi

# ---------------------------------------------------------------------------
# Test 1: update_work_plan() records a receipt at each of its outcomes
# ---------------------------------------------------------------------------
echo "Test 1: update_work_plan() sets WORK_PLAN_RECEIPT at every outcome branch"

UWP_BODY="$(awk '/^update_work_plan\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ -n "$UWP_BODY" ]]; then
    pass "extracted update_work_plan() from guide.md"
else
    fail "could not extract update_work_plan() from guide.md"
fi

assert_contains "$UWP_BODY" 'record_work_plan_receipt "current"' \
    "the no-change fast path records receipt 'current'"
assert_contains "$UWP_BODY" 'record_work_plan_receipt "debounced (elapsed ${elapsed}s < window ${debounce_secs}s)"' \
    "the debounce suppression records receipt 'debounced' with elapsed vs window figures"
assert_contains "$UWP_BODY" 'record_work_plan_receipt "provenance-refused"' \
    "the provenance gate records receipt 'provenance-refused'"
assert_contains "$UWP_BODY" 'record_work_plan_receipt "written"' \
    "the successful splice records receipt 'written'"
assert_contains "$UWP_BODY" 'record_work_plan_receipt "write-failed"' \
    "a failed splice records receipt 'write-failed' instead of leaving a stale value"
assert_contains "$UWP_BODY" 'WORK_PLAN_RENDER_HASH="$(printf' \
    "update_work_plan() fingerprints the rendered body into WORK_PLAN_RENDER_HASH"
assert_contains "$UWP_BODY" 'cut -c1-12' \
    "the render hash is truncated to its first 12 hex characters"

# The receipt variables must NOT be declared local — they have to survive
# into Step 5's create_docs_pr().
if grep -q 'local WORK_PLAN_RECEIPT\|local WORK_PLAN_RENDER_HASH' <<<"$UWP_BODY"; then
    fail "WORK_PLAN_RECEIPT/WORK_PLAN_RENDER_HASH must not be local to update_work_plan() (Step 5 reads them)"
else
    pass "WORK_PLAN_RECEIPT/WORK_PLAN_RENDER_HASH are globals (not declared local)"
fi

# ---------------------------------------------------------------------------
# Test 2: the NOT RUN default — a tick that never ran the check is visible
# ---------------------------------------------------------------------------
echo ""
echo "Test 2: WORK_PLAN_RECEIPT defaults to NOT RUN before the check runs"

assert_grep 'WORK_PLAN_RECEIPT="NOT RUN"' "$GUIDE_MD" \
    "the tick is documented to initialize WORK_PLAN_RECEIPT to NOT RUN before calling update_work_plan()"

# ---------------------------------------------------------------------------
# Test 3: create_docs_pr() renders the machine-recorded check line
# ---------------------------------------------------------------------------
echo ""
echo "Test 3: create_docs_pr() renders the WORK_PLAN check receipt line"

CDP_BODY="$(awk '/^create_docs_pr\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ -n "$CDP_BODY" ]]; then
    pass "extracted create_docs_pr() from guide.md"
else
    fail "could not extract create_docs_pr() from guide.md"
fi

assert_contains "$CDP_BODY" '- **WORK_PLAN check**: ${wp_receipt:-NOT RUN} (render-hash ${wp_hash:-none}' \
    "the rendered line carries the receipt, a NOT RUN fallback and a render-hash fallback"
assert_contains "$CDP_BODY" "%Y-%m-%dT%H:%M:%SZ" \
    "the rendered line carries a UTC timestamp"
assert_contains "$CDP_BODY" '@@WORK_PLAN_RECEIPT_LINE@@' \
    "the body template carries the placeholder the receipt line is spliced into"
assert_contains "$CDP_BODY" 'DOCS_PR_BODY="${DOCS_PR_BODY/"@@WORK_PLAN_RECEIPT_LINE@@"' \
    "create_docs_pr() splices the receipt line into the quoted-heredoc body"

# THE REGRESSION: the old fixed status bullet must be gone from the PR body
# template — it claimed a regeneration that debounced/refused ticks never
# performed.
assert_not_contains "$CDP_BODY" '**WORK_PLAN.md**: Regenerated backlog snapshot from current GitHub label state' \
    "the old free-text 'Regenerated backlog snapshot' bullet is gone from the PR body"

# The PR body heredoc itself (between the quoted PRBODY markers): the
# placeholder must sit in the Changes list, and no other plan-status prose
# may have crept back in.
PR_HEREDOC_COUNT="$(grep -c "cat <<'PRBODY'" "$GUIDE_MD")"
assert_eq "$PR_HEREDOC_COUNT" "1" "guide.md has exactly one PRBODY heredoc (the docs PR body)"
PR_BODY_TEMPLATE="$(awk "/cat <<'PRBODY'/{flag=1; next} /^PRBODY$/{flag=0} flag" "$GUIDE_MD")"
assert_contains "$PR_BODY_TEMPLATE" '@@WORK_PLAN_RECEIPT_LINE@@' \
    "the quoted heredoc holds the receipt placeholder (the quoted delimiter cannot interpolate it inline)"

# ---------------------------------------------------------------------------
# Test 4: the no-free-text rule is stated in the role text
# ---------------------------------------------------------------------------
echo ""
echo "Test 4: the role text forbids plan-status claims outside the receipt line"

assert_grep "is the PR body's only plan-status" "$GUIDE_MD" \
    "Step 5's prose states the check line is the PR body's only plan-status claim"
assert_grep "states WORK_PLAN's status only through the" "$GUIDE_MD" \
    "the phase Constraints list carries the no-free-text rule (#14204)"
assert_grep 'free-text a claim like "WORK_PLAN.md is' "$GUIDE_MD" \
    "the rule names the forbidden free-text form explicitly"

# ---------------------------------------------------------------------------
# Test 5: THE RECEIPTS, executed — extracted lines evaluated verbatim
# ---------------------------------------------------------------------------
echo ""
echo "Test 5: extracted receipt lines, evaluated in a sandbox, classify each outcome"

# Extract the exact receipt-resolution block from create_docs_pr() (#14259: the
# globals-then-file resolution, the WORK_PLAN_CHECK_LINE assignment AND the
# DOCS_PR_BODY splice that consumes it), leading whitespace stripped so it
# evals at top level in the sandbox. `local` is not valid outside a function,
# so the sandbox wraps the block in one.
SPLICE_LINES="$(awk '/local wp_receipt=/{flag=1} flag{print} /DOCS_PR_BODY="\$\{DOCS_PR_BODY\/"@@WORK_PLAN_RECEIPT_LINE@@"/{if(flag){exit}}' "$GUIDE_MD" | sed 's/^[[:space:]]*//')"
if [[ "$(grep -c 'DOCS_PR_BODY="' <<<"$SPLICE_LINES")" -ge 1 && "$(grep -c 'WORK_PLAN_CHECK_LINE=' <<<"$SPLICE_LINES")" -ge 1 ]]; then
    pass "extracted the receipt resolution + WORK_PLAN_CHECK_LINE assignment + DOCS_PR_BODY splice from create_docs_pr()"
else
    fail "could not extract the receipt resolution + WORK_PLAN_CHECK_LINE assignment + DOCS_PR_BODY splice from create_docs_pr()"
fi

# The receipt helpers, extracted verbatim from guide.md for the sandbox.
RECORD_FN="$(awk '/^WORK_PLAN_RECEIPT_FILE_NAME=/{print} /^record_work_plan_receipt\(\) \{/{flag=1} flag{print} /^\}/{flag=0}' "$GUIDE_MD")"
READ_FN="$(awk '/^read_work_plan_receipt_field\(\) \{/{flag=1} flag{print} /^\}/{flag=0}' "$GUIDE_MD")"
if [[ -n "$RECORD_FN" && -n "$READ_FN" ]]; then
    pass "extracted record_work_plan_receipt()/read_work_plan_receipt_field() from guide.md"
else
    fail "could not extract the receipt helper functions from guide.md"
fi

run_pr_body_splice() {
    # $1 = WORK_PLAN_RECEIPT value ("__unset__" = leave unset, the
    #      never-called tick), $2 = WORK_PLAN_RENDER_HASH (same sentinel),
    # $3 = fixture PR body template, $4 = optional receipt-file content
    # (printf-interpreted; empty/absent = no file). Prints the spliced
    # DOCS_PR_BODY.
    local receipt="$1" hash="$2" body="$3" file_content="${4:-}"
    local SANDBOX
    SANDBOX="$(mktemp -d)"
    mkdir -p "$SANDBOX/wt"
    if [ -n "$file_content" ]; then
        printf "$file_content" > "$SANDBOX/wt/.loom-work-plan-receipt"
    fi
    {
        printf 'DOCS_WT=%q\n' "$SANDBOX/wt"
        printf '%s\n' "$RECORD_FN" "$READ_FN"
        if [ "$receipt" != "__unset__" ]; then printf 'WORK_PLAN_RECEIPT=%q\n' "$receipt"; fi
        if [ "$hash" != "__unset__" ]; then printf 'WORK_PLAN_RENDER_HASH=%q\n' "$hash"; fi
        printf 'DOCS_PR_BODY=%q\n' "$body"
        printf 'f() {\n%s\n}\nf\n' "$SPLICE_LINES"
        printf '%s\n' 'printf "%s\n" "$DOCS_PR_BODY"'
    } > "$SANDBOX/run.sh"
    bash "$SANDBOX/run.sh"
    rm -rf "$SANDBOX"
}

FIXTURE_TEMPLATE='### Changes
- **WORK_LOG.md**: entries
@@WORK_PLAN_RECEIPT_LINE@@
- **README.md**: n/a'

# 5a. THE NEVER-CALLED TICK: both receipt variables unset — the line must
# read NOT RUN / none, visible rather than silent.
RESULT="$(run_pr_body_splice "__unset__" "__unset__" "$FIXTURE_TEMPLATE")"
assert_contains "$RESULT" '- **WORK_PLAN check**: NOT RUN (render-hash none,' \
    "a tick that never called update_work_plan() renders 'NOT RUN (render-hash none, ...)'"
assert_not_contains "$RESULT" '@@WORK_PLAN_RECEIPT_LINE@@' \
    "the placeholder is fully consumed by the splice"

# 5b. A debounced tick: the classification and the refused render's hash
# surface verbatim in the line.
RESULT="$(run_pr_body_splice "debounced (elapsed 300s < window 3600s)" "6c47a827e98f" "$FIXTURE_TEMPLATE")"
assert_contains "$RESULT" '- **WORK_PLAN check**: debounced (elapsed 300s < window 3600s) (render-hash 6c47a827e98f,' \
    "a debounced tick's receipt and render hash render verbatim into the PR body"

# 5c. The debounced receipt assignment, extracted from update_work_plan(),
# composes its elapsed/window figures from the live arithmetic variables.
DEBOUNCED_LINE="$(grep -m1 'record_work_plan_receipt "debounced' "$GUIDE_MD" | sed 's/^[[:space:]]*//')"
SANDBOX="$(mktemp -d)"
{
    printf 'DOCS_WT=%q\n' "$SANDBOX"
    printf '%s\n' "$RECORD_FN"
    printf 'elapsed=300\ndebounce_secs=3600\n'
    printf '%s\n' "$DEBOUNCED_LINE"
    printf '%s\n' 'printf "%s\n" "$WORK_PLAN_RECEIPT"'
} > "$SANDBOX/run.sh"
RESULT="$(bash "$SANDBOX/run.sh")"
assert_eq "$RESULT" "debounced (elapsed 300s < window 3600s)" \
    "the extracted debounced assignment composes elapsed/window from the tick's arithmetic"
rm -rf "$SANDBOX"

# 5d. (#14259) SEPARATE SHELL INVOCATIONS: Step 3 runs in one process, Step 5
# in another, so no global survives. The receipt must still arrive populated.
RESULT="$(run_pr_body_splice "__unset__" "__unset__" "$FIXTURE_TEMPLATE" 'receipt=written\nrender_hash=6c47a827e98f\n')"
assert_contains "$RESULT" '- **WORK_PLAN check**: written (render-hash 6c47a827e98f,' \
    "unset globals + receipt file => the PR body line is populated from the file"

# 5e. The tick's `NOT RUN` initialization is not an outcome: it must not mask a
# real receipt file.
RESULT="$(run_pr_body_splice "NOT RUN" "__unset__" "$FIXTURE_TEMPLATE" 'receipt=current\nrender_hash=abcdef012345\n')"
assert_contains "$RESULT" '- **WORK_PLAN check**: current (render-hash abcdef012345,' \
    "a NOT RUN initialization global does not mask a receipt file"

# 5f. Absent file + unset globals keeps the visible NOT RUN fallback.
RESULT="$(run_pr_body_splice "__unset__" "__unset__" "$FIXTURE_TEMPLATE")"
assert_contains "$RESULT" '- **WORK_PLAN check**: NOT RUN (render-hash none,' \
    "absent receipt file => NOT RUN (render-hash none, ...)"

# 5g. Globals still win when set (single-shell execution unchanged).
RESULT="$(run_pr_body_splice "written" "111111111111" "$FIXTURE_TEMPLATE" 'receipt=current\nrender_hash=222222222222\n')"
assert_contains "$RESULT" '- **WORK_PLAN check**: written (render-hash 111111111111,' \
    "a global carrying a real outcome wins over the file"

# 5h. A debounced classification (contains parens and '<') survives the file.
RESULT="$(run_pr_body_splice "__unset__" "__unset__" "$FIXTURE_TEMPLATE" 'receipt=debounced (elapsed 300s < window 3600s)\nrender_hash=6c47a827e98f\n')"
assert_contains "$RESULT" '- **WORK_PLAN check**: debounced (elapsed 300s < window 3600s) (render-hash 6c47a827e98f,' \
    "a debounced receipt round-trips through the file intact"

# 5i. The writer, run as its OWN process, then the reader in a SECOND process.
SANDBOX="$(mktemp -d)"
{
    printf 'DOCS_WT=%q\n' "$SANDBOX"
    printf '%s\n' "$RECORD_FN"
    printf 'WORK_PLAN_RENDER_HASH=cafe00112233\n'
    printf '%s\n' 'record_work_plan_receipt "write-failed"'
} > "$SANDBOX/step3.sh"
{
    printf 'DOCS_WT=%q\n' "$SANDBOX"
    printf '%s\n' "$RECORD_FN" "$READ_FN"
    printf '%s\n' 'printf "%s|%s|%s\n" "${WORK_PLAN_RECEIPT:-unset}" "$(read_work_plan_receipt_field receipt)" "$(read_work_plan_receipt_field render_hash)"'
} > "$SANDBOX/step5.sh"
bash "$SANDBOX/step3.sh"
RESULT="$(bash "$SANDBOX/step5.sh")"
assert_eq "$RESULT" "unset|write-failed|cafe00112233" \
    "write-failed recorded by one process is recovered by a second process"

# 5j. The file is never staged: the staging paths are exact names, none of
# which is the receipt file, and no blanket add exists in create_docs_pr().
CDP_CODE="$(grep -v '^[[:space:]]*#' <<<"$CDP_BODY")"
assert_not_contains "$CDP_CODE" 'add -A' "create_docs_pr() never blanket-stages (comments excluded)"
assert_not_contains "$CDP_BODY" '.loom-work-plan-receipt' \
    "create_docs_pr() never stages the receipt file by name"

# 5k. Entry reset: update_work_plan() removes any stale file BEFORE its first
# early return (the skipDocs one), so a stale file cannot leak.
FIRST_RM_LINE="$(grep -n 'rm -f "$DOCS_WT/$WORK_PLAN_RECEIPT_FILE_NAME"' <<<"$UWP_BODY" | head -1 | cut -d: -f1)"
FIRST_RETURN_LINE="$(grep -n 'return 1' <<<"$UWP_BODY" | head -1 | cut -d: -f1)"
# The skipDocs guard must stay the function's first statement (test-guide-skip-docs.sh),
# so the reset lives INSIDE that branch (before its `return 1`) AND right after it.
RM_COUNT="$(grep -c 'rm -f "$DOCS_WT/$WORK_PLAN_RECEIPT_FILE_NAME"' <<<"$UWP_BODY")"
if [[ "$RM_COUNT" -ge 2 && -n "$FIRST_RM_LINE" && -n "$FIRST_RETURN_LINE" && "$FIRST_RM_LINE" -lt "$FIRST_RETURN_LINE" ]]; then
    pass "update_work_plan() deletes the stale receipt file before the skipDocs early return and again after it"
else
    fail "update_work_plan() must delete the stale receipt file before its first early return (rm at ${FIRST_RM_LINE:-none}, return at ${FIRST_RETURN_LINE:-none})"
fi

# ---------------------------------------------------------------------------
# Test 6: the render-hash line computes the real sha256 prefix
# ---------------------------------------------------------------------------
echo ""
echo "Test 6: WORK_PLAN_RENDER_HASH is the first 12 hex of the body's sha256"

HASH_LINE="$(grep -m1 'WORK_PLAN_RENDER_HASH="$(printf' "$GUIDE_MD" | sed 's/^[[:space:]]*//')"
SANDBOX="$(mktemp -d)"
FIXTURE_RENDER='## Ready

- **#1234**: fixture issue
'
{
    printf 'new_body=%q\n' "$FIXTURE_RENDER"
    printf '%s\n' "$HASH_LINE"
    printf '%s\n' 'printf "%s\n" "$WORK_PLAN_RENDER_HASH"'
} > "$SANDBOX/run.sh"
RESULT="$(bash "$SANDBOX/run.sh")"
EXPECTED_HASH="$(printf '%s\n' "$FIXTURE_RENDER" | { sha256sum 2>/dev/null || shasum -a 256; } | cut -c1-12)"
assert_eq "$RESULT" "$EXPECTED_HASH" \
    "the extracted hash assignment equals an independently computed 12-hex sha256 prefix"
rm -rf "$SANDBOX"

# ---------------------------------------------------------------------------
# Test 7: the generated SKILL.md mirror is in sync (#14204)
# ---------------------------------------------------------------------------
echo ""
echo "Test 7: the loom-guide SKILL.md mirror carries the receipt contract"

# Same installed-first resolution as guide.md above: the skill mirror ships
# under defaults/, and a consumer repo's installed copy (if it has one) sits
# at .agents/skills/. Neither layout present -> nothing to assert here.
if [[ -f "$REPO_ROOT/.agents/skills/loom-guide/SKILL.md" ]]; then
    SKILL_MD="$REPO_ROOT/.agents/skills/loom-guide/SKILL.md"
elif [[ -f "$REPO_ROOT/defaults/.agents/skills/loom-guide/SKILL.md" ]]; then
    SKILL_MD="$REPO_ROOT/defaults/.agents/skills/loom-guide/SKILL.md"
else
    echo "SKIP: no loom-guide SKILL.md mirror in this layout" >&2
    SKILL_MD=""
fi
if [[ -n "$SKILL_MD" ]]; then
    assert_grep 'WORK_PLAN_RECEIPT' "$SKILL_MD" \
        "the generated SKILL.md mirror was regenerated with the receipt variable"
    assert_grep 'WORK_PLAN check' "$SKILL_MD" \
        "the generated SKILL.md mirror was regenerated with the check line"
fi

# ---------------------------------------------------------------------------
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
