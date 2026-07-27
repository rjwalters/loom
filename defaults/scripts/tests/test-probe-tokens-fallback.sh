#!/usr/bin/env bash
# test-probe-tokens-fallback.sh - Regression guard for #4079.
#
# probe-tokens.sh falls back to a Python module invocation
# (`python3 -m loom_tools.tokens.cli check`) when the `loom-tokens` console
# script isn't on PATH. #4079 fixed a typo where the fallback invoked a
# module that never existed (`loom_tools.cli.loom_tokens`, real module is
# `loom_tools.tokens.cli`). This test asserts:
#   1. probe-tokens.sh no longer references the stale module path.
#   2. The fallback module path it does reference is actually importable.
#   3. With no `loom-tokens` on PATH, probe-tokens.sh --json exits 0 and
#      emits JSON (exercises the real fallback codepath end-to-end).
#
# Usage:
#   ./.loom/scripts/tests/test-probe-tokens-fallback.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DEFAULTS_DIR="$(cd "$SCRIPTS_DIR/.." && pwd)"
REPO_ROOT="$(cd "$DEFAULTS_DIR/.." && pwd)"
PROBE_SCRIPT="$SCRIPTS_DIR/probe-tokens.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
}

# -------- Test 1: script exists and is executable --------
echo "Test 1: script exists and is executable"
if [[ -x "$PROBE_SCRIPT" ]]; then
    pass "probe-tokens.sh is executable"
else
    fail "probe-tokens.sh is missing or not executable: $PROBE_SCRIPT"
    echo "FAILED: $TESTS_FAILED/$TESTS_RUN"
    exit 1
fi

# -------- Test 2: no reference to the stale never-existent module --------
echo "Test 2: no reference to the stale loom_tools.cli.loom_tokens module"
if grep -q "loom_tools\.cli\.loom_tokens" "$PROBE_SCRIPT"; then
    fail "probe-tokens.sh still references the never-existent loom_tools.cli.loom_tokens"
else
    pass "probe-tokens.sh does not reference loom_tools.cli.loom_tokens"
fi

# -------- Test 3: the fallback module it references is importable --------
echo "Test 3: fallback module path is importable"
fallback_module="$(grep -oE 'loom_tools\.[a-zA-Z0-9_.]+' "$PROBE_SCRIPT" | grep -v '^loom_tools\.cli\.loom_tokens$' | head -n1 || true)"
if [[ -z "$fallback_module" ]]; then
    fail "could not extract a fallback module reference from probe-tokens.sh"
else
    if PYTHONPATH="$REPO_ROOT/loom-tools/src${PYTHONPATH:+:$PYTHONPATH}" python3 -c \
        "import importlib.util, sys; sys.exit(0 if importlib.util.find_spec('$fallback_module') else 1)" \
        2>/dev/null; then
        pass "fallback module '$fallback_module' is importable"
    else
        fail "fallback module '$fallback_module' is NOT importable"
    fi
fi

# -------- Test 4: with no loom-tokens on PATH, --json exits 0 and emits JSON --------
echo "Test 4: --json fallback exits 0 and emits JSON with no loom-tokens on PATH"
# Build a PATH with common system dirs but no loom-tokens console script, so
# the script is forced down the python3 fallback branch.
stripped_path="/usr/bin:/bin:/usr/sbin:/sbin"
if command -v python3 >/dev/null 2>&1; then
    python3_dir="$(dirname "$(command -v python3)")"
    stripped_path="$python3_dir:$stripped_path"
fi
if command -v loom-tokens >/dev/null 2>&1; then
    echo "  (skipping: loom-tokens is on PATH in this environment, cannot force fallback via PATH stripping)"
    pass "skipped (loom-tokens present; not a failure)"
else
    json_out="$(cd "$REPO_ROOT" && PATH="$stripped_path" PYTHONPATH="$REPO_ROOT/loom-tools/src${PYTHONPATH:+:$PYTHONPATH}" "$PROBE_SCRIPT" --json 2>/dev/null)"
    rc=$?
    if [[ "$rc" -eq 0 ]]; then
        pass "fallback --json exit code is 0"
    else
        fail "fallback --json expected exit 0, got $rc"
    fi
    if printf '%s' "$json_out" | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
        pass "fallback --json emits parseable JSON"
    else
        fail "fallback --json did not emit parseable JSON. Got: $json_out"
    fi
fi

# -------- Summary --------
echo ""
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo -e "${RED}FAILED${NC}: $TESTS_FAILED test(s) failed"
    exit 1
fi
echo -e "${GREEN}OK${NC}: all tests passed"
exit 0
