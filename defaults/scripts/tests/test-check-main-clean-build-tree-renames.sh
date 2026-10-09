#!/usr/bin/env bash
# test-check-main-clean-build-tree-renames.sh - check-main-clean.sh --quarantine
# at the edges of the cargo build-tree filter (#11149, follow-ups to #11075).
#
# A sibling of test-check-main-clean.sh, which is at the file-size ceiling.
#
# Verified behavior:
#   - a staged `git mv` INTO a build tree: the quarantine exits 4, the stash
#     carries the source's deletion, and the build tree is neither stashed nor
#     removed from disk (11149a)
#   - a `loom-daemon stashes build-trees` that prints dirs and then FAILS
#     excludes nothing, so its "will NOT be excluded" warning is true (11149b)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WS_STUB_DIR="$(mktemp -d)"
# shellcheck source=lib/write-scope-stub.sh
source "$SCRIPT_DIR/lib/write-scope-stub.sh"
write_scope_allow_all "$WS_STUB_DIR"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/check-main-clean.sh"
# The rename case needs THIS checkout's `stashes build-trees` (the fix is in it).
# shellcheck source=lib/require-daemon-bin.sh
source "$SCRIPT_DIR/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin --self-only "$HELPERS_DIR" "stashes"

TESTS_RUN=0
TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }

# A repo with one committed source file (as test-check-main-clean.sh builds).
make_repo_with_source() {
    local dir
    dir=$(mktemp -d)
    git -C "$dir" init -q
    git -C "$dir" config user.email t@t.t
    git -C "$dir" config user.name test
    printf '.loom/worktrees/\n.loom/sweep-checkpoint/\n' > "$dir/.gitignore"
    printf 'original tracked content\n' > "$dir/tracked_source.py"
    git -C "$dir" add .gitignore tracked_source.py
    git -C "$dir" commit -q -m init
    echo "$dir"
}

# stash_files <repo> -> every path in stash@{0}'s worktree, index and untracked trees.
stash_files() {
    local r
    for r in 'stash@{0}' 'stash@{0}^2' 'stash@{0}^3'; do
        git -C "$1" ls-tree -r --name-only "$r" 2>/dev/null || true
    done
}

# -------- Test: a rename INTO a build tree still rescues the source deletion (#11149) --------
echo "Test 11149a: git mv into a build tree -> the source deletion is stashed, the tree is not"
REPO=$(make_repo_with_source)
printf 'old\n' > "$REPO/old.txt"
git -C "$REPO" add old.txt && git -C "$REPO" commit -qm "add old.txt"
SNAP="$REPO/.loom/sweep-checkpoint/main-clean-baseline-11149a.txt"
( cd "$REPO" && "$SCRIPT" --snapshot "$SNAP" >/dev/null 2>&1 )
mkdir -p "$REPO/target-x/debug"
printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > "$REPO/target-x/CACHEDIR.TAG"
printf 'bin\n' > "$REPO/target-x/debug/artifact.o"
git -C "$REPO" mv old.txt target-x/b.o
out=$( cd "$REPO" && LOOM_QUARANTINE_COMMENT=0 "$SCRIPT" --baseline "$SNAP" --quarantine --label "run=R issue=11149" 2>&1 ); RC=$?
if [[ "$RC" -eq 4 ]]; then pass "rename-into-build-tree quarantine exits 4 (no STILL PRESENT)"; else fail "expected 4, got $RC; out=$out"; fi
if grep -q $'^D\told.txt$' <<<"$(git -C "$REPO" diff --name-status 'stash@{0}^1' 'stash@{0}' 2>/dev/null)"; then
    pass "the stash carries the old.txt deletion"
else
    fail "old.txt deletion missing from the stash; out=$out"
fi
STASH_FILES=$(stash_files "$REPO")
if grep -q '^target-x/' <<<"$STASH_FILES"; then
    fail "build-tree content leaked into refs/stash: $STASH_FILES"
elif [[ -f "$REPO/target-x/b.o" && -f "$REPO/target-x/debug/artifact.o" ]]; then
    pass "build-tree content kept out of refs/stash and left on disk"
else
    fail "build-tree content was removed from disk"
fi
rm -rf "${REPO:?}"

# -------- Test: a daemon that fails AFTER printing dirs excludes nothing (#11149) --------
echo "Test 11149b: partial build-trees output then failure -> warning holds, nothing excluded"
STUB_DIR=$(mktemp -d)
printf '#!/usr/bin/env bash\nprintf "target-x\\0"\necho "error: boom" >&2\nexit 1\n' > "$STUB_DIR/loom-daemon"
chmod +x "$STUB_DIR/loom-daemon"
REPO=$(make_repo_with_source)
SNAP="$REPO/.loom/sweep-checkpoint/main-clean-baseline-11149b.txt"
( cd "$REPO" && "$SCRIPT" --snapshot "$SNAP" >/dev/null 2>&1 )
mkdir -p "$REPO/target-x/debug"
printf 'Signature: 8a477f597d28d172789f06886806bc55\n' > "$REPO/target-x/CACHEDIR.TAG"
printf 'bin\n' > "$REPO/target-x/debug/artifact.o"
printf 'def leaked(): pass\n' > "$REPO/leaked_module.py"
out=$( cd "$REPO" && LOOM_QUARANTINE_COMMENT=0 LOOM_DAEMON_SELF_BIN="$STUB_DIR/loom-daemon" \
    "$SCRIPT" --baseline "$SNAP" --quarantine --label "run=R issue=11149" 2>&1 ); RC=$?
STASH_FILES=$(stash_files "$REPO")
if [[ "$RC" -eq 4 ]] && grep -q 'NOT be excluded' <<<"$out"; then
    pass "partial output: quarantine exits 4 with the fallback warning"
else
    fail "expected 4 plus the warning, got rc=$RC; out=$out"
fi
if grep -q '^target-x/debug/artifact.o$' <<<"$STASH_FILES" && grep -q '^leaked_module.py$' <<<"$STASH_FILES"; then
    pass "partial output: the printed dir is NOT excluded (include-everything, as warned)"
else
    fail "partial daemon output still drove an exclusion; stash holds: $STASH_FILES"
fi
rm -rf "${REPO:?}" "${STUB_DIR:?}"

echo ""
if [[ "$TESTS_FAILED" -eq 0 ]]; then
    echo "All $TESTS_RUN/$TESTS_RUN tests passed"
    exit 0
fi
echo "FAILED: $TESTS_FAILED/$TESTS_RUN tests failed"
exit 1
