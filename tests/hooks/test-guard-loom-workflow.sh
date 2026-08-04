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

# Hermetic baseline: ambient guard-behavior overrides must not leak into tests
# (#4325). Tests that exercise env-driven behavior deliberately inject their
# vars explicitly per invocation, or via run_guard_in_worktree, so they are
# unaffected by this unset.
unset LOOM_FORCE_SCOPE LOOM_DEFAULT_BRANCH LOOM_GUARD_SQL LOOM_GUARD_CLOUD \
      LOOM_GUARD_REVERSIBLE_GH LOOM_RM_SCOPE LOOM_GUARD_READONLY_FASTPATH \
      LOOM_GUARD_WORKTREE_ISOLATION LOOM_WORKTREE_PATH LOOM_WORKTREE_ROOT \
      LOOM_GUARD_DECISION_LOG LOOM_GUARD_DECISION_LOG_FILE

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

# --- False-positive regression tests (issue #5109) -----------------------
# The phrase "gh pr merge" appearing as INERT TEXT (a heredoc-quoted commit
# message, a --search query value) must not deny -- only an actual invocation
# should. Both reproduce the exact occurrences reported in #5109.

PHRASE_CMD="gh pr merge"

# Occurrence 1: a commit message built via the CLAUDE.md-documented
# `-m "$(cat <<'EOF' ... EOF)"` heredoc idiom, quoting the phrase as prose
# documenting the very rule this guard enforces.
GH_5109_COMMIT_CMD='git add foo.md && git commit -m "$(cat <<'"'"'EOF'"'"'
Document the rule: never `'"$PHRASE_CMD"'` directly, use merge-pr.sh instead.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
)" && git push'
assert_allow "Allow heredoc commit message that quotes the phrase as prose" \
    "$GH_5109_COMMIT_CMD"

# Occurrence 2: a read-only search query whose --search value happens to
# contain the phrase as text to search FOR, not a command to run.
assert_allow "Allow gh issue list --search containing the phrase as query text" \
    "gh issue list --state open --search \"$PHRASE_CMD redirect guard false positive\" --limit 20 --json number,title,url"

# Regression guard: masking must NOT weaken the guard against an actual
# invocation wrapped in a shell -c string -- '-c' is deliberately not in the
# masked-flag whitelist, so this must still deny.
assert_deny "Still block gh pr merge wrapped in sh -c (no masking regression)" \
    "sh -c \"$PHRASE_CMD 123\""

# Regression guard: a heredoc that feeds an INTERPRETER (not `cat`) is live
# code, not inert data, and must stay visible to the check.
GH_5109_BASH_HEREDOC_CMD='bash <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block gh pr merge inside a bash-fed (non-cat) heredoc" \
    "$GH_5109_BASH_HEREDOC_CMD"

# Regression guard (PR #5115 review): `cat` never executes its own body, but
# piping its stdout into a shell on the SAME line -- `cat <<'EOF' | bash` --
# makes the heredoc body live, executed code despite `cat` being the literal
# consumer. The cat-heredoc masking must NOT neutralize such a body (it is not
# confined to inert text: it reaches `bash`), so a real invocation shaped this
# way must still deny. This is the exact bypass reported in the #5115 review.
GH_5115_CAT_PIPE_BASH_CMD='cat <<'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF'
assert_deny "Still block gh pr merge in a cat-heredoc piped into bash" \
    "$GH_5115_CAT_PIPE_BASH_CMD"

# And its `| sh` cousin -- same reasoning, different interpreter.
GH_5115_CAT_PIPE_SH_CMD='cat <<'"'"'EOF'"'"' | sh
'"$PHRASE_CMD"' 123
EOF'
assert_deny "Still block gh pr merge in a cat-heredoc piped into sh" \
    "$GH_5115_CAT_PIPE_SH_CMD"

# And a cat-heredoc captured then eval-executed: captured by $() but the
# consumer is `eval`, NOT a text-data flag, so the body is not inert and must
# stay visible.
GH_5115_CAT_EVAL_CMD='eval "$(cat <<'"'"'EOF'"'"'
'"$PHRASE_CMD"' 123
EOF
)"'
assert_deny "Still block gh pr merge in a cat-heredoc captured then eval'd" \
    "$GH_5115_CAT_EVAL_CMD"

# Regression guard (issue #5122 -- capre-gate bypass reported after #5115
# merged): the `capre` prefix gate (added by #5115 to allow the documented
# `-m "$(cat <<'EOF' ... EOF)"` idiom) only inspected the text BEFORE the
# `cat` token -- never what follows the heredoc opener line. So wrapping the
# SAME bypass shapes above in a flag-captured prefix (`-m "$(cat <<'EOF' |
# bash ...)"`) slipped straight past the gate and was masked as inert data,
# even though `cat`'s stdout is still piped into a live shell. The opener
# line must END immediately after the quoted delimiter to be masked; any
# suffix -- a pipe, a redirect, anything -- must keep the body visible to
# the merge-redirect grep.
GH_5122_FLAG_CAPTURED_PIPE_BASH_CMD='git commit -m "$(cat <<'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF
)"'
assert_deny "Still block gh pr merge in a flag-captured cat-heredoc piped into bash (#5122)" \
    "$GH_5122_FLAG_CAPTURED_PIPE_BASH_CMD"

# Same class, but the redirect parks the body in a file instead of piping it
# directly -- a later command on the same line then executes that file. The
# pipe is not the only live vector; any redirect on the opener line is.
GH_5122_FLAG_CAPTURED_REDIRECT_FILE_CMD='git commit -m "$(cat <<'"'"'EOF'"'"' 1> /tmp/loom-test-5122.sh
'"$PHRASE_CMD"' 123 --admin
EOF
)" ; bash /tmp/loom-test-5122.sh'
assert_deny "Still block gh pr merge in a flag-captured cat-heredoc redirected to a file then bash'd (#5122)" \
    "$GH_5122_FLAG_CAPTURED_REDIRECT_FILE_CMD"

# The `<<-` (dash) heredoc variant (strips leading tabs from the body) must
# get the same treatment -- the opener-line-suffix check does not special-
# case the `-` after `<<`.
GH_5122_FLAG_CAPTURED_DASH_PIPE_BASH_CMD='git commit -m "$(cat <<-'"'"'EOF'"'"' | bash
'"$PHRASE_CMD"' 123 --admin
EOF
)"'
assert_deny "Still block gh pr merge in a flag-captured cat <<- heredoc piped into bash (#5122)" \
    "$GH_5122_FLAG_CAPTURED_DASH_PIPE_BASH_CMD"

# --- False-positive regression tests (issue #5155) -----------------------
# The phrase appearing as INERT TEXT inside a POSITIONAL (no preceding flag
# name) argument to a known non-executing command must not deny -- only an
# actual invocation should. Both reproduce the exact occurrences reported in
# #5155 (a fresh shape not covered by #5109/#5115, which only masked
# NAMED-flag values).

# Reproduction 1: ./.loom/scripts/check-duplicate.sh's signature is
# `check-duplicate.sh TITLE DESCRIPTION` (purely positional, no flags). Both
# the title and description quote the phrase as prose describing this very
# guard bug.
GH_5155_CHECK_DUPLICATE_CMD="./.loom/scripts/check-duplicate.sh \"Guard false positive: $PHRASE_CMD redirect\" \"quotes the phrase '$PHRASE_CMD' as inert text, not a live invocation\""
assert_allow "Allow check-duplicate.sh positional TITLE/DESCRIPTION quoting the phrase as prose" \
    "$GH_5155_CHECK_DUPLICATE_CMD"

# Reproduction 2: a read-only `grep -n` source-code search whose pattern
# argument (positional, after the `-n` flag) happens to contain the phrase as
# search text. `grep` cannot execute anything it searches for.
assert_allow "Allow grep -n search of the guard's own source for the phrase" \
    "grep -n \"gh-pr-merge-redirect\\|$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh"

# ripgrep cousin of the same shape.
assert_allow "Allow rg search containing the phrase as a search pattern" \
    "rg -n \"$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh"

# Regression guard: masking must NOT weaken the guard against a command that
# is NOT in the new positional-arg allowlist -- `echo` piped into `bash` is a
# genuine (if contrived) execution vector, and `echo` is deliberately absent
# from the allowlist, so the phrase must remain visible and still deny.
assert_deny "Still block phrase piped through echo | bash (echo not allowlisted, #5155)" \
    "echo \"$PHRASE_CMD 123\" | bash"

# Regression guard: masking a matched positional span must not blind the
# check to a SECOND, real invocation elsewhere on the same command line --
# masking only narrows what THIS ONE check misses inside the matched
# grep/rg/check-duplicate.sh argument, it never widens what it misses
# outside that span.
assert_deny "Still block a real gh pr merge invocation chained after a masked grep search (#5155)" \
    "grep -n \"$PHRASE_CMD\" defaults/hooks/guard-loom-workflow.sh && $PHRASE_CMD 123"

echo ""

# --- False-positive regression tests (issue #5172) -----------------------
# `gh api`'s `-f <field>=<value>` syntax is a DIFFERENT shape from the
# `--body <value>` flags #5109/#5115 masked, and a heredoc ASSIGNED TO A
# SHELL VARIABLE earlier in the command (then only referenced later via that
# variable) is a two-hop indirection neither #5109 nor #5155 covered. Both
# shapes are reproduced here from the exact occurrence reported in #5172.

# Reproduction (exact #5172 shape): a heredoc assigned to $BODY, whose prose
# quotes the disallowed phrase describing test cases, referenced later via
# `gh api -f body="$BODY"`.
GH_5172_VAR_HEREDOC_CMD='BODY="$(cat <<'"'"'EOF2'"'"'
Tested: raw '"$PHRASE_CMD"' 123 denied; echo "'"$PHRASE_CMD"' 123" | bash denied.
EOF2
)"
gh api "repos/rjwalters/loom/issues/5172/comments" -f body="$BODY"'
assert_allow "Allow gh api -f body=\$VAR where \$VAR is a heredoc quoting the phrase as prose (#5172)" \
    "$GH_5172_VAR_HEREDOC_CMD"

# A heredoc directly captured (no variable indirection) by `-f body=` must
# also be recognized -- the `gh api` field-syntax analog of the #5109 `-m`/
# `--body` case.
GH_5172_DIRECT_FIELD_HEREDOC_CMD='gh api "repos/o/r/issues/1/comments" -f body="$(cat <<'"'"'EOF5'"'"'
'"$PHRASE_CMD"' 123 as prose
EOF5
)"'
assert_allow "Allow gh api -f body=\"\$(cat <<EOF ...)\" heredoc captured directly (#5172)" \
    "$GH_5172_DIRECT_FIELD_HEREDOC_CMD"

# A literal (non-heredoc) `-f body="..."` value quoting the phrase directly.
assert_allow "Allow gh api -f body=\"...\" literal value quoting the phrase as prose (#5172)" \
    "gh api \"repos/o/r/issues/1/comments\" -f body=\"Tested: $PHRASE_CMD 123 denied\""

# The variable-indirection fix must generalize beyond `body` to the other
# known text-bearing `gh api` fields (message here).
GH_5172_VAR_HEREDOC_MESSAGE_CMD='MSG="$(cat <<'"'"'EOF6'"'"'
'"$PHRASE_CMD"' 123 as prose in message field
EOF6
)"
gh api "repos/o/r/issues/1/comments" -f message="$MSG"'
assert_allow "Allow gh api -f message=\$VAR where \$VAR is a heredoc quoting the phrase as prose (#5172)" \
    "$GH_5172_VAR_HEREDOC_MESSAGE_CMD"

# Regression guard (control, #5172): the two-hop indirection fix must NOT
# widen the guard into a bypass. A heredoc assigned to a variable that is
# THEN eval'd is a genuine live invocation and must still deny -- masking a
# variable-assigned heredoc's body is gated on EVERY later reference to that
# variable being confined to a known-safe flag/field value; `eval "$CMD"` is
# not in that allowlist.
GH_5172_EVAL_BYPASS_CMD='CMD="$(cat <<'"'"'EOF3'"'"'
'"$PHRASE_CMD"' 123 --admin
EOF3
)"
eval "$CMD"'
assert_deny "Still block a heredoc-assigned variable later eval'd (two-hop bypass attempt, #5172)" \
    "$GH_5172_EVAL_BYPASS_CMD"

# Regression guard (control, #5172): a heredoc-assigned variable referenced
# bare (as a command, not a safe flag/field value) must also still deny.
GH_5172_BARE_REF_CMD='X="$(cat <<'"'"'EOF4'"'"'
'"$PHRASE_CMD"' 123
EOF4
)"
$X'
assert_deny "Still block a heredoc-assigned variable referenced bare as a command (#5172)" \
    "$GH_5172_BARE_REF_CMD"

# Regression guard (control, #5172): a heredoc-assigned variable referenced
# TWICE -- once safely (-f body=), once unsafely (piped into bash) -- must
# still deny. One confined reference does not excuse an unconfined one.
GH_5172_MIXED_REF_CMD='X="$(cat <<'"'"'EOF7'"'"'
'"$PHRASE_CMD"' 123
EOF7
)"
gh api "repos/o/r/issues/1/comments" -f body="$X"
echo "$X" | bash'
assert_deny "Still block a heredoc-assigned variable with one safe and one unsafe later reference (#5172)" \
    "$GH_5172_MIXED_REF_CMD"

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
