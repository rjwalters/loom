#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-loom-workflow.sh
#
# Usage: ./tests/hooks/test-guard-loom-workflow.sh
#
# Tests the extracted Loom-workflow PreToolUse guard (issue #3604): the
# 'gh pr merge' -> merge-pr.sh redirect and the 'pip install -e' worktree block.
# Exit code 0 = all tests pass, 1 = failures detected.
#
# The guard under test is the canonical source at defaults/hooks/ (the
# version-controlled source of truth), NOT the gitignored .loom/hooks/ install
# artifact — so the suite validates exactly what ships.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD="$REPO_ROOT/defaults/hooks/guard-loom-workflow.sh"

PASS=0
FAIL=0
TOTAL=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Build a JSON input blob for the guard script
make_input() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    jq -n --arg cmd "$cmd" --arg cwd "$cwd" '{
        tool_name: "Bash",
        tool_input: { command: $cmd },
        cwd: $cwd
    }'
}

# Run the guard and capture output + exit code
run_guard() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    local output
    local exit_code
    output=$(make_input "$cmd" "$cwd" | "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

# Run the guard with LOOM_WORKTREE_PATH set (simulates worktree context)
run_guard_in_worktree() {
    local cmd="$1"
    local cwd="${2:-$REPO_ROOT}"
    local output
    local exit_code
    output=$(LOOM_WORKTREE_PATH="$cwd" make_input "$cmd" "$cwd" | LOOM_WORKTREE_PATH="$cwd" "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

# Assert the guard denies a command
assert_deny() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

# Assert the guard denies a command AND the reason matches an ERE.
assert_deny_reason_matches() {
    local description="$1"
    local cmd="$2"
    local pattern="$3"
    local cwd="${4:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output reason
    output=$(run_guard "$cmd" "$cwd") || true
    reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null)
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 && \
       echo "$reason" | grep -qE "$pattern"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny with reason matching /$pattern/"
        echo -e "       Got: $output"
    fi
}

# Assert the guard allows a command (exit 0, no decision)
assert_allow() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    local exit_code=0
    output=$(run_guard "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

# Assert the guard denies a command when inside a worktree
assert_deny_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard_in_worktree "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

# Assert the guard allows a command when inside a worktree
assert_allow_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$REPO_ROOT}"
    TOTAL=$((TOTAL + 1))
    local output
    local exit_code=0
    output=$(run_guard_in_worktree "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

# =========================================================================
echo ""
echo -e "${YELLOW}=== Testing guard-loom-workflow.sh ===${NC}"
echo ""

# =========================================================================
echo -e "${YELLOW}--- gh pr merge redirect ---${NC}"
# =========================================================================

assert_deny "Block gh pr merge" \
    "gh pr merge 123"

assert_deny "Block gh pr merge --squash" \
    "gh pr merge 123 --squash"

# The deny message must name merge-pr.sh so the agent learns the correct tool.
assert_deny_reason_matches "gh pr merge deny reason names merge-pr.sh" \
    "gh pr merge 123" "merge-pr\.sh"

echo ""

# =========================================================================
echo -e "${YELLOW}--- pip install -e WORKTREE GUARD (issue #2495) ---${NC}"
# =========================================================================

# Should DENY editable installs when LOOM_WORKTREE_PATH is set
assert_deny_in_worktree "Block pip install -e in worktree" \
    "pip install -e ."

assert_deny_in_worktree "Block pip install -e ./loom-tools in worktree" \
    "pip install -e ./loom-tools"

assert_deny_in_worktree "Block pip3 install -e in worktree" \
    "pip3 install -e ."

assert_deny_in_worktree "Block pip install --editable in worktree" \
    "pip install --editable ."

assert_deny_in_worktree "Block uv pip install -e in worktree" \
    "uv pip install -e ./loom-tools"

assert_deny_in_worktree "Block pip install -e with absolute path in worktree" \
    "pip install -e /Users/dev/project/loom-tools"

# The deny message must reference issue #2495.
TOTAL=$((TOTAL + 1))
_wt_out=$(run_guard_in_worktree "pip install -e ." "$REPO_ROOT") || true
if echo "$_wt_out" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null | grep -q "2495"; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: pip install -e deny reason references issue #2495"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: pip install -e deny reason references issue #2495"
    echo -e "       Got: $_wt_out"
fi

# Should ALLOW non-editable pip installs in worktree
assert_allow_in_worktree "Allow pip install (non-editable) in worktree" \
    "pip install pytest"

assert_allow_in_worktree "Allow pip install -r requirements.txt in worktree" \
    "pip install -r requirements.txt"

# Should ALLOW editable installs OUTSIDE worktrees (no LOOM_WORKTREE_PATH)
assert_allow "Allow pip install -e outside worktree" \
    "pip install -e ."

assert_allow "Allow pip3 install -e ./loom-tools outside worktree" \
    "pip3 install -e ./loom-tools"

assert_allow "Allow uv pip install -e outside worktree" \
    "uv pip install -e ."

assert_allow "Allow pip install --editable outside worktree" \
    "pip install --editable ."

echo ""

# =========================================================================
echo -e "${YELLOW}--- Unrelated commands pass through (allow) ---${NC}"
# =========================================================================

assert_allow "Allow git status" \
    "git status"

assert_allow "Allow gh pr create" \
    "gh pr create --title 'My PR' --body 'Description'"

assert_allow "Allow gh pr list" \
    "gh pr list"

# Catastrophic/generic patterns are NOT this hook's job (guard-destructive.sh
# owns them); this hook must allow them through.
assert_allow "Allow rm -rf / (not this hook's responsibility)" \
    "rm -rf /"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Hook schema contract ---${NC}"
# =========================================================================

# Deny decisions must carry hookEventName: PreToolUse (#3550).
TOTAL=$((TOTAL + 1))
_schema_out=$(run_guard "gh pr merge 123" "$REPO_ROOT") || true
if echo "$_schema_out" | jq -e '.hookSpecificOutput.hookEventName == "PreToolUse"' >/dev/null 2>&1; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: deny decision carries hookEventName == PreToolUse"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: deny decision carries hookEventName == PreToolUse"
    echo -e "       Got: $_schema_out"
fi

# Never exits non-zero, even on empty command input.
TOTAL=$((TOTAL + 1))
_empty_exit=0
echo '{"tool_input":{"command":""},"cwd":"'"$REPO_ROOT"'"}' | "$GUARD" >/dev/null 2>&1 || _empty_exit=$?
if [[ $_empty_exit -eq 0 ]]; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: empty command exits 0 (allow)"
else
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: empty command exits 0 (allow), got exit $_empty_exit"
fi

echo ""

# =========================================================================
echo -e "${YELLOW}--- Decision telemetry log (#3898) ---${NC}"
# =========================================================================
#
# guard-loom-workflow.sh appends one JSONL record per DENY to the SAME decision
# log guard-destructive.sh writes (.loom/logs/guard-decisions.log by default),
# gated by guards.decisionLog / LOOM_GUARD_DECISION_LOG (default OFF). The record
# schema is the STABLE contract shared with guard-destructive.sh:
#   {"ts","decision":"deny","pattern":"<tag>","tier":"catastrophic","command"}.
# The LOOM_GUARD_DECISION_LOG_FILE test seam overrides the write path.

DLW_DIR="$(mktemp -d)"
DLW_LOG="$DLW_DIR/guard-decisions.log"

dlw_assert() {
    TOTAL=$((TOTAL + 1))
    if [[ "$2" -eq 0 ]]; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $1"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $1"
        [[ -n "${3:-}" ]] && echo -e "       ${3}"
    fi
}

# (a) gh pr merge deny writes decision=deny, tier=catastrophic, stable tag.
rm -f "$DLW_LOG"
make_input "gh pr merge 123" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
_dlw_rec="$(tail -1 "$DLW_LOG" 2>/dev/null)"
if [[ -f "$DLW_LOG" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.decision' 2>/dev/null)" == "deny" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.tier' 2>/dev/null)" == "catastrophic" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.pattern' 2>/dev/null)" == "loom:gh-pr-merge-redirect" ]] && \
   [[ -n "$(printf '%s' "$_dlw_rec" | jq -r '.ts' 2>/dev/null)" ]] && \
   [[ -n "$(printf '%s' "$_dlw_rec" | jq -r '.command' 2>/dev/null)" ]]; then
    dlw_assert "gh pr merge deny logs a JSONL record (tag=loom:gh-pr-merge-redirect)" 0
else
    dlw_assert "gh pr merge deny logs a JSONL record (tag=loom:gh-pr-merge-redirect)" 1 "record: ${_dlw_rec:-<none>}"
fi

# (b) pip install -e worktree deny writes its own stable tag.
rm -f "$DLW_LOG"
LOOM_WORKTREE_PATH="$REPO_ROOT" make_input "pip install -e ." "$REPO_ROOT" | \
    env LOOM_WORKTREE_PATH="$REPO_ROOT" LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
_dlw_rec="$(tail -1 "$DLW_LOG" 2>/dev/null)"
if [[ -f "$DLW_LOG" ]] && \
   [[ "$(printf '%s' "$_dlw_rec" | jq -r '.pattern' 2>/dev/null)" == "loom:pip-install-editable-worktree" ]]; then
    dlw_assert "pip install -e worktree deny logs tag=loom:pip-install-editable-worktree" 0
else
    dlw_assert "pip install -e worktree deny logs tag=loom:pip-install-editable-worktree" 1 "record: ${_dlw_rec:-<none>}"
fi

# (c) Toggle default OFF (no env, non-repo cwd so config can't flip it on):
# no decision record is written even though the command denies.
_dlw_norepo="$(mktemp -d)"
rm -f "$DLW_LOG"
make_input "gh pr merge 123" "$_dlw_norepo" | \
    env LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
if [[ ! -f "$DLW_LOG" ]]; then
    dlw_assert "toggle default OFF: deny writes NO decision record" 0
else
    dlw_assert "toggle default OFF: deny writes NO decision record" 1 "unexpected: $(cat "$DLW_LOG")"
fi
rm -rf "$_dlw_norepo"

# (d) An allow-only command writes NO record even with the toggle on.
rm -f "$DLW_LOG"
make_input "git status" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="$DLW_LOG" "$GUARD" >/dev/null 2>&1 || true
if [[ ! -f "$DLW_LOG" ]] || [[ "$(wc -l < "$DLW_LOG" 2>/dev/null || echo 0)" -eq 0 ]]; then
    dlw_assert "allow-only command writes NO decision record (toggle on)" 0
else
    dlw_assert "allow-only command writes NO decision record (toggle on)" 1 "unexpected: $(cat "$DLW_LOG")"
fi

# (e) Fail-open: an unwritable decision-log path never changes the deny and never
# causes a non-zero exit.
_dlw_out=""; _dlw_rc=0
_dlw_out="$(make_input "gh pr merge 123" "$REPO_ROOT" | \
    env LOOM_GUARD_DECISION_LOG=1 LOOM_GUARD_DECISION_LOG_FILE="/nonexistent-dir-3898/a/b/decisions.log" "$GUARD" 2>/dev/null)" || _dlw_rc=$?
if [[ "$_dlw_rc" -eq 0 ]] && \
   [[ "$(printf '%s' "$_dlw_out" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)" == "deny" ]]; then
    dlw_assert "fail-open: unwritable decision log still denies and exits 0" 0
else
    dlw_assert "fail-open: unwritable decision log still denies and exits 0" 1 "rc=$_dlw_rc out=$_dlw_out"
fi

[[ -n "$DLW_DIR" && "$DLW_DIR" != "/" && -d "$DLW_DIR" ]] && rm -rf "$DLW_DIR"

echo ""

# =========================================================================
# Summary
# =========================================================================

echo "========================================="
echo -e "  Total:  $TOTAL"
echo -e "  ${GREEN}Passed${NC}: $PASS"
echo -e "  ${RED}Failed${NC}: $FAIL"
echo "========================================="

if [[ $FAIL -gt 0 ]]; then
    echo -e "\n${RED}TESTS FAILED${NC}"
    exit 1
else
    echo -e "\n${GREEN}ALL TESTS PASSED${NC}"
    exit 0
fi
