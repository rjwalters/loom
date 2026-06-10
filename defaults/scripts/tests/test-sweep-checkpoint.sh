#!/bin/bash
# test-sweep-checkpoint.sh - Smoke tests for the sweep-checkpoint helper.
#
# These exercise the read/write/delete/exists/phase/list commands and the
# expected exit codes documented in sweep-checkpoint.sh and consumed by
# defaults/.claude/commands/loom/sweep.md (#3373).
#
# Run from anywhere — uses an isolated TMPDIR for the checkpoint directory so
# it never touches a real workspace's .loom/sweep-checkpoint/.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIR/../sweep-checkpoint.sh"

if [[ ! -x "$HELPER" ]]; then
    echo "FAIL: helper not executable at $HELPER" >&2
    exit 1
fi

# Isolated workspace — make sure we don't pollute the real .loom/sweep-checkpoint/
TMP_REPO="$(mktemp -d)"
trap 'rm -rf "$TMP_REPO"' EXIT

cd "$TMP_REPO" || exit 1
git init -q .
mkdir -p .loom/scripts
# Use a script-relative copy so `repo_root` lands here, not in the real loom checkout.
cp "$HELPER" .loom/scripts/sweep-checkpoint.sh
chmod +x .loom/scripts/sweep-checkpoint.sh

CHECKPOINT="$TMP_REPO/.loom/scripts/sweep-checkpoint.sh"

PASS=0
FAIL=0
assert() {
    local desc="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $desc (cmd: $*)" >&2
        FAIL=$((FAIL + 1))
    fi
}
assert_exit() {
    local desc="$1" expected="$2"; shift 2
    "$@" >/dev/null 2>&1
    local actual=$?
    if [[ $actual -eq $expected ]]; then
        echo "PASS: $desc (exit $actual)"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $desc (expected exit $expected, got $actual)" >&2
        FAIL=$((FAIL + 1))
    fi
}
assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        echo "PASS: $desc"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $desc (expected '$expected', got '$actual')" >&2
        FAIL=$((FAIL + 1))
    fi
}

# 1. exists on missing checkpoint → exit 1
assert_exit "exists returns 1 when missing" 1 "$CHECKPOINT" exists 42

# 2. phase on missing → empty output, exit 0
out=$("$CHECKPOINT" phase 42)
assert_eq "phase is empty when no checkpoint" "" "$out"

# 3. write curator-done
assert "write curator-done succeeds" "$CHECKPOINT" write 42 curator-done --task-id sweep-test

# 4. exists now returns 0
assert_exit "exists returns 0 after write" 0 "$CHECKPOINT" exists 42

# 5. phase returns curator-done
out=$("$CHECKPOINT" phase 42)
assert_eq "phase reads back curator-done" "curator-done" "$out"

# 6. read produces valid JSON containing the phase
out=$("$CHECKPOINT" read 42)
if echo "$out" | grep -q '"phase": "curator-done"'; then
    echo "PASS: read JSON contains phase=curator-done"
    PASS=$((PASS + 1))
else
    echo "FAIL: read JSON missing phase: $out" >&2
    FAIL=$((FAIL + 1))
fi

# 7. write builder-done with PR number
assert "write builder-done with pr-number" "$CHECKPOINT" write 42 builder-done --task-id sweep-test --pr-number 999
out=$("$CHECKPOINT" read 42)
if echo "$out" | grep -q '"pr_number": 999'; then
    echo "PASS: pr_number persisted as integer"
    PASS=$((PASS + 1))
else
    echo "FAIL: pr_number missing or wrong: $out" >&2
    FAIL=$((FAIL + 1))
fi

# 8. Invalid phase → exit 2
assert_exit "invalid phase exits 2" 2 "$CHECKPOINT" write 42 bogus-phase

# 9. Invalid issue number → exit 1
assert_exit "non-numeric issue exits 1" 1 "$CHECKPOINT" write abc curator-done

# 10. list shows the issue
out=$("$CHECKPOINT" list)
assert_eq "list reports issue 42" "42" "$out"

# 11. Add a second checkpoint and verify sorted listing
"$CHECKPOINT" write 7 judge-done --task-id sweep-test --pr-number 100 >/dev/null
out=$("$CHECKPOINT" list | tr '\n' ' ' | sed 's/ $//')
assert_eq "list returns sorted numeric order" "7 42" "$out"

# 12. delete removes the file
assert "delete succeeds" "$CHECKPOINT" delete 42
assert_exit "exists returns 1 after delete" 1 "$CHECKPOINT" exists 42

# 13. delete on already-missing is a no-op (exit 0)
assert_exit "delete-missing exits 0" 0 "$CHECKPOINT" delete 42

# 14. Atomic-write semantics: no stray .tmp.* files
strays=$(find "$TMP_REPO/.loom/sweep-checkpoint" -name 'issue-*.tmp.*' 2>/dev/null | wc -l | tr -d ' ')
assert_eq "no stray .tmp files after writes" "0" "$strays"

# 15. All valid phases accepted
for phase in curator-done builder-done judge-done doctor-done merge-done; do
    if "$CHECKPOINT" write 1 "$phase" --task-id t >/dev/null 2>&1; then
        echo "PASS: phase '$phase' accepted"
        PASS=$((PASS + 1))
    else
        echo "FAIL: phase '$phase' rejected" >&2
        FAIL=$((FAIL + 1))
    fi
done

# --- Optional attempt field (#3481, model escalation bookkeeping) ---

# 16. write with --attempt round-trips through read and attempt
assert "write doctor-done with --attempt 2" "$CHECKPOINT" write 50 doctor-done --task-id t --pr-number 123 --attempt 2
out=$("$CHECKPOINT" read 50)
if echo "$out" | grep -q '"attempt": 2'; then
    echo "PASS: attempt persisted as integer in JSON"
    PASS=$((PASS + 1))
else
    echo "FAIL: attempt missing or wrong: $out" >&2
    FAIL=$((FAIL + 1))
fi
out=$("$CHECKPOINT" attempt 50)
assert_eq "attempt command reads back 2" "2" "$out"

# 17. pr_number still intact alongside attempt
out=$("$CHECKPOINT" read 50)
if echo "$out" | grep -q '"pr_number": 123'; then
    echo "PASS: pr_number coexists with attempt"
    PASS=$((PASS + 1))
else
    echo "FAIL: pr_number lost when attempt present: $out" >&2
    FAIL=$((FAIL + 1))
fi

# 18. Backward compat: write WITHOUT --attempt omits the field entirely
"$CHECKPOINT" write 51 builder-done --task-id t >/dev/null
out=$("$CHECKPOINT" read 51)
if echo "$out" | grep -q '"attempt"'; then
    echo "FAIL: attempt field should be omitted when not provided: $out" >&2
    FAIL=$((FAIL + 1))
else
    echo "PASS: attempt field omitted when not provided"
    PASS=$((PASS + 1))
fi

# 19. Legacy checkpoint (no attempt field): attempt prints empty, exit 0
out=$("$CHECKPOINT" attempt 51)
assert_eq "attempt is empty on legacy checkpoint (= attempt 1)" "" "$out"
assert_exit "attempt on legacy checkpoint exits 0" 0 "$CHECKPOINT" attempt 51

# 20. Legacy checkpoint read path unaffected (phase still resolves)
out=$("$CHECKPOINT" phase 51)
assert_eq "phase still reads on attempt-less checkpoint" "builder-done" "$out"

# 21. attempt on missing checkpoint: empty output, exit 0 (mirrors phase)
out=$("$CHECKPOINT" attempt 9999)
assert_eq "attempt is empty when no checkpoint" "" "$out"
assert_exit "attempt on missing checkpoint exits 0" 0 "$CHECKPOINT" attempt 9999

# 22. Invalid --attempt values rejected with exit 1
assert_exit "non-numeric --attempt exits 1" 1 "$CHECKPOINT" write 52 doctor-done --attempt abc
assert_exit "zero --attempt exits 1" 1 "$CHECKPOINT" write 52 doctor-done --attempt 0
assert_exit "negative --attempt exits 1" 1 "$CHECKPOINT" write 52 doctor-done --attempt -1
if "$CHECKPOINT" exists 52 >/dev/null 2>&1; then
    echo "FAIL: rejected --attempt write should not create a checkpoint" >&2
    FAIL=$((FAIL + 1))
else
    echo "PASS: rejected --attempt write leaves no checkpoint behind"
    PASS=$((PASS + 1))
fi

# 23. Overwrite an attempt-bearing checkpoint without --attempt drops the field
"$CHECKPOINT" write 50 doctor-done --task-id t >/dev/null
out=$("$CHECKPOINT" attempt 50)
assert_eq "attempt cleared after attempt-less rewrite" "" "$out"

echo
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
