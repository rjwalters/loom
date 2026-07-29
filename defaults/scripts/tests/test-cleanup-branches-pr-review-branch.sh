#!/usr/bin/env bash
# test-cleanup-branches-pr-review-branch.sh - Tests for the pr-* review-branch
# cleanup pass added to cleanup-branches.sh (#4405)
#
# Judge/Doctor review-time branches (e.g. a bare `gh pr checkout` outside a
# managed worktree, or a scratch `-v2` iteration) never match
# `feature/issue-*`, so cleanup-branches.sh's original loop never saw them —
# they accumulated forever because a squash-merge repo can never classify
# them via `git branch --merged`. cleanup-branches.sh now additionally
# discovers branches shaped like `pr-<N>` / `pr<N>-*` / `pr-<N>-*`, resolves
# each to its originating PR, and deletes it only when that PR's state is
# MERGED or CLOSED — reusing merge-pr.sh's tip-SHA-verified
# `_maybe_delete_local_branch` helper rather than a raw `git branch -D`.
#
# Verifies:
#   1. Source contains the pr-* discovery regex and the PR-state gate.
#   2. Behavioral, end-to-end run of the REAL cleanup-branches.sh against a
#      throwaway git repo with a stubbed `gh`/forge on PATH:
#      (a) a pr-<N>-style branch whose PR is MERGED gets deleted.
#      (b) a pr-<N>-style branch whose PR is still OPEN is left alone.
#      (c) a feature/issue-<N> branch is processed by the pre-existing path
#          only — unaffected by the new pr-* pass (no double-processing).
#      (d) --dry-run reports the merged-PR branch as "would delete" without
#          actually deleting it.
#
# Companion to test-merge-pr-local-branch-cleanup.sh, which tests
# `_maybe_delete_local_branch` itself (the safety check this script reuses).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CLEANUP_SCRIPT="$SCRIPTS_DIR/cleanup-branches.sh"
MERGE_PR_SCRIPT="$SCRIPTS_DIR/merge-pr.sh"
DEFAULT_BRANCH_LIB="$SCRIPTS_DIR/lib/default-branch.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

assert_grep() {
    local pattern="$1" file="$2" msg="$3"
    if grep -qE "$pattern" "$file"; then pass "$msg"; else fail "$msg (pattern: $pattern)"; fi
}

[[ -x "$CLEANUP_SCRIPT" ]] || { echo "ERROR: $CLEANUP_SCRIPT not executable" >&2; exit 1; }
[[ -x "$MERGE_PR_SCRIPT" ]] || { echo "ERROR: $MERGE_PR_SCRIPT not executable" >&2; exit 1; }

# --- Test 1: source contains the pr-* discovery + reuse wiring ---
echo "Test 1: cleanup-branches.sh source contains the #4405 pr-* review-branch wiring"

assert_grep "grep -E '\\^pr-\\?\\[0-9\\]\\+\\(-\\.\\*\\)\\?\\\$'" "$CLEANUP_SCRIPT" \
    "discovers pr-<N> / pr<N>-* / pr-<N>-* branches via the shared regex"
if grep -qF '_MAYBE_DELETE_FN="$(awk '"'"'' "$CLEANUP_SCRIPT" && grep -qF '_maybe_delete_local_branch' "$CLEANUP_SCRIPT"; then
    pass "extracts the real _maybe_delete_local_branch() function body from merge-pr.sh (no duplication)"
else
    fail "extracts the real _maybe_delete_local_branch() function body from merge-pr.sh (no duplication)"
fi
assert_grep '_maybe_delete_local_branch "\$branch" "\$pr_head_sha"' "$CLEANUP_SCRIPT" \
    "invokes the extracted helper with the PR's head SHA for the tip-match safety check"
assert_grep 'pr view "\$pr_num" --json state,headRefOid' "$CLEANUP_SCRIPT" \
    "resolves PR state + head SHA via \$FORGE/gh pr view"
assert_grep 'pr_state" == "OPEN"' "$CLEANUP_SCRIPT" \
    "leaves the branch alone when its PR is still OPEN"
assert_grep 'pr_state" != "MERGED" && "\$pr_state" != "CLOSED"' "$CLEANUP_SCRIPT" \
    "only proceeds to delete when the PR is MERGED or CLOSED"
assert_grep '"\$1" == "--dry-run"' "$CLEANUP_SCRIPT" \
    "cleanup-branches.sh still supports --dry-run"
assert_grep 'would delete \$branch \(dry-run\)' "$CLEANUP_SCRIPT" \
    "--dry-run previews the pr-* branch it would delete without deleting it"

# --- Behavioral end-to-end tests ---
echo ""
echo "Test 2: end-to-end run of the real cleanup-branches.sh"

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/loom-cleanup-branches-pr.XXXXXX")"
TMP_ROOT="$(cd "$TMP_ROOT" && pwd -P)"
cleanup() { rm -rf "$TMP_ROOT" 2>/dev/null || true; }
trap cleanup EXIT

REPO="$TMP_ROOT/repo"
mkdir -p "$REPO"
git -C "$REPO" init -q
git -C "$REPO" config user.email "test@example.com"
git -C "$REPO" config user.name "Test"
echo "hello" > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m "initial"
git -C "$REPO" branch -M main
HEAD_SHA="$(git -C "$REPO" rev-parse HEAD)"

# Scratch scripts/ tree the fake `gh` and cleanup-branches.sh both need
# (cleanup-branches.sh resolves merge-pr.sh and lib/default-branch.sh
# relative to its own SCRIPT_DIR).
mkdir -p "$REPO/scripts/lib"
cp "$CLEANUP_SCRIPT" "$REPO/scripts/cleanup-branches.sh"
cp "$MERGE_PR_SCRIPT" "$REPO/scripts/merge-pr.sh"
[[ -f "$DEFAULT_BRANCH_LIB" ]] && cp "$DEFAULT_BRANCH_LIB" "$REPO/scripts/lib/default-branch.sh"
chmod +x "$REPO/scripts/cleanup-branches.sh" "$REPO/scripts/merge-pr.sh"

# Branches under test:
#   pr-100      -> PR #100, MERGED, tip == HEAD_SHA  -> should be deleted
#   pr-200-old  -> PR #200, OPEN                      -> must be kept
#   feature/issue-300 -> issue #300, OPEN              -> must be kept
#     (also proves the pre-existing feature/issue-* path is unaffected by
#      the new pr-* pass — it is never matched by the pr-* regex)
git -C "$REPO" branch pr-100 main
git -C "$REPO" branch pr-200-old main
git -C "$REPO" branch feature/issue-300 main

FAKE_BIN="$TMP_ROOT/fakebin"
mkdir -p "$FAKE_BIN"
cat > "$FAKE_BIN/gh" <<EOF
#!/bin/bash
if [[ "\$1" == "issue" && "\$2" == "view" ]]; then
  case "\$3" in
    300) echo "OPEN" ;;
    *) echo "NOT_FOUND" ;;
  esac
  exit 0
fi
if [[ "\$1" == "pr" && "\$2" == "view" ]]; then
  case "\$3" in
    100) echo '{"state":"MERGED","headRefOid":"$HEAD_SHA"}' ;;
    101) echo '{"state":"MERGED","headRefOid":"$HEAD_SHA"}' ;;
    200) echo '{"state":"OPEN","headRefOid":"$HEAD_SHA"}' ;;
    *) exit 1 ;;
  esac
  exit 0
fi
exit 1
EOF
chmod +x "$FAKE_BIN/gh"

# --- (a)+(b)+(c): real run deletes the merged pr-* branch, keeps the open
#     pr-* branch and the open feature/issue-* branch untouched ---
run_output="$(cd "$REPO" && PATH="$FAKE_BIN:$PATH" ./scripts/cleanup-branches.sh 2>&1)"

if git -C "$REPO" show-ref --verify --quiet refs/heads/pr-100; then
    fail "(a) pr-100 (PR #100 MERGED) should have been deleted; still present"
else
    pass "(a) pr-<N> branch whose PR is MERGED gets deleted"
fi

if git -C "$REPO" show-ref --verify --quiet refs/heads/pr-200-old; then
    pass "(b) pr-<N>-style branch whose PR is still OPEN is left alone"
else
    fail "(b) pr-200-old (PR #200 OPEN) was unexpectedly deleted"
fi

if git -C "$REPO" show-ref --verify --quiet refs/heads/feature/issue-300; then
    pass "(c) feature/issue-<N> branch is unaffected by the new pr-* pass (issue still OPEN, kept)"
else
    fail "(c) feature/issue-300 was unexpectedly deleted"
fi

if [[ "$run_output" == *"PR #100 is MERGED"* ]] && [[ "$run_output" == *"deleted"* ]]; then
    pass "output reports PR #100 as MERGED and the branch as deleted"
else
    fail "expected MERGED+deleted reporting for PR #100; got: $run_output"
fi

if [[ "$run_output" == *"PR #200 is OPEN - keeping pr-200-old"* ]]; then
    pass "output reports PR #200 as OPEN and keeps pr-200-old"
else
    fail "expected OPEN+keep reporting for PR #200; got: $run_output"
fi

# --- (d): --dry-run never deletes, only previews ---
git -C "$REPO" branch pr-101 main
dry_output="$(cd "$REPO" && PATH="$FAKE_BIN:$PATH" ./scripts/cleanup-branches.sh --dry-run 2>&1)"
if [[ "$dry_output" == *"would delete pr-101 (dry-run)"* ]] || [[ "$dry_output" == *"pr-101"* && "$dry_output" == *"dry-run"* ]]; then
    if git -C "$REPO" show-ref --verify --quiet refs/heads/pr-101; then
        pass "(d) --dry-run previews the merged-PR branch without deleting it"
    else
        fail "(d) --dry-run must not actually delete the branch; pr-101 is gone"
    fi
else
    fail "(d) expected a dry-run preview mentioning pr-101; got: $dry_output"
fi
git -C "$REPO" branch -D pr-101 >/dev/null 2>&1 || true

# --- Summary ---
echo ""
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
