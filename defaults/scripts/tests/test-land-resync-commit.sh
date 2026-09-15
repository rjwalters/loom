#!/usr/bin/env bash
# test-land-resync-commit.sh - Smoke tests for land-resync-commit.sh (#6646)
#
# Constructs throwaway git repos (a bare "origin" + a primary checkout clone,
# plus a "third-party" clone standing in for another merged PR) to exercise
# the load-bearing cases:
#   (a) clean tree                          -> no-op, exit 0
#   (b) resync-only dirt, no divergence     -> committed + pushed directly, exit 0
#   (c) resync dirt + a NON-Loom-authored   -> commits the resync, refuses to
#       commit already ahead of origin         push or rebase, exit 3; the
#                                               operator's commit SHA is
#                                               untouched (never recreated)
#   (d) resync dirt, direct push rejected   -> falls back to a short-lived
#       (origin advanced, no foreign            branch + PR (via a stubbed
#       commits locally)                        `gh`), never a forced/bypass
#                                                push; origin/<default> itself
#                                                is left untouched; the
#                                                primary checkout resets back
#                                                to origin's tip afterward
#   (e) resync dirt + unrelated dirt        -> refuses to commit ANYTHING
#   (f) --dry-run                           -> preview only, no mutation
#   (g) invoked from a linked worktree      -> refuses (mirrors #4563)
#   (h) --allow-worktree                    -> permitted, warns
#   (i) untracked, unignored path matching a pure-copy-surface pattern but
#       with no defaults/ counterpart (the Loom source repo itself only)
#                                          -> excluded from the commit
#       (#6613/#7336 parity), left dirty in the tree; a legitimate resync
#       path alongside it still lands normally
#
# Usage:
#   ./.loom/scripts/tests/test-land-resync-commit.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/land-resync-commit.sh"

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

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-land-resync.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

LOOM_EMAIL="ci@loom.test"
LOOM_NAME="Loom CI"

# make_origin <name> -> creates $WORKDIR/<name>.git, a bare repo with its HEAD
# symref pointed at refs/heads/main (so a from-empty clone can check out and
# commit into "main" directly, rather than landing detached).
make_origin() {
    local name="$1"
    git init --bare -q "$WORKDIR/$name.git"
    git --git-dir="$WORKDIR/$name.git" symbolic-ref HEAD refs/heads/main
}

# make_primary <origin-name> <clone-name> -> clones <origin-name>.git into
# $WORKDIR/<clone-name>, configures the Loom automation identity, seeds one
# resync-managed file, commits, and pushes it as the initial "main".
make_primary() {
    local origin_name="$1" clone_name="$2"
    git clone -q "$WORKDIR/$origin_name.git" "$WORKDIR/$clone_name"
    git -C "$WORKDIR/$clone_name" config user.email "$LOOM_EMAIL"
    git -C "$WORKDIR/$clone_name" config user.name "$LOOM_NAME"
    mkdir -p "$WORKDIR/$clone_name/.loom/hooks"
    printf 'initial\n' > "$WORKDIR/$clone_name/.loom/hooks/foo.sh"
    git -C "$WORKDIR/$clone_name" checkout -q -b main
    git -C "$WORKDIR/$clone_name" add -A
    git -C "$WORKDIR/$clone_name" commit -q -m init
    git -C "$WORKDIR/$clone_name" push -q -u origin main
}

echo ""
echo "=== (a) clean tree -> no-op ==="
make_origin origin-a
make_primary origin-a primary-a
OUT="$(cd "$WORKDIR/primary-a" && "$SCRIPT" 2>&1)"; RC=$?
if [[ $RC -eq 0 ]] && grep -q "nothing to land" <<< "$OUT"; then
    pass "clean tree exits 0 with a 'nothing to land' message"
else
    fail "clean tree exits 0 with a 'nothing to land' message (rc=$RC, out=$OUT)"
fi

echo ""
echo "=== (b) resync-only dirt, no divergence -> committed + pushed directly ==="
make_origin origin-b
make_primary origin-b primary-b
printf 'updated\n' > "$WORKDIR/primary-b/.loom/hooks/foo.sh"
OUT="$(cd "$WORKDIR/primary-b" && "$SCRIPT" 2>&1)"; RC=$?
ORIGIN_LOG="$(git --git-dir="$WORKDIR/origin-b.git" log --oneline main)"
if [[ $RC -eq 0 ]] && grep -q "pushed" <<< "$OUT" && grep -q "resync installed Loom surfaces" <<< "$ORIGIN_LOG"; then
    pass "resync-only dirt with no divergence is committed and pushed directly"
else
    fail "resync-only dirt with no divergence is committed and pushed directly (rc=$RC, out=$OUT, origin_log=$ORIGIN_LOG)"
fi
if [[ -z "$(git -C "$WORKDIR/primary-b" status --porcelain)" ]]; then
    pass "primary checkout is clean after a direct push"
else
    fail "primary checkout is clean after a direct push"
fi

echo ""
echo "=== (c) resync dirt + a non-Loom commit already ahead -> commit, refuse to push/rebase ==="
make_origin origin-c
make_primary origin-c primary-c
git -C "$WORKDIR/primary-c" config user.email "operator@example.com"
git -C "$WORKDIR/primary-c" config user.name "An Operator"
printf 'operator change\n' > "$WORKDIR/primary-c/operator-file.txt"
git -C "$WORKDIR/primary-c" add -A
git -C "$WORKDIR/primary-c" commit -q -m "operator: local tooling commit"
OPERATOR_SHA="$(git -C "$WORKDIR/primary-c" rev-parse HEAD)"
git -C "$WORKDIR/primary-c" config user.email "$LOOM_EMAIL"
git -C "$WORKDIR/primary-c" config user.name "$LOOM_NAME"
printf 'updated\n' > "$WORKDIR/primary-c/.loom/hooks/foo.sh"
OUT="$(cd "$WORKDIR/primary-c" && "$SCRIPT" 2>&1)"; RC=$?
ORIGIN_LOG="$(git --git-dir="$WORKDIR/origin-c.git" log --oneline main)"
if [[ $RC -eq 3 ]] && grep -q "NOT pushed" <<< "$OUT" && grep -q "operator commit" <<< "$OUT"; then
    pass "operator commit ahead -> stops with exit 3 and names it"
else
    fail "operator commit ahead -> stops with exit 3 and names it (rc=$RC, out=$OUT)"
fi
if ! grep -q "resync installed Loom surfaces" <<< "$ORIGIN_LOG"; then
    pass "origin is untouched when stopping for an operator commit ahead"
else
    fail "origin is untouched when stopping for an operator commit ahead (origin_log=$ORIGIN_LOG)"
fi
if git -C "$WORKDIR/primary-c" cat-file -e "$OPERATOR_SHA" 2>/dev/null && \
   [[ "$(git -C "$WORKDIR/primary-c" log --format='%H' | sed -n '2p')" == "$OPERATOR_SHA" ]]; then
    pass "the operator's commit SHA is preserved verbatim (never rebased/recreated)"
else
    fail "the operator's commit SHA is preserved verbatim (never rebased/recreated)"
fi
if git -C "$WORKDIR/primary-c" log -1 --format='%s' | grep -q "resync installed Loom surfaces"; then
    pass "the resync commit itself was still made locally (just not pushed)"
else
    fail "the resync commit itself was still made locally (just not pushed)"
fi

echo ""
echo "=== (d) direct push rejected (origin advanced, no foreign local commits) -> branch + PR fallback ==="
make_origin origin-d
make_primary origin-d primary-d
# A third party lands an unrelated commit on origin that primary-d hasn't fetched.
git clone -q "$WORKDIR/origin-d.git" "$WORKDIR/other-d"
git -C "$WORKDIR/other-d" config user.email "someone@example.com"
git -C "$WORKDIR/other-d" config user.name "Someone Else"
printf 'other change\n' > "$WORKDIR/other-d/other-file.txt"
git -C "$WORKDIR/other-d" add -A
git -C "$WORKDIR/other-d" commit -q -m "unrelated merged PR"
git -C "$WORKDIR/other-d" push -q origin main

mkdir -p "$WORKDIR/stub-bin-d"
GH_CALLS_LOG="$WORKDIR/gh-calls-d.log"
cat > "$WORKDIR/stub-bin-d/gh" <<STUB
#!/usr/bin/env bash
echo "gh \$*" >> "$GH_CALLS_LOG"
case "\$1 \$2" in
    "pr list") echo "null" ;;
    "pr create") echo "https://example.invalid/pr/999" ;;
    *) echo "stub gh: unhandled args: \$*" >&2; exit 3 ;;
esac
STUB
chmod +x "$WORKDIR/stub-bin-d/gh"

printf 'updated\n' > "$WORKDIR/primary-d/.loom/hooks/foo.sh"
OUT="$(cd "$WORKDIR/primary-d" && PATH="$WORKDIR/stub-bin-d:$PATH" LOOM_FORGE_TYPE=github "$SCRIPT" 2>&1)"; RC=$?
ORIGIN_MAIN_LOG="$(git --git-dir="$WORKDIR/origin-d.git" log --oneline main)"
if [[ $RC -eq 0 ]] && grep -q "opened https://example.invalid/pr/999" <<< "$OUT"; then
    pass "rejected push falls back to a branch + PR and exits 0"
else
    fail "rejected push falls back to a branch + PR and exits 0 (rc=$RC, out=$OUT)"
fi
if ! grep -q "resync installed Loom surfaces" <<< "$ORIGIN_MAIN_LOG"; then
    pass "origin/<default> itself is never bypass-pushed — only the fallback branch carries the commit"
else
    fail "origin/<default> itself is never bypass-pushed (origin_main_log=$ORIGIN_MAIN_LOG)"
fi
FALLBACK_BRANCH="$(git --git-dir="$WORKDIR/origin-d.git" for-each-ref --format='%(refname:short)' 'refs/heads/chore/resync-installed-*')"
if [[ -n "$FALLBACK_BRANCH" ]]; then
    pass "a short-lived chore/resync-installed-* branch was pushed to origin"
else
    fail "a short-lived chore/resync-installed-* branch was pushed to origin"
fi
if grep -q "^gh pr create" "$GH_CALLS_LOG" 2>/dev/null; then
    pass "create-pr.sh's gh pr create was invoked for the fallback branch"
else
    fail "create-pr.sh's gh pr create was invoked for the fallback branch"
fi
if [[ -z "$(git -C "$WORKDIR/primary-d" status --porcelain)" ]] && \
   [[ "$(git -C "$WORKDIR/primary-d" rev-parse HEAD)" == "$(git -C "$WORKDIR/primary-d" rev-parse origin/main)" ]]; then
    pass "primary checkout resets back to origin's tip after the branch+PR fallback (never sits diverged)"
else
    fail "primary checkout resets back to origin's tip after the branch+PR fallback"
fi

echo ""
echo "=== (e) resync dirt + unrelated dirt -> refuses to commit ANYTHING ==="
make_origin origin-e
make_primary origin-e primary-e
printf 'updated\n' > "$WORKDIR/primary-e/.loom/hooks/foo.sh"
printf 'unrelated\n' > "$WORKDIR/primary-e/scratch.txt"
BEFORE_SHA="$(git -C "$WORKDIR/primary-e" rev-parse HEAD)"
OUT="$(cd "$WORKDIR/primary-e" && "$SCRIPT" 2>&1)"; RC=$?
AFTER_SHA="$(git -C "$WORKDIR/primary-e" rev-parse HEAD)"
if [[ $RC -ne 0 ]] && grep -q "non-resync dirt" <<< "$OUT" && [[ "$BEFORE_SHA" == "$AFTER_SHA" ]]; then
    pass "unrelated dirt alongside resync output refuses to commit anything"
else
    fail "unrelated dirt alongside resync output refuses to commit anything (rc=$RC, out=$OUT)"
fi
if [[ -n "$(git -C "$WORKDIR/primary-e" status --porcelain)" ]]; then
    pass "the tree is left exactly as dirty as before (both files still uncommitted)"
else
    fail "the tree is left exactly as dirty as before"
fi

echo ""
echo "=== (f) --dry-run previews only, no mutation ==="
make_origin origin-f
make_primary origin-f primary-f
printf 'updated\n' > "$WORKDIR/primary-f/.loom/hooks/foo.sh"
BEFORE_STATUS="$(git -C "$WORKDIR/primary-f" status --porcelain)"
BEFORE_SHA="$(git -C "$WORKDIR/primary-f" rev-parse HEAD)"
OUT="$(cd "$WORKDIR/primary-f" && "$SCRIPT" --dry-run 2>&1)"; RC=$?
AFTER_STATUS="$(git -C "$WORKDIR/primary-f" status --porcelain)"
AFTER_SHA="$(git -C "$WORKDIR/primary-f" rev-parse HEAD)"
if [[ $RC -eq 0 ]] && grep -q "\[dry-run\]" <<< "$OUT" && \
   [[ "$BEFORE_STATUS" == "$AFTER_STATUS" ]] && [[ "$BEFORE_SHA" == "$AFTER_SHA" ]]; then
    pass "--dry-run previews without committing or pushing"
else
    fail "--dry-run previews without committing or pushing (rc=$RC, out=$OUT)"
fi

echo ""
echo "=== (g) invoked from a linked worktree -> refuses (mirrors #4563) ==="
make_origin origin-g
make_primary origin-g primary-g
git -C "$WORKDIR/primary-g" worktree add -q -b feature/issue-1 "$WORKDIR/primary-g-wt" main
printf 'updated\n' > "$WORKDIR/primary-g/.loom/hooks/foo.sh"
OUT="$(cd "$WORKDIR/primary-g-wt" && "$SCRIPT" 2>&1)"; RC=$?
if [[ $RC -ne 0 ]] && grep -qi "linked git worktree" <<< "$OUT"; then
    pass "refuses to run from a linked worktree"
else
    fail "refuses to run from a linked worktree (rc=$RC, out=$OUT)"
fi
if [[ -n "$(git -C "$WORKDIR/primary-g" status --porcelain)" ]] && \
   [[ "$(git -C "$WORKDIR/primary-g" rev-parse HEAD)" != "" ]] && \
   ! git -C "$WORKDIR/primary-g" log -1 --format='%s' | grep -q "resync installed Loom surfaces"; then
    pass "the primary checkout's dirt is untouched by the refused worktree invocation"
else
    fail "the primary checkout's dirt is untouched by the refused worktree invocation"
fi

echo ""
echo "=== (h) --allow-worktree permits operating on the main checkout from a worktree ==="
OUT="$(cd "$WORKDIR/primary-g-wt" && "$SCRIPT" --allow-worktree 2>&1)"; RC=$?
ORIGIN_LOG="$(git --git-dir="$WORKDIR/origin-g.git" log --oneline main)"
if [[ $RC -eq 0 ]] && grep -qi "allow-worktree" <<< "$OUT" && grep -q "resync installed Loom surfaces" <<< "$ORIGIN_LOG"; then
    pass "--allow-worktree permits landing the commit from a linked worktree, warning loudly"
else
    fail "--allow-worktree permits landing the commit from a linked worktree, warning loudly (rc=$RC, out=$OUT)"
fi
git -C "$WORKDIR/primary-g" worktree remove --force "$WORKDIR/primary-g-wt" 2>/dev/null || true

echo ""
echo "=== (i) retired-but-unlisted pure-copy-surface path is excluded from the commit (#6613/#7336 parity) ==="
make_origin origin-i
make_primary origin-i primary-i
# Only IS_LOOM_SOURCE_REPO=1 (a local defaults/ tree) activates this check.
# Committed as scaffolding FIRST so it isn't itself flagged as unrelated dirt
# when the actual test scenario below runs.
mkdir -p "$WORKDIR/primary-i/defaults/hooks" "$WORKDIR/primary-i/defaults/scripts"
printf 'placeholder\n' > "$WORKDIR/primary-i/defaults/hooks/placeholder.sh"
git -C "$WORKDIR/primary-i" add -A
git -C "$WORKDIR/primary-i" commit -q -m "scaffold: defaults/ tree"
git -C "$WORKDIR/primary-i" push -q origin main
# A legitimate resync change: tracked-and-modified, always included.
printf 'updated\n' > "$WORKDIR/primary-i/.loom/hooks/foo.sh"
# An untracked file matching the pure-copy-surface pattern with NO defaults/
# counterpart -- presumed retired-but-unlisted, must be excluded.
mkdir -p "$WORKDIR/primary-i/.loom/scripts"
printf 'ORPHAN\n' > "$WORKDIR/primary-i/.loom/scripts/some-retired-tool.sh"
OUT="$(cd "$WORKDIR/primary-i" && "$SCRIPT" 2>&1)"; RC=$?
if [[ $RC -eq 0 ]] && grep -q "Excluded from the commit" <<< "$OUT" && grep -q "some-retired-tool.sh" <<< "$OUT"; then
    pass "retired-but-unlisted path is flagged and excluded, run still lands the legitimate change"
else
    fail "retired-but-unlisted path is flagged and excluded, run still lands the legitimate change (rc=$RC, out=$OUT)"
fi
ORIGIN_LOG_TREE="$(git --git-dir="$WORKDIR/origin-i.git" log -1 --format='%H' main | xargs -I{} git --git-dir="$WORKDIR/origin-i.git" ls-tree -r --name-only {})"
if ! grep -q "some-retired-tool.sh" <<< "$ORIGIN_LOG_TREE"; then
    pass "the retired-but-unlisted file was never actually committed"
else
    fail "the retired-but-unlisted file was never actually committed"
fi
if grep -q "\.loom/hooks/foo\.sh" <<< "$ORIGIN_LOG_TREE"; then
    pass "the legitimate resync-managed change was still committed and pushed"
else
    fail "the legitimate resync-managed change was still committed and pushed"
fi
if [[ -f "$WORKDIR/primary-i/.loom/scripts/some-retired-tool.sh" ]] && \
   [[ -n "$(git -C "$WORKDIR/primary-i" status --porcelain -- .loom/scripts/some-retired-tool.sh)" ]]; then
    pass "the excluded file is left untouched (still present, still untracked) for a human to reconcile"
else
    fail "the excluded file is left untouched (still present, still untracked) for a human to reconcile"
fi

echo ""
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED test(s) failed${NC}"
    exit 1
fi
echo -e "${GREEN}All tests passed${NC}"
exit 0
