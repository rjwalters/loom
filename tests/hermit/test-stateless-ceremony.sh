#!/usr/bin/env bash
# Test suite for the stateless ceremony heuristic in hermit.md / hermit-patterns.md
#
# Usage: ./tests/hermit/test-stateless-ceremony.sh
#
# Runs the AST-based detection script against fixture Python files and verifies
# that dispatch-table classes, large-namespace classes, and other false-positive
# patterns are correctly excluded while genuinely stateless classes are still flagged.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures"

PASS=0
FAIL=0
TOTAL=0

# Colors (if terminal supports them)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Run the AST detection script against a single directory
run_detector() {
    local target_dir="$1"
    python3 -c "
import ast, sys, os
for root, dirs, files in os.walk('${target_dir}'):
    dirs[:] = [d for d in dirs if not d.startswith('.') and d != 'node_modules']
    for f in files:
        if not f.endswith('.py'): continue
        path = os.path.join(root, f)
        try:
            tree = ast.parse(open(path).read())
        except: continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ClassDef): continue
            has_self_assign = any(
                isinstance(n, ast.Assign) and
                any(isinstance(t, ast.Attribute) and
                    isinstance(t.value, ast.Name) and t.value.id == 'self'
                    for t in n.targets)
                for n in ast.walk(node)
            )
            if has_self_assign:
                continue
            has_self_method_call = any(
                isinstance(n, ast.Call) and
                isinstance(getattr(n, 'func', None), ast.Attribute) and
                isinstance(getattr(n.func, 'value', None), ast.Name) and
                n.func.value.id == 'self'
                for n in ast.walk(node)
            )
            if has_self_method_call:
                continue
            method_count = sum(
                1 for n in ast.walk(node)
                if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))
            )
            if method_count >= 10:
                continue
            has_dispatch_table = False
            for n in ast.walk(node):
                if not isinstance(n, (ast.Dict, ast.List, ast.Set)):
                    continue
                for val in ast.walk(n):
                    if (isinstance(val, ast.Attribute) and
                        isinstance(getattr(val, 'value', None), ast.Name) and
                        val.value.id == 'self'):
                        has_dispatch_table = True
                        break
                if has_dispatch_table:
                    break
            if has_dispatch_table:
                continue
            print(f'{path}:{node.lineno}: {node.name} (no instance state)')
" 2>&1
}

# Assert that a class name appears in detector output (should be flagged)
assert_flagged() {
    local fixture="$1"
    local class_name="$2"
    local description="$3"
    TOTAL=$((TOTAL + 1))

    local output
    output=$(run_detector "$FIXTURES")

    if echo "$output" | grep -q "$class_name"; then
        PASS=$((PASS + 1))
        printf "${GREEN}PASS${NC}: %s\n" "$description"
    else
        FAIL=$((FAIL + 1))
        printf "${RED}FAIL${NC}: %s\n" "$description"
        printf "  Expected class '%s' to be flagged but it was not.\n" "$class_name"
        printf "  Detector output:\n%s\n" "$output"
    fi
}

# Assert that a class name does NOT appear in detector output (should be excluded)
assert_not_flagged() {
    local fixture="$1"
    local class_name="$2"
    local description="$3"
    TOTAL=$((TOTAL + 1))

    local output
    output=$(run_detector "$FIXTURES")

    if echo "$output" | grep -q "$class_name"; then
        FAIL=$((FAIL + 1))
        printf "${RED}FAIL${NC}: %s\n" "$description"
        printf "  Expected class '%s' to NOT be flagged but it was.\n" "$class_name"
        printf "  Detector output:\n%s\n" "$output"
    else
        PASS=$((PASS + 1))
        printf "${GREEN}PASS${NC}: %s\n" "$description"
    fi
}

echo "=== Hermit Stateless Ceremony Heuristic Tests ==="
echo ""

# Test 1: Genuinely stateless class SHOULD be flagged
assert_flagged "genuine_stateless.py" "PatternAdapter" \
    "Genuinely stateless class (no self.x=, no self.method() calls, <10 methods) is flagged"

# Test 2: Dispatch-table class with self.method() calls should NOT be flagged (Exclusion 1)
assert_not_flagged "dispatch_table.py" "CommandRouter" \
    "Dispatch-table class with self.method() calls is excluded (Exclusion 1: internal dispatch)"

# Test 3: Large namespace class (10+ methods) should NOT be flagged (Exclusion 2)
assert_not_flagged "large_namespace.py" "MathUtils" \
    "Large namespace class with 11 methods is excluded (Exclusion 2: method count >= 10)"

# Test 4: Dispatch-table pattern with dict of self._method refs should NOT be flagged (Exclusion 3)
assert_not_flagged "dispatch_dict_pattern.py" "EventProcessor" \
    "Class with dict of self._method references is excluded (Exclusion 3: dispatch-table pattern)"

# Test 5: Stateful class should NOT be flagged (existing behavior, no regression)
assert_not_flagged "stateful_class.py" "Counter" \
    "Stateful class (self.count = 0) is not flagged (regression check)"

# Test 6: Class with both state and self.method() calls should NOT be flagged (regression)
assert_not_flagged "self_method_and_state.py" "Processor" \
    "Class with state AND self.method() calls is not flagged (regression check)"

echo ""
echo "=== Results: $PASS passed, $FAIL failed, $TOTAL total ==="

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
