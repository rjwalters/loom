#!/usr/bin/env bash
# Test suite for the stateless ceremony heuristic in hermit.md / hermit-patterns.md
#
# Usage: ./tests/hermit/test-stateless-ceremony.sh
#
# The detector under test is EXTRACTED VERBATIM from the shipped role file
# (defaults/.claude/commands/loom/hermit-patterns.md, section
# "9. Stateless Ceremony -> Detection scripts"), so this suite exercises what
# Hermit actually runs -- not a pasted copy that can silently drift (#13884).
# The block itself walks '.', so the suite runs it with the fixtures
# directory as the working directory.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PATTERNS_FILE="$REPO_ROOT/defaults/.claude/commands/loom/hermit-patterns.md"

PASS=0
FAIL=0
TOTAL=0

# Colors (if terminal supports them)
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

# Extract the python3 -c detector block from the role file, verbatim: the
# lines between the `python3 -c "` opener and the lone closing `"` line,
# inside the bash fence that contains them.
extract_detector() {
    awk '
        /^```bash$/ { fence = 1; next }
        /^```$/     { fence = 0; next }
        fence && /^python3 -c "$/ { capture = 1; next }
        capture && /^"$/          { exit }
        capture { print }
    ' "$PATTERNS_FILE"
}

# Run the shipped detector against a directory.
run_detector() {
    local target_dir="$1"
    local block
    block="$(extract_detector)"
    if [ -z "$block" ]; then
        echo "ERROR: could not extract the python3 -c detector block from $PATTERNS_FILE" >&2
        return 1
    fi
    ( cd "$target_dir" && python3 -c "$block" ) 2>&1
}

DETECTOR_OUTPUT="$(run_detector "$FIXTURES")"

# Assert that a class name appears in detector output (should be flagged)
assert_flagged() {
    local _fixture="$1"   # documents which fixture the assertion targets
    local class_name="$2"
    local description="$3"
    TOTAL=$((TOTAL + 1))

    if echo "$DETECTOR_OUTPUT" | grep -q "$class_name"; then
        PASS=$((PASS + 1))
        printf "${GREEN}PASS${NC}: %s\n" "$description"
    else
        FAIL=$((FAIL + 1))
        printf "${RED}FAIL${NC}: %s\n" "$description"
        printf "  Expected class '%s' to be flagged but it was not.\n" "$class_name"
        printf "  Detector output:\n%s\n" "$DETECTOR_OUTPUT"
    fi
}

# Assert that a class name does NOT appear in detector output (should be excluded)
assert_not_flagged() {
    local _fixture="$1"   # documents which fixture the assertion targets
    local class_name="$2"
    local description="$3"
    TOTAL=$((TOTAL + 1))

    if echo "$DETECTOR_OUTPUT" | grep -q "$class_name"; then
        FAIL=$((FAIL + 1))
        printf "${RED}FAIL${NC}: %s\n" "$description"
        printf "  Expected class '%s' to NOT be flagged but it was.\n" "$class_name"
        printf "  Detector output:\n%s\n" "$DETECTOR_OUTPUT"
    else
        PASS=$((PASS + 1))
        printf "${GREEN}PASS${NC}: %s\n" "$description"
    fi
}

echo "=== Hermit Stateless Ceremony Heuristic Tests ==="
echo "detector extracted from: $PATTERNS_FILE"
echo ""

# --- Flagged: genuine stateless ceremonies ---

# Test 1: Genuinely stateless class SHOULD be flagged
assert_flagged "genuine_stateless.py" "PatternAdapter" \
    "Genuinely stateless class (no self.x=, no self.method() calls, <10 methods) is flagged"

# Test 2: Nested defs must NOT inflate the method count -- a class with <10
# real methods but >=10 defs including nested ones is still flagged (#13884)
assert_flagged "nested_def_namespace.py" "AlgorithmRunner" \
    "Class with 2 real methods + 8 nested defs is flagged (method count uses node.body, not ast.walk)"

# --- Not flagged: existing exclusions (regression checks) ---

# Test 3: Dispatch-table class with self.method() calls should NOT be flagged (Exclusion 1)
assert_not_flagged "dispatch_table.py" "CommandRouter" \
    "Dispatch-table class with self.method() calls is excluded (Exclusion 1: internal dispatch)"

# Test 4: Large namespace class (10+ methods) should NOT be flagged (Exclusion 2)
assert_not_flagged "large_namespace.py" "MathUtils" \
    "Large namespace class with 11 methods is excluded (Exclusion 2: method count >= 10)"

# Test 5: Dispatch-table pattern with dict of self._method refs should NOT be flagged (Exclusion 3)
assert_not_flagged "dispatch_dict_pattern.py" "EventProcessor" \
    "Class with dict of self._method references is excluded (Exclusion 3: dispatch-table pattern)"

# Test 6: Stateful class should NOT be flagged (existing behavior, no regression)
assert_not_flagged "stateful_class.py" "Counter" \
    "Stateful class (self.count = 0) is not flagged (regression check)"

# Test 7: Class with both state and self.method() calls should NOT be flagged (regression)
assert_not_flagged "self_method_and_state.py" "Processor" \
    "Class with state AND self.method() calls is not flagged (regression check)"

# --- Not flagged: widened state test (#13884 root cause 1) ---

# Test 8: Tuple-assignment state (self.a, self.b = ...) is instance state
assert_not_flagged "tuple_assign_state.py" "PairHolder" \
    "Tuple assignment (self.a, self.b = a, b) counts as instance state"

# Test 9: Annotated assignment (AnnAssign) state is instance state
assert_not_flagged "annassign_state.py" "Config" \
    "Annotated assignment (self.timeout: int = 30) counts as instance state"

# Test 10: AnnAssign without a value must not crash or mask later assignment
assert_not_flagged "annassign_state.py" "DeclaredOnly" \
    "Bare AnnAssign declaration (self.size: int) does not crash the detector"

# Test 11: Augmented assignment (AugAssign) state is instance state
assert_not_flagged "augassign_state.py" "Tally" \
    "Augmented assignment (self.count += 1) counts as instance state"

# --- Not flagged: type-based exclusions (#13884 root cause 2) ---

# Test 12: @dataclass classes are excluded (generated __init__ is the state)
assert_not_flagged "dataclass_holder.py" "Point" \
    "@dataclass class is excluded (generated __init__ is the state)"

# Test 13: @dataclass(frozen=True) likewise
assert_not_flagged "dataclass_holder.py" "FrozenPoint" \
    "@dataclass(frozen=True) class is excluded"

# Test 14: Exception subclass is excluded (no state by design)
assert_not_flagged "exception_subclass.py" "Refusal" \
    "Exception subclass is excluded (any base class = structural type)"

# Test 15: Exception subclass with a non-Exception base too
assert_not_flagged "exception_subclass.py" "ValidationError" \
    "ValueError subclass is excluded"

# Test 16: Polymorphic subclasses are excluded (any base class)
assert_not_flagged "polymorphic_subclass.py" "FileSink" \
    "Polymorphic subclass FileSink is excluded (any base class)"

assert_not_flagged "polymorphic_subclass.py" "NullSink" \
    "Polymorphic subclass NullSink is excluded (any base class)"

# Test 17: The polymorphic base itself is excluded via internal self.* dispatch
assert_not_flagged "polymorphic_subclass.py" "Sink" \
    "Template-method base is excluded (internal self.render() dispatch)"

# Test 18: Setter-property state is excluded (@x.setter pair)
assert_not_flagged "setter_property.py" "Temperature" \
    "Class whose state flows through a @x.setter pair is excluded"

assert_not_flagged "setter_property.py" "Dial" \
    "Conventional property pair class is not flagged"

# --- Not flagged: test-double files skipped by name (#13884) ---

# Test 19: test_*.py files are skipped
assert_not_flagged "test_fake_repo.py" "FakeGit" \
    "test_*.py files are skipped (test doubles dominate false positives)"

# Test 20: *_stub*.py files are skipped
assert_not_flagged "git_stub.py" "StubGit" \
    "*_stub*.py files are skipped"

# Test 21: conftest.py is skipped
assert_not_flagged "conftest.py" "ConfFake" \
    "conftest.py is skipped"

echo ""
echo "=== Results: $PASS passed, $FAIL failed, $TOTAL total ==="

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
