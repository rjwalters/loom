#!/usr/bin/env bash
# test-changelog.sh - Unit tests for scripts/changelog.sh (#5196).
#
# Builds a disposable scratch git repo with controlled conventional-commit
# subjects (one per bucketing rule, plus the two "don't silently drop"
# edge cases: an unrecognized prefix and no prefix at all), then asserts
# `changelog.sh draft`'s bucketing, ref preservation, and determinism, plus
# `changelog.sh verify`'s ref-coverage check. Uses CHANGELOG_REPO_ROOT so
# nothing here touches this repo's own history or CHANGELOG.md.
#
# Usage: bash scripts/test-changelog.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CHANGELOG_SH="$REPO_ROOT/scripts/changelog.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

passed=0
failed=0

pass() { echo -e "${GREEN}✓${NC} $1"; passed=$((passed + 1)); }
fail() { echo -e "${RED}✗${NC} $1"; failed=$((failed + 1)); }

assert_contains() {
  local haystack="$1" needle="$2" desc="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    pass "$desc"
  else
    fail "$desc (expected to find: $needle)"
  fi
}

assert_not_contains() {
  local haystack="$1" needle="$2" desc="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    fail "$desc (unexpectedly found: $needle)"
  else
    pass "$desc"
  fi
}

SCRATCH="$(mktemp -d)"
cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

git -C "$SCRATCH" init --quiet --initial-branch=main
git -C "$SCRATCH" config user.name "Loom Test"
git -C "$SCRATCH" config user.email "test@example.invalid"

commit() {
  local subject="$1"
  # Filename varies per commit so each produces a distinct, real change --
  # `git commit --allow-empty` would also work, but a real diff is closer to
  # how these subjects actually arrive in this repo's history.
  echo "$subject" >> "$SCRATCH/log.txt"
  git -C "$SCRATCH" add log.txt
  git -C "$SCRATCH" commit --quiet -m "$subject"
}

echo "=== Building scratch commit range ==="
commit "chore: initial scaffold"
git -C "$SCRATCH" tag v0.0.0
commit "feat(cli): add --verbose flag (#101)"
commit "fix(daemon): correct off-by-one in retry loop (#102)"
commit "docs: clarify install steps (#103)"
commit "refactor(core): extract shared helper (#104)"
commit "perf(query): cache repeated lookups (#105)"
commit "revert: revert \"feat: risky experiment\" (#106)"
commit "test(daemon): add coverage for retry loop (#107)"
commit "chore: bump lockfile"
commit "ci: pin runner image (#108)"
commit "build: bump toolchain (#109)"
commit "config(loom): enable a feature flag (#110)"
commit "totally unstructured commit message with no prefix (#111)"
git -C "$SCRATCH" tag v0.1.0

RANGE="v0.0.0..v0.1.0"

echo ""
echo "=== draft: bucketing ==="
DRAFT1="$(CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" draft "$RANGE")"

assert_contains "$DRAFT1" "### Added" "Added section header present"
assert_contains "$DRAFT1" "add --verbose flag (#101)" "feat commit bucketed under Added, ref preserved"

assert_contains "$DRAFT1" "### Fixed" "Fixed section header present"
assert_contains "$DRAFT1" "correct off-by-one in retry loop (#102)" "fix commit bucketed under Fixed"

assert_contains "$DRAFT1" "### Changed" "Changed section header present"
assert_contains "$DRAFT1" "clarify install steps (#103)" "docs commit bucketed under Changed"
assert_contains "$DRAFT1" "extract shared helper (#104)" "refactor commit bucketed under Changed"
assert_contains "$DRAFT1" "cache repeated lookups (#105)" "perf commit bucketed under Changed"

assert_contains "$DRAFT1" "### Removed" "Removed section header present"
assert_contains "$DRAFT1" 'revert "feat: risky experiment" (#106)' "revert commit bucketed under Removed"

assert_contains "$DRAFT1" "### Other" "Other section header present (unrecognized/no-prefix commits)"
assert_contains "$DRAFT1" "enable a feature flag (#110)" "unrecognized-prefix commit surfaced under Other, not dropped"
assert_contains "$DRAFT1" "totally unstructured commit message with no prefix (#111)" "no-prefix commit surfaced under Other, not dropped"

echo ""
echo "=== draft: non-shipping types excluded ==="
assert_not_contains "$DRAFT1" "add coverage for retry loop" "test commit excluded"
assert_not_contains "$DRAFT1" "bump lockfile" "chore commit excluded"
assert_not_contains "$DRAFT1" "pin runner image" "ci commit excluded"
assert_not_contains "$DRAFT1" "bump toolchain" "build commit excluded"

echo ""
echo "=== draft: determinism ==="
DRAFT2="$(CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" draft "$RANGE")"
if [ "$DRAFT1" = "$DRAFT2" ]; then
  pass "re-running draft on the same range is byte-identical"
else
  fail "re-running draft on the same range produced different output"
fi

echo ""
echo "=== draft: empty range does not error ==="
EMPTY_OUT="$(CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" draft "v0.1.0..v0.1.0")"
assert_contains "$EMPTY_OUT" "no shipping changes" "empty range prints a valid empty-but-headed skeleton, no error"

echo ""
echo "=== verify: detects a missing ref ==="
CHANGELOG_STUB="$SCRATCH/CHANGELOG.md"
{
  echo "## [0.1.0]"
  echo "- add --verbose flag (#101)"
  echo "- correct off-by-one in retry loop (#102)"
  # #103-#111 deliberately omitted.
} > "$CHANGELOG_STUB"

VERIFY_OUT="$(CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" verify "$RANGE" "$CHANGELOG_STUB" 2>&1)" && VERIFY_EXIT=0 || VERIFY_EXIT=$?
if [ "$VERIFY_EXIT" -ne 0 ]; then
  pass "verify exits non-zero when a shipping ref is missing"
else
  fail "verify should have exited non-zero (missing refs)"
fi
assert_contains "$VERIFY_OUT" "MISSING: #103" "verify reports the missing docs ref"
assert_not_contains "$VERIFY_OUT" "MISSING: #107" "verify does not flag an excluded test commit's ref"
assert_not_contains "$VERIFY_OUT" "MISSING: #108" "verify does not flag an excluded ci commit's ref"

echo ""
echo "=== verify: passes when every shipping ref is present ==="
CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" draft "$RANGE" > "$CHANGELOG_STUB"
if CHANGELOG_REPO_ROOT="$SCRATCH" bash "$CHANGELOG_SH" verify "$RANGE" "$CHANGELOG_STUB" > /dev/null 2>&1; then
  pass "verify exits zero when every shipping ref is present"
else
  fail "verify should have exited zero (all refs present, since CHANGELOG_STUB is the draft itself)"
fi

echo ""
echo "=== usage: unknown subcommand exits non-zero ==="
if bash "$CHANGELOG_SH" bogus > /dev/null 2>&1; then
  fail "unknown subcommand should exit non-zero"
else
  pass "unknown subcommand exits non-zero"
fi

echo ""
echo "=== Results: $passed passed, $failed failed ==="
[ "$failed" -eq 0 ]
