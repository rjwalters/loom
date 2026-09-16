#!/usr/bin/env bash
# test-detect-startable-subset.sh - Tests for detect-startable-subset.sh
# (issue #5664, "recurred after closure").
#
# Champion's dependency handling only ever answered questions about the WHOLE
# issue ("is the blocker closed", "is there a cycle"). A proposal can declare a
# `## Startable Subset` naming part of its work that does not depend on an open
# blocker at all -- an explicit split point an architect stated so a Builder
# could land the unblocked half first. This suite covers the pure parsing
# function directly, and the CLI wrapper black-box via `--body-file` (no forge
# stub needed for that path).
#
# Hermetic: no network, no live forge, no tokens.
#
# Usage:
#   ./.loom/scripts/tests/test-detect-startable-subset.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
DSS="$SCRIPTS_DIR/detect-startable-subset.sh"

# Two `..` reaches repo-root/.claude/commands/loom for an INSTALLED copy
# (SCRIPTS_DIR is .loom/scripts there); one `..` reaches defaults/.claude/
# commands/loom when running inside this source repo (SCRIPTS_DIR is
# defaults/scripts) -- the two layouts differ in depth, so probe both rather
# than hard-coding one (#6725).
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../../.claude/commands/loom" && pwd)"
else
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../.claude/commands/loom" && pwd)"
fi
CHAMPION_PROMO_MD="$PROMPT_DIR/champion-issue-promo.md"

# The subject is now a `loom-daemon` subcommand behind a thin stub (epic #7810
# PR 3). Resolve the binary ONCE and pin it, preferring a repo build, so the
# stub cannot silently exec a stale installed copy instead of the build under
# test. In an installed consumer repo there is no repo build and this falls
# through to the installed binary exactly as before.
#
# FATAL, not SKIP: these assertions were written against the shell
# implementation and are the evidence that the port preserved its behaviour. A
# suite that skipped itself would drop that evidence while reporting green.
# shellcheck source=../lib/locate-daemon-bin.sh
source "$SCRIPTS_DIR/lib/locate-daemon-bin.sh"
LOOM_LOCATE_DAEMON_BIN_QUIET=1
LOOM_PREFER_REPO_BUILD=1
export LOOM_LOCATE_DAEMON_BIN_QUIET LOOM_PREFER_REPO_BUILD
DAEMON_BIN="$(loom_locate_daemon_bin "$(cd "$SCRIPTS_DIR/../.." && pwd)")"
if [[ -z "$DAEMON_BIN" ]]; then
    echo "FATAL: no loom-daemon binary found. Build it with" >&2
    echo "  cargo build --package loom-daemon" >&2
    echo "or set LOOM_DAEMON_BIN=/path/to/loom-daemon." >&2
    exit 1
fi
export LOOM_DAEMON_BIN="$DAEMON_BIN"
if ! "$DAEMON_BIN" detect-startable-subset --help >/dev/null 2>&1; then
    echo "FATAL: $DAEMON_BIN does not know 'detect-startable-subset'." >&2
    echo "It predates epic #7810 PR 3. Rebuild it: cargo build --package loom-daemon" >&2
    exit 1
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass "$msg"
    else
        fail "$msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        pass "$msg"
    else
        fail "$msg"
        echo "    Expected to contain: '$needle'"
        echo "    Actual: '$haystack'"
    fi
}

assert_true() { if "$@"; then pass "${!#}"; else fail "${!#}"; fi; }
assert_false() { if "$@"; then fail "${!#}"; else pass "${!#}"; fi; }

assert_doc_contains() {
    local file="$1" needle="$2" msg="$3"
    if grep -qF -- "$needle" "$file"; then
        pass "$msg"
    else
        fail "$msg (missing literal in $file: $needle)"
    fi
}

# =====================================================================
# Where the pure-helper unit tests went (epic #7810, PR 3)
# =====================================================================
#
# This suite used to `source "$DSS"` and call extract_startable_subset /
# has_startable_subset directly. Those functions are now
# `loom-daemon/src/dep_classify/subset.rs`, with their own unit tests covering
# the same cases — heading depth 2 through 6, prefix and case-insensitive
# matching, deeper subsections staying inside the section, and a
# whitespace-only section not counting as a declaration.
#
# #7943 landed that port beside a differential test that ran the Rust function
# and this shell function over the same fixtures and asserted they agreed. That
# test is deleted with the shell it compared against; the evidence is the merged
# CI run, not a fixture pinning a deleted file.
#
# What remains below is what can still be proven both ways: the black-box
# assertions, written against the shell, run unchanged against the Rust CLI.

# The fixture the black-box assertions below still use.
# shellcheck disable=SC2016  # literal text, not an expansion
BODY_WITH_SUBSET='## Summary
A proposal that hard-depends on #1 for most of its scope.

## Startable Subset

The comparator and mutation tests need only `warmup/01_netlist.v`, which is
already published upstream -- independent of the blocked RTL deliverable.

## Dependencies
- [ ] #1'

# =====================================================================
# Black-box: --body-file (no forge stub needed for this path)
# =====================================================================

echo
echo "--- CLI: --body-file ---"

TMP_BODY="$(mktemp)"
trap 'rm -f "$TMP_BODY"' EXIT

printf '%s' "$BODY_WITH_SUBSET" > "$TMP_BODY"
OUT="$("$DSS" --issue 3 --body-file "$TMP_BODY")"; RC=$?
assert_eq "0" "$RC" "exit 0 when a subset is declared"
assert_contains "$OUT" "STARTABLE_SUBSET" "STARTABLE_SUBSET marker present"
assert_contains "$OUT" "comparator and mutation tests" "the subset text is printed"

printf '%s' '## Summary
No subset.' > "$TMP_BODY"
OUT="$("$DSS" --issue 3 --body-file "$TMP_BODY")"; RC=$?
assert_eq "1" "$RC" "exit 1 when no subset is declared"
assert_contains "$OUT" "NO_STARTABLE_SUBSET" "NO_STARTABLE_SUBSET marker present"

echo
echo "--- CLI: argument validation ---"

OUT="$("$DSS" --repo o/r 2>&1)"; RC=$?
assert_eq "2" "$RC" "missing --issue exits 2"

OUT="$("$DSS" --issue notanumber --repo o/r 2>&1)"; RC=$?
assert_eq "2" "$RC" "non-numeric --issue exits 2"

OUT="$("$DSS" --issue 3 --body-file /nonexistent/path 2>&1)"; RC=$?
assert_eq "2" "$RC" "a missing --body-file exits 2"

# =====================================================================
# Doc pins: the Champion prose actually calls the detector
# =====================================================================

echo
echo "--- Doc pins: champion-issue-promo.md wires the carve-out ---"

assert_doc_contains "$CHAMPION_PROMO_MD" "detect-startable-subset.sh" \
    "champion-issue-promo.md invokes detect-startable-subset.sh"
assert_doc_contains "$CHAMPION_PROMO_MD" "Startable Subset" \
    "the convention (## Startable Subset) is documented in the role file"
assert_doc_contains "$CHAMPION_PROMO_MD" "Part of #" \
    "Champion's promotion guidance directs the Builder to the partial-increment convention"

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
