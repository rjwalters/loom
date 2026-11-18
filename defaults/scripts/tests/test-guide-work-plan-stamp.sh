#!/usr/bin/env bash
# test-guide-work-plan-stamp.sh - Regression test for issue #13909
#
# The committed WORK_PLAN.md generated region carried no render date: a reader
# comparing rows against live labels could not tell a fresh snapshot from a
# days-old one without a forge query (measured instance, genericized per
# check-vendored-private-refs: example-org/tool-repo#202 -- the committed
# region listed five closed issues as open, undated). The durable fix is
# STAMP ON WRITE, never stamp on render:
#
#   1. plan_stamp_line() emits the fixed-shape stamp line. It is called ONLY
#      by the splice step at the bottom of update_work_plan(), never by
#      render_plan_body().
#   2. strip_plan_stamp() conditionally drops that line from old_body (line 1
#      only, never an unconditional `1d`), so the new_body == old_body
#      comparison stays purely about label-derived content.
#   3. The stamp is prepended only after BOTH the no-change skip branch and
#      the #5890 debounce gate pass, so it never refreshes on an unchanged
#      tick and no PR's only diff can be a timestamp.
#
# The regression this suite guards against is the #5413 dead-skip bug
# replayed through wall-clock text: had the stamp been emitted inside
# render_plan_body(), new_body would differ from old_body on every tick, the
# skip branch would go dead, and each debounce window would manufacture one
# content-free docs PR whose entire diff is a timestamp (the #5930/#5890
# churn class).
#
# Verifies that (grep + executed fixtures, mirroring
# test-guide-work-plan-debounce.sh's extraction style):
#   1. guide.md defines plan_stamp_line() and strip_plan_stamp(), and
#      update_work_plan()'s old_body pipeline pipes through strip_plan_stamp.
#   2. THE #5413 GUARD: render_plan_body()'s body neither calls
#      plan_stamp_line() nor contains the stamp text -- the render side stays
#      wall-clock-free.
#   3. Executed: plan_stamp_line() emits the exact fixed shape.
#   4. Executed: strip_plan_stamp() against fixtures -- a stamped body strips
#      to the bare body; a stamp-less legacy body passes through
#      byte-identical; empty stays empty; a non-matching first line is kept;
#      a stamp-shaped line NOT on line 1 is kept.
#   5. THE REGRESSION, executed: a reconstruction of update_work_plan()'s
#      extraction + normalization + comparison + debounce against real
#      fixture WORK_PLAN.md files --
#        a. committed region with an OLD stamp + identical render => skip
#           ("current", no write): a stale stamp alone must never produce a
#           PR whose only diff is the refreshed timestamp.
#        b. committed region with NO stamp (pre-#13909 legacy) + identical
#           render => skip: normalization must not eat the legacy region's
#           real first line.
#        c. changed render, debounce elapsed => write, and the spliced
#           region's FIRST line is a fresh stamp with the new body after it;
#           re-extracting + re-stripping the spliced file yields exactly the
#           rendered body (round-trip).
#        d. changed render INSIDE the debounce window => no write -- the
#           stamp must not offer a side door around the #5890 gate.
#        e. first-ever generation (no markers at all) => write-worthy.
#   6. The stamp prepend sits AFTER the debounce gate inside
#      update_work_plan() (byte-offset order check on the extracted function
#      body, comments included -- the splice step is a comment there).
#   7. The WORK_PLAN.md header and the imagine.md WORK_PLAN template point
#      readers at the stamp, keeping the #13894 snapshot-semantics wording
#      and only adding the stamp pointer.
#
# Hermetic: pure grep/awk/sed against fixture files. No forge, network, or
# `gh` calls.

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
WORK_PLAN_MD="$REPO_ROOT/WORK_PLAN.md"
IMAGINE_MD="$REPO_ROOT/defaults/.claude/commands/loom/imagine.md"

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

# Extract the two #13909 helpers VERBATIM from guide.md and define them in
# this shell, so the test executes exactly what the prompt ships (same
# extraction discipline as test-guide-work-plan-debounce.sh's jq filter).
plan_stamp_line_def="$(awk '/^plan_stamp_line\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
strip_plan_stamp_def="$(awk '/^strip_plan_stamp\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"

# ---------------------------------------------------------------------------
# Test 1: guide.md defines the helpers and wires strip_plan_stamp into the
# old_body extraction pipeline
# ---------------------------------------------------------------------------
echo "Test 1: guide.md defines and wires the #13909 stamp helpers"

assert_grep 'plan_stamp_line\(\) \{' "$GUIDE_MD" \
    "plan_stamp_line() is defined"
assert_grep 'strip_plan_stamp\(\) \{' "$GUIDE_MD" \
    "strip_plan_stamp() is defined"
assert_grep '\| strip_plan_stamp\)"' "$GUIDE_MD" \
    "update_work_plan()'s old_body pipeline pipes through strip_plan_stamp"
assert_grep 'update_work_plan\(\) \{' "$GUIDE_MD" \
    "update_work_plan() is defined"

if [[ -n "$plan_stamp_line_def" ]]; then
    pass "extracted plan_stamp_line() verbatim from guide.md"
else
    fail "could not extract plan_stamp_line() from guide.md"
fi
if [[ -n "$strip_plan_stamp_def" ]]; then
    pass "extracted strip_plan_stamp() verbatim from guide.md"
else
    fail "could not extract strip_plan_stamp() from guide.md"
fi

eval "$plan_stamp_line_def"
eval "$strip_plan_stamp_def"

if command -v plan_stamp_line >/dev/null 2>&1; then
    pass "plan_stamp_line() evals to a callable function"
else
    fail "plan_stamp_line() did not eval to a callable function"
fi
if command -v strip_plan_stamp >/dev/null 2>&1; then
    pass "strip_plan_stamp() evals to a callable function"
else
    fail "strip_plan_stamp() did not eval to a callable function"
fi

# ---------------------------------------------------------------------------
# Test 2: THE #5413 GUARD -- render_plan_body() stays wall-clock-free
# ---------------------------------------------------------------------------
echo ""
echo "Test 2: render_plan_body() never emits or references the stamp"

RPB_BODY="$(awk '/^render_plan_body\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ -n "$RPB_BODY" ]]; then
    pass "extracted render_plan_body()'s body from guide.md"
else
    fail "could not extract render_plan_body()'s body from guide.md"
fi
assert_not_contains "$RPB_BODY" "plan_stamp_line" \
    "render_plan_body() does not call plan_stamp_line() (stamp on write, not on render)"
assert_not_contains "$RPB_BODY" "_Generated" \
    "render_plan_body() contains no stamp text"

# ---------------------------------------------------------------------------
# Test 3: executed -- plan_stamp_line() emits the exact fixed shape
# ---------------------------------------------------------------------------
echo ""
echo "Test 3: plan_stamp_line() emits the fixed stamp shape"

STAMP_OUT="$(plan_stamp_line)"
if printf '%s\n' "$STAMP_OUT" | grep -Eq '^_Generated: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z \(snapshot; compare against live labels\)_$'; then
    pass "plan_stamp_line() matches '^_Generated: <UTC ISO-8601> (snapshot; compare against live labels)_'"
else
    fail "plan_stamp_line() emitted a malformed stamp: '$STAMP_OUT'"
fi

# ---------------------------------------------------------------------------
# Test 4: executed -- strip_plan_stamp() against direct fixtures
# ---------------------------------------------------------------------------
echo ""
echo "Test 4: strip_plan_stamp() normalizes exactly the stamp line"

REGION_BODY='## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#42**: demo fixture issue'
STALE_STAMP='_Generated: 2026-11-01T00:00:00Z (snapshot; compare against live labels)_'

# A stamped committed region strips back to the bare body (the round-trip
# that keeps the equality test purely about label-derived content).
OUT="$(printf '%s\n%s\n' "$STALE_STAMP" "$REGION_BODY" | strip_plan_stamp)"
assert_eq "$OUT" "$REGION_BODY" \
    "stamp line 1 + body strips to the bare body"

# A pre-#13909 region with no stamp passes through byte-identical: the
# line-1 delete is conditional, so a legacy region's real first line (the
# first section heading) is never eaten.
OUT="$(printf '%s\n' "$REGION_BODY" | strip_plan_stamp)"
assert_eq "$OUT" "$REGION_BODY" \
    "a stamp-less legacy body passes through byte-identical"

# Empty input (first-ever region, or a missing file) stays empty.
OUT="$(printf '' | strip_plan_stamp)"
assert_eq "$OUT" "" \
    "empty input stays empty"

# A first line that merely resembles prose with underscores is kept.
OUT="$(printf '_None._\nsecond line\n' | strip_plan_stamp)"
assert_eq "$OUT" "$(printf '_None._\nsecond line')" \
    "a non-matching first line is kept"

# A stamp-shaped line NOT on line 1 is kept -- only the region's first line
# is ever a stamp.
OUT="$(printf '%s\n%s\n' "## Ready" "$STALE_STAMP" | strip_plan_stamp)"
assert_eq "$OUT" "$(printf '%s\n%s' "## Ready" "$STALE_STAMP")" \
    "a stamp-shaped line not on line 1 is kept"

# ---------------------------------------------------------------------------
# Test 5: THE REGRESSION, executed -- the full tick decision against fixture
# WORK_PLAN.md files, reconstructed exactly as update_work_plan() performs
# it (extraction pipeline verbatim, debounce arithmetic as in
# test-guide-work-plan-debounce.sh). $1 = rendered body, $2 = plan file,
# $3 = last_merged_epoch, $4 = now_epoch, $5 = debounce_secs,
# $6 = output file (written only on a WRITE decision, mimicking "a
# suppressed or unchanged tick writes nothing").
# ---------------------------------------------------------------------------
work_plan_tick() {
    local rendered="$1" plan_file="$2" last_merged_epoch="$3" now_epoch="$4" debounce_secs="$5" out_file="$6"
    local new_body old_body
    new_body="$rendered"
    old_body="$(sed -n '/<!-- guide:plan-body:start -->/,/<!-- guide:plan-body:end -->/p' \
        "$plan_file" 2>/dev/null | sed '1d;$d' | strip_plan_stamp)"

    if [ "$new_body" = "$old_body" ]; then
        echo "SKIP-current"
        return 0
    fi

    local elapsed
    elapsed=$(( now_epoch - last_merged_epoch ))
    if [ "$last_merged_epoch" -gt 0 ] && [ "$elapsed" -lt "$debounce_secs" ]; then
        echo "SKIP-debounce"
        return 0
    fi

    # The #13909 splice: region first line is a fresh "$(plan_stamp_line)",
    # directly above the new body, markers and everything outside untouched.
    local stamp
    stamp="$(plan_stamp_line)"
    awk -v stamp="$stamp" -v body="$new_body" '
        /^<!-- guide:plan-body:start -->$/ { print; print stamp; print body; skip=1; next }
        /^<!-- guide:plan-body:end -->$/   { skip=0; print; next }
        !skip { print }
    ' "$plan_file" > "$out_file"
    echo "WRITE"
}

PLAN_HEADER='# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

The body below is a snapshot of GitHub label state as of the last merged docs-maintenance PR — it may lag live labels until the next one merges. The `_Generated:` stamp at the top of the generated region dates the snapshot; `git log -1 -- WORK_PLAN.md` finds the merge that wrote it.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->
'

FIXTURE_DIR="$(mktemp -d)"
cleanup_fixtures() { rm -rf "$FIXTURE_DIR"; }
trap cleanup_fixtures EXIT

PLAN_STAMPED="$FIXTURE_DIR/WORK_PLAN.stamped.md"
PLAN_LEGACY="$FIXTURE_DIR/WORK_PLAN.legacy.md"
PLAN_UNMARKED="$FIXTURE_DIR/WORK_PLAN.unmarked.md"

{   printf '%s\n\n' "$PLAN_HEADER"
    printf '<!-- guide:plan-body:start -->\n'
    printf '%s\n' "$STALE_STAMP"
    printf '%s\n' "$REGION_BODY"
    printf '<!-- guide:plan-body:end -->\n'
} > "$PLAN_STAMPED"

{   printf '%s\n\n' "$PLAN_HEADER"
    printf '<!-- guide:plan-body:start -->\n'
    printf '%s\n' "$REGION_BODY"
    printf '<!-- guide:plan-body:end -->\n'
} > "$PLAN_LEGACY"

printf '%s\n' "$PLAN_HEADER" > "$PLAN_UNMARKED"

CHANGED_BODY='## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#42**: demo fixture issue
- **#43**: a second issue merged since the last snapshot'

DEBOUNCE=3600
LAST_MERGE=1000000
ELAPSED_TICK=$((LAST_MERGE + DEBOUNCE + 1))
INSIDE_TICK=$((LAST_MERGE + 300))

echo ""
echo "Test 5a: an old stamp + identical render skips (no PR whose only diff is the stamp)"

OUT_FILE="$FIXTURE_DIR/out-5a.md"
rm -f "$OUT_FILE"
DECISION="$(work_plan_tick "$REGION_BODY" "$PLAN_STAMPED" "$LAST_MERGE" "$ELAPSED_TICK" "$DEBOUNCE" "$OUT_FILE")"
assert_eq "$DECISION" "SKIP-current" \
    "a stale stamp with unchanged label-derived content is 'current'"
if [[ ! -e "$OUT_FILE" ]]; then
    pass "the no-change tick with a stale stamp wrote nothing (worktree stays clean)"
else
    fail "the no-change tick with a stale stamp wrote a file -- a stamp-only diff PR"
fi

echo ""
echo "Test 5b: a stamp-less legacy region + identical render also skips"

OUT_FILE="$FIXTURE_DIR/out-5b.md"
rm -f "$OUT_FILE"
DECISION="$(work_plan_tick "$REGION_BODY" "$PLAN_LEGACY" "$LAST_MERGE" "$ELAPSED_TICK" "$DEBOUNCE" "$OUT_FILE")"
assert_eq "$DECISION" "SKIP-current" \
    "a pre-#13909 region with unchanged content compares equal after normalization"

echo ""
echo "Test 5c: a changed render (debounce elapsed) writes a fresh-stamped region"

OUT_FILE="$FIXTURE_DIR/out-5c.md"
rm -f "$OUT_FILE"
DECISION="$(work_plan_tick "$CHANGED_BODY" "$PLAN_STAMPED" "$LAST_MERGE" "$ELAPSED_TICK" "$DEBOUNCE" "$OUT_FILE")"
assert_eq "$DECISION" "WRITE" \
    "a genuine label-state change past the debounce window writes"

REGION_FIRST_LINE="$(sed -n '/<!-- guide:plan-body:start -->/,/<!-- guide:plan-body:end -->/p' "$OUT_FILE" | sed '1d;$d' | head -1)"
if printf '%s\n' "$REGION_FIRST_LINE" | grep -Eq '^_Generated: [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z \(snapshot; compare against live labels\)_$'; then
    pass "the spliced region's first line is a fresh generated-at stamp"
else
    fail "the spliced region's first line is not a stamp: '$REGION_FIRST_LINE'"
fi
if [[ "$REGION_FIRST_LINE" != "$STALE_STAMP" ]]; then
    pass "the written stamp is fresh, not the stale committed one carried forward"
else
    fail "the written stamp is byte-identical to the stale committed stamp"
fi

# Round-trip: re-extracting + re-stripping the spliced region must yield
# exactly the rendered body -- the normalization and the splice are inverse
# operations, or the NEXT tick would manufactured a phantom diff.
ROUNDTRIP="$(sed -n '/<!-- guide:plan-body:start -->/,/<!-- guide:plan-body:end -->/p' \
    "$OUT_FILE" | sed '1d;$d' | strip_plan_stamp)"
assert_eq "$ROUNDTRIP" "$CHANGED_BODY" \
    "re-extract + re-strip the spliced region yields exactly the rendered body (round-trip)"

# The header and markers survived the splice untouched.
if cmp -s <(sed -n '1,/^<!-- guide:plan-body:start -->$/p' "$PLAN_STAMPED") \
          <(sed -n '1,/^<!-- guide:plan-body:start -->$/p' "$OUT_FILE"); then
    pass "everything above the start marker is byte-identical after the splice"
else
    fail "the splice touched content above the start marker"
fi

echo ""
echo "Test 5d: a changed render INSIDE the debounce window writes nothing"

OUT_FILE="$FIXTURE_DIR/out-5d.md"
rm -f "$OUT_FILE"
DECISION="$(work_plan_tick "$CHANGED_BODY" "$PLAN_STAMPED" "$LAST_MERGE" "$INSIDE_TICK" "$DEBOUNCE" "$OUT_FILE")"
assert_eq "$DECISION" "SKIP-debounce" \
    "a genuine diff inside the debounce window is suppressed (the stamp adds no side door around #5890)"
if [[ ! -e "$OUT_FILE" ]]; then
    pass "the debounce-suppressed tick wrote nothing"
else
    fail "the debounce-suppressed tick wrote a file"
fi

echo ""
echo "Test 5e: first-ever generation (no markers at all) is write-worthy"

OUT_FILE="$FIXTURE_DIR/out-5e.md"
rm -f "$OUT_FILE"
DECISION="$(work_plan_tick "$REGION_BODY" "$PLAN_UNMARKED" 0 1000000 "$DEBOUNCE" "$OUT_FILE")"
assert_eq "$DECISION" "WRITE" \
    "an empty extraction (no committed region) never compares equal, so the first generation writes"

trap - EXIT
cleanup_fixtures

# ---------------------------------------------------------------------------
# Test 6: the stamp prepend sits AFTER both gates inside update_work_plan()
# ---------------------------------------------------------------------------
echo ""
echo "Test 6: the splice (stamp) step is ordered after the debounce gate"

UWP_BODY="$(awk '/^update_work_plan\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
assert_contains "$UWP_BODY" 'new_body" = "$old_body"' \
    "update_work_plan() still performs the byte-for-byte equality test (normalized old_body)"

DEBOUNCE_POS="$(printf '%s\n' "$UWP_BODY" | grep -boF 'lt "$debounce_secs" ]' | head -1 | cut -d: -f1)"
STAMP_POS="$(printf '%s\n' "$UWP_BODY" | grep -boF 'plan_stamp_line' | tail -1 | cut -d: -f1)"
if [[ -n "$DEBOUNCE_POS" && -n "$STAMP_POS" && "$STAMP_POS" -gt "$DEBOUNCE_POS" ]]; then
    pass "the stamp prepend (splice step) comes after the debounce gate in update_work_plan()"
else
    fail "expected the stamp prepend to follow the debounce gate (debounce_pos='$DEBOUNCE_POS', stamp_pos='$STAMP_POS')"
fi

# ---------------------------------------------------------------------------
# Test 7: the two headers point readers at the stamp (#13894 wording kept,
# stamp pointer added)
# ---------------------------------------------------------------------------
echo ""
echo "Test 7: WORK_PLAN.md and the imagine.md template point at the stamp"

assert_doc_contains "$WORK_PLAN_MD" \
    'The `_Generated:` stamp at the top of the generated region dates the snapshot' \
    "committed WORK_PLAN.md header points readers at the stamp"
assert_doc_contains "$WORK_PLAN_MD" \
    'it may lag live labels until the next one merges' \
    "WORK_PLAN.md keeps the #13894 snapshot-semantics wording"
assert_doc_contains "$IMAGINE_MD" \
    'The `_Generated:` stamp at the top of the generated region dates the snapshot' \
    "imagine.md WORK_PLAN template header points readers at the stamp"

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
