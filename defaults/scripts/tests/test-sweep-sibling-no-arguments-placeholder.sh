#!/usr/bin/env bash
# test-sweep-sibling-no-arguments-placeholder.sh - Regression test for issue
# #14659.
#
# The `/loom:sweep` dispatcher (`sweep.md`) is the only sweep file that may
# contain the `$ARGUMENTS` harness placeholder: it is the only file that
# receives the invocation's arguments. Sibling reference files
# (`sweep-arguments.md`, `sweep-backend-detection.md`, `sweep-wave-lifecycle.md`,
# …) are also exposed as standalone skills and loaded with NO arguments, so a
# placeholder there renders as the EMPTY string — the file whose whole job is
# classifying the invocation's arguments would render without them, and an
# agent following the text literally would classify an empty token list (the
# usage-error path).
#
# Verifies:
#   1. No sibling `sweep-*.md` (any sweep file other than `sweep.md`) contains
#      the literal `$ARGUMENTS`; siblings must say "the arguments passed to
#      `/loom:sweep`" instead.
#   2. `sweep.md` retains exactly one `$ARGUMENTS` — the load-order
#      interpolation point that carries the invocation's arguments into the
#      dispatcher's context. A second occurrence would interpolate somewhere
#      unintended.
#   3. Self-check: the detection grep actually flags a placeholder
#      re-introduced into a sibling (fails against an unfixed copy).
#
# Hermetic: pure grep over repo files; no forge, network, or tokens.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# Subject is SHIPPED (installed into a consumer repo as
# .claude/commands/loom/): resolve the installed copy first, falling back to
# the defaults/ source-tree path, so the suite runs in both layouts
# (#6194/#6241).
if [[ -d "$REPO_ROOT/.claude/commands/loom" ]]; then
    COMMANDS_DIR="$REPO_ROOT/.claude/commands/loom"
else
    COMMANDS_DIR="$REPO_ROOT/defaults/.claude/commands/loom"
fi

source "$SCRIPT_DIR/lib/assert.sh"

if [[ ! -d "$COMMANDS_DIR" ]]; then
    echo "FATAL: sweep command dir not found at $COMMANDS_DIR"; exit 1
fi
SWEEP_MD="$COMMANDS_DIR/sweep.md"
if [[ ! -f "$SWEEP_MD" ]]; then
    echo "FATAL: sweep.md not found at $SWEEP_MD"; exit 1
fi

find_offenders() { # $1 = dir to scan; prints basenames of offending siblings
    local f
    while IFS= read -r f; do
        grep -q -F '$ARGUMENTS' "$f" && basename "$f"
    done < <(find "$1" -maxdepth 1 -name 'sweep-*.md' | sort)
}

echo "Test 1: no sibling sweep-*.md contains the \$ARGUMENTS placeholder"
SIBLING_COUNT="$(find "$COMMANDS_DIR" -maxdepth 1 -name 'sweep-*.md' | wc -l)"
if [[ "$SIBLING_COUNT" -lt 1 ]]; then
    fail "no sibling sweep-*.md files found under $COMMANDS_DIR (detection is vacuous)"
fi
OFFENDERS="$(find_offenders "$COMMANDS_DIR" | tr '\n' ' ')"
if [[ -z "${OFFENDERS// /}" ]]; then
    pass "no sibling sweep-*.md contains \$ARGUMENTS ($SIBLING_COUNT siblings checked)"
else
    fail "sibling sweep-*.md files containing \$ARGUMENTS: $OFFENDERS"
fi

echo ""
echo "Test 2: sweep.md retains exactly one \$ARGUMENTS (the load-order interpolation point)"
COUNT="$(grep -c -F '$ARGUMENTS' "$SWEEP_MD" || true)"
assert_eq "1" "$COUNT" "sweep.md has exactly one \$ARGUMENTS interpolation"

echo ""
echo "Test 3: the check fails when \$ARGUMENTS is re-introduced into a sibling"
TMPROOT="$(mktemp -d)"; trap 'rm -rf "$TMPROOT"' EXIT
mkdir -p "$TMPROOT/commands"
find "$COMMANDS_DIR" -maxdepth 1 -name 'sweep-*.md' -exec cp {} "$TMPROOT/commands/" \;
printf '\n`$ARGUMENTS` must not appear in a sibling\n' \
    >> "$TMPROOT/commands/sweep-wave-lifecycle.md"
BAD="$(find_offenders "$TMPROOT/commands" | tr '\n' ' ')"
if [[ "${BAD// /}" == "sweep-wave-lifecycle.md" ]]; then
    pass "re-introduced placeholder is detected in the mutated copy"
else
    fail "expected 'sweep-wave-lifecycle.md' flagged in mutated copy, got: '${BAD:-none}'"
fi

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"; exit 1; fi
echo "All tests passed"
exit 0
