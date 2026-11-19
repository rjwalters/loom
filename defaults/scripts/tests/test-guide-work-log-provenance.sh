#!/usr/bin/env bash
# test-guide-work-log-provenance.sh - Regression test for issue #13760
#
# A Guide backfill wrote another repository's merged history into a consumer
# repo's WORK_LOG.md (sg13g2-vco#311: 61 entries above that repo's highest
# number, verbatim titles from sg13g2-comparator), and Judge's docs-only fast
# path approved the file-name shape without reading the content. The Guide
# side fix is the #13760 provenance gate: before update_work_log() writes
# anything, compose_work_log_delta() emits the exact canonical lines the tick
# would publish and verify_work_log_provenance() verifies every one against
# the pinned repo — number exists there, kind matches (merged PR vs closed
# issue), title matches; a foreign/missing/wrong-kind/mismatched/unreadable
# entry refuses the whole delta.
#
# Verifies (stubbed `$GH_READ`, hermetic, two named repositories):
#   1. compose_work_log_delta() emits the canonical renderer format from the
#      candidate JSON arrays.
#   2. verify_work_log_provenance() ACCEPTS a delta whose entries match the
#      pinned repo (a merged PR and a closed issue), and a zero-entry delta
#      (no-op tick) with no forge reads at all.
#   3. It REJECTS a wrong-repo listing: a number absent from the target, a
#      number present with a different title, and a number present with the
#      wrong kind (target has an issue where the entry records a PR).
#   4. It REJECTS a foreign row carrying the SAME number and title as a real
#      target item, when the entry itself names the foreign repository —
#      source-supplied repository identity cannot override the target binding.
#   5. It REJECTS a mixed valid/invalid delta (no partial writes) and fails
#      closed on a temporary forge failure.
#   6. Every forge read hit the TARGET repository only (the stub records what
#      was queried), and distinct numbers are read once each.
#   7. update_work_log() actually wires the gate: the call sites exist in the
#      shipped prompt, after the debounce gate and before the write.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi
source "$SCRIPT_DIR/lib/assert.sh"

extract() { awk -v re="^$1\\\\(\\\\) \\\\{" '$0 ~ re {f=1} f{print} /^\}/{if(f) exit}' "$GUIDE_MD"; }

CWD_FN="$(extract compose_work_log_delta)"
VWP="$(extract verify_work_log_provenance)"
[[ -n "$CWD_FN" && -n "$VWP" ]] || { echo "FATAL: helpers not found in guide.md"; exit 1; }
pass "helpers extracted from $GUIDE_MD"

# Stub: forge state for TWO named repositories. owner/mine is the pinned
# target (PR #5 "Add widget", closed issue #7 "Fix gizmo"); owner/other is a
# foreign repo the gate must never query. Every call is recorded to
# $GH_CALL_LOG so the tests can assert which target was actually queried.
# FAIL_LOOKUP=1 simulates a temporary forge failure (the call is logged, then
# exits 1).
GH_CALL_LOG="$(mktemp)"
GH_STUB="$(mktemp -d)/gh"
cat >"$GH_STUB" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$2" >> "$GH_CALL_LOG"
[[ "\${FAIL_LOOKUP:-}" == 1 ]] && exit 1
case "\$2" in
  repos/owner/mine/issues/5) printf 'Add widget\ttrue' ;;
  repos/owner/mine/issues/7) printf 'Fix gizmo\tfalse' ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$GH_STUB"
export GH_READ="$GH_STUB" GUIDE_REPO="owner/mine"
eval "$CWD_FN"
eval "$VWP"

echo "Test 1: compose_work_log_delta emits the canonical renderer format"
DELTA="$(compose_work_log_delta \
  '[{"number":5,"title":"Add widget"},{"number":6,"title":"Tidy docs"}]' \
  '[{"number":7,"title":"Fix gizmo"}]')"
assert_eq '- **PR #5**: Add widget
- **PR #6**: Tidy docs
- **Issue #7** (closed): Fix gizmo' "$DELTA" "delta composed as - **PR #N**: Title / - **Issue #N** (closed): Title"
DELTA="$(compose_work_log_delta '[]' '[]')"
assert_eq '' "$DELTA" "empty candidate arrays compose an empty delta (no-op tick)"

echo "Test 2: matching target entries pass"
verify_work_log_provenance $'- **PR #5**: Add widget' 2>/dev/null \
  && pass "matching target PR accepted" || fail "matching target PR rejected"
verify_work_log_provenance $'- **Issue #7** (closed): Fix gizmo' 2>/dev/null \
  && pass "matching closed issue accepted" || fail "matching closed issue rejected"
verify_work_log_provenance $'- **PR #5**: Add widget\n- **Issue #7** (closed): Fix gizmo' 2>/dev/null \
  && pass "mixed PR+issue delta accepted" || fail "mixed PR+issue delta rejected"
verify_work_log_provenance $'### 2026-11-19\n\nSome prose mentioning #5 and a non-canonical line\n- **PR #5**: Add widget' 2>/dev/null \
  && pass "prose and non-canonical lines are skipped (only canonical records verified)" || fail "prose lines were treated as records"

echo "Test 3: zero-entry delta passes with NO forge reads"
: >"$GH_CALL_LOG"
verify_work_log_provenance '' 2>/dev/null \
  && pass "empty delta accepted (no-op preserved)" || fail "empty delta rejected"
[[ -s "$GH_CALL_LOG" ]] && fail "empty delta still queried the forge" || pass "empty delta made zero forge reads"

echo "Test 4: wrong-repo listing rejected"
verify_work_log_provenance $'- **PR #99**: Brand new feature' 2>/dev/null \
  && fail "number absent from the target accepted" || pass "number absent from the target rejected"
verify_work_log_provenance $'- **PR #5**: Some other repo\x27s title' 2>/dev/null \
  && fail "wrong title accepted" || pass "wrong title rejected"
verify_work_log_provenance $'- **Issue #5** (closed): Add widget' 2>/dev/null \
  && fail "wrong kind accepted (target #5 is a PR)" || pass "wrong kind rejected (target #5 is a PR)"
verify_work_log_provenance $'- **PR #7**: Fix gizmo' 2>/dev/null \
  && fail "wrong kind accepted (target #7 is an issue)" || pass "wrong kind rejected (target #7 is an issue)"

echo "Test 5: source-supplied repository identity cannot override the target binding"
# Same number AND same title as the real target item — but the entry names a
# foreign repository, so it must be refused regardless of the coincidence.
verify_work_log_provenance $'- **PR owner/other#5**: Add widget' 2>/dev/null \
  && fail "foreign-repo-identified entry with matching number+title accepted" \
  || pass "foreign-repo-identified entry refused even with matching number+title"
verify_work_log_provenance $'- **Issue owner/other#7** (closed): Fix gizmo' 2>/dev/null \
  && fail "foreign-repo-identified issue entry accepted" \
  || pass "foreign-repo-identified issue entry refused"

echo "Test 6: mixed valid/invalid delta refused whole (no partial writes)"
verify_work_log_provenance $'- **PR #5**: Add widget\n- **PR #99**: Not there' 2>/dev/null \
  && fail "mixed valid+invalid delta accepted" || pass "one bad entry refuses the whole delta"

echo "Test 7: temporary forge failure fails closed"
FAIL_LOOKUP=1 verify_work_log_provenance $'- **PR #5**: Add widget' 2>/dev/null \
  && fail "forge failure treated as pass" || pass "unreadable target lookup fails closed"

echo "Test 8: the target actually queried is the pinned repo; distinct numbers read once"
: >"$GH_CALL_LOG"
verify_work_log_provenance $'- **PR #5**: Add widget\n- **Issue #7** (closed): Fix gizmo\n- **PR #5**: Add widget' 2>/dev/null \
  && pass "delta with a duplicate number accepted" || fail "delta with a duplicate number rejected"
assert_eq "repos/owner/mine/issues/5
repos/owner/mine/issues/7" "$(cat "$GH_CALL_LOG")" \
  "exactly two reads, both against the pinned repo owner/mine, deduped by number"
if grep -q 'owner/other' "$GH_CALL_LOG"; then
  fail "the gate queried the foreign repository"
else
  pass "the gate never queried the foreign repository"
fi

echo "Test 9: update_work_log() wires the gate before the write"
AWK_OUT="$(awk '/^update_work_log\(\) \{/{f=1} f{print} f && /^\}$/{exit}' "$GUIDE_MD")"
assert_contains "$AWK_OUT" 'delta="$(compose_work_log_delta "$new_prs" "$new_issues")"' \
  "update_work_log composes the delta from the verified candidates"
assert_contains "$AWK_OUT" 'verify_work_log_provenance "$delta"' \
  "update_work_log gates the delta before writing"
assert_contains "$AWK_OUT" 'PROVENANCE GATE' \
  "update_work_log marks the gate do-not-remove"
# Ordering: the compose call must precede the gate call, and both must precede
# the write step ("Group entries by date").
C_LINE="$(grep -n 'compose_work_log_delta "\$new_prs"' <<<"$AWK_OUT" | cut -d: -f1)"
V_LINE="$(grep -n 'verify_work_log_provenance "\$delta"' <<<"$AWK_OUT" | cut -d: -f1)"
W_LINE="$(grep -n 'Group entries by date' <<<"$AWK_OUT" | cut -d: -f1)"
if [[ -n "$C_LINE" && -n "$V_LINE" && -n "$W_LINE" && "$C_LINE" -lt "$V_LINE" && "$V_LINE" -lt "$W_LINE" ]]; then
  pass "gate order: compose -> verify -> write"
else
  fail "gate order wrong (compose=$C_LINE verify=$V_LINE write=$W_LINE)"
fi

echo
echo "Tests run: $TESTS_RUN, passed: $TESTS_PASSED, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]] || exit 1
echo "All tests passed"
