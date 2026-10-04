#!/usr/bin/env bash
# test-worktree-refused-claim-no-lease.sh — Tests for #10204.
#
# `worktree.sh N` used to run `loom-daemon lease ensure` at pre-flight, BEFORE
# its refusal gates (the claim-lock cross-check, the co-occupancy guard and the
# #9453 `forge check-claim --force-claim` open-linked-PR refusal). Every
# refusal therefore left a published lease comment and a running renewer
# behind for a claim the caller was told it did not hold. The call now sits
# after the last refusal gate of each arm.
#
# Driven by a stub `loom-daemon` (LOOM_DAEMON_SELF_BIN) that logs every
# invocation and answers `forge check-claim` from $STUB_CHECK_CLAIM_RC. Every
# other subcommand exits 2 (unrecognized -> the script's fail-open path).
#   1. check-claim exits 0 (open linked PR) -> refused, NO `lease ensure`.
#   2. check-claim exits 1 (safe)           -> worktree created, `lease ensure`
#      called exactly once with the issue number.
#   3. reuse arm (worktree already exists)  -> `lease ensure` still called.
#   4. claim-lock check-issue exits 1       -> refused, NO `lease ensure`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE_SH="$SCRIPTS_DIR/worktree.sh"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

TMP=$(mktemp -d /tmp/loom-refused-lease.XXXXXX)
trap 'rm -rf "$TMP"; cd "$SCRIPTS_DIR" 2>/dev/null || true' EXIT

git init -q -b main "$TMP/origin.git" --bare
git init -q -b main "$TMP/repo"
cd "$TMP/repo"
git config user.email t@t
git config user.name t
git commit --allow-empty -q -m init
git remote add origin "$TMP/origin.git"
git push -q origin main
mkdir -p .loom/scripts/lib .loom/hooks
cp "$WORKTREE_SH" .loom/scripts/worktree.sh
[[ -d "$SCRIPTS_DIR/lib" ]] && cp -R "$SCRIPTS_DIR"/lib/* .loom/scripts/lib/ 2>/dev/null || true
chmod +x .loom/scripts/worktree.sh

STUB="$TMP/loom-daemon"
LOG="$TMP/daemon.log"
cat > "$STUB" <<'STUBEOF'
#!/usr/bin/env bash
echo "$*" >> "$STUB_LOG"
case "$1 $2" in
    "forge check-claim")     exit "${STUB_CHECK_CLAIM_RC:-1}" ;;
    "worktree-lock check-issue") exit "${STUB_CHECK_ISSUE_RC:-2}" ;;
    "lease ensure")          exit 0 ;;
esac
exit 2
STUBEOF
chmod +x "$STUB"
export LOOM_DAEMON_SELF_BIN="$STUB" STUB_LOG="$LOG"

run_wt() { # <issue> -> sets RC, resets log
    : > "$LOG"; RC=0
    ./.loom/scripts/worktree.sh "$1" >"$TMP/out.log" 2>&1 || RC=$?
}
lease_calls() { grep -c '^lease ensure' "$LOG" || true; }

echo "Test 1: open linked PR (check-claim exit 0) -> refused, no lease"
STUB_CHECK_CLAIM_RC=0 run_wt 501
if [[ "$RC" -ne 0 && ! -d .loom/worktrees/issue-501 ]]; then pass "refused, no worktree"; else fail "expected refusal (rc=$RC)"; cat "$TMP/out.log"; fi
if [[ "$(lease_calls)" -eq 0 ]]; then pass "lease ensure not called"; else fail "lease ensure called on refusal"; fi

echo "Test 2: safe claim (check-claim exit 1) -> worktree created, lease ensured once"
STUB_CHECK_CLAIM_RC=1 run_wt 502
if [[ "$RC" -eq 0 && -d .loom/worktrees/issue-502 ]]; then pass "worktree created"; else fail "expected success (rc=$RC)"; cat "$TMP/out.log"; fi
if grep -q '^lease ensure 502 ' "$LOG" && [[ "$(lease_calls)" -eq 1 ]]; then pass "lease ensure called once"; else fail "lease ensure calls: $(lease_calls)"; fi

echo "Test 3: reuse arm (worktree exists) -> lease still ensured"
STUB_CHECK_CLAIM_RC=1 run_wt 502
if grep -q '^lease ensure 502 ' "$LOG"; then pass "reuse arm leases"; else fail "reuse arm did not lease (rc=$RC)"; cat "$TMP/out.log"; fi

echo "Test 4: claim-lock conflict (check-issue exit 1) -> refused, no lease"
STUB_CHECK_ISSUE_RC=1 run_wt 503
if [[ "$RC" -ne 0 && "$(lease_calls)" -eq 0 ]]; then pass "refused, no lease"; else fail "rc=$RC leases=$(lease_calls)"; fi

echo
echo "Tests run: $TESTS_RUN, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]]
