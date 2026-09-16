#!/usr/bin/env bash
# test-worktree-forge-pr-check.sh — Tests for #7765.
#
# `worktree.sh N` used to resolve the issue branch ONLY against `origin`
# (`git fetch origin "$BRANCH_NAME" 2>/dev/null || true`, then a plain
# `show-ref` check). When that ref was absent it fell straight through to
# creating a FRESH branch at the base ref's HEAD under the same name and
# reported success — with zero forge awareness of whether an open PR already
# claims that branch name. This is blind to:
#   - a cross-repository (fork) PR, whose head branch never shows up as
#     origin/<branch> at all (rjwalters/loom#7765's headline case)
#   - a same-repo PR whose head simply wasn't fetched yet by the plain-name
#     fetch (a variant of #4823's in-flight-cycle case, reachable even when
#     that fetch fails/misses)
#   - a forge query that itself fails (gh missing/unauthenticated/rate-limited)
#     — which is NOT the same as "confirmed no PR exists"
#
# This suite verifies `_worktree_open_pr_for_branch` (the new helper) is
# consulted before falling through to a fresh branch, in exactly the case
# where NEITHER a local branch NOR `refs/remotes/origin/$BRANCH_NAME` exists:
#   1. Cross-repo open PR found -> worktree.sh REFUSES (exit != 0), never
#      silently reports success on a fresh main-HEAD branch of the same name.
#   2. Same-repo open PR found, but its ref wasn't fetched by the plain-name
#      fetch -> worktree.sh fetches it (via refs/pull/<n>/head, which the
#      forge publishes for every open PR) and REUSES it (#4823 extended).
#   3. Forge query genuinely unavailable (gh present but the query itself
#      fails, e.g. auth/rate-limit) -> worktree.sh REFUSES rather than
#      guessing "safe to create fresh".
#   4. Origin is not a recognized forge remote at all (offline/throwaway
#      clone — gh's own "no known GitHub host" signal) -> treated as "no PR
#      to shadow"; worktree.sh still creates the fresh branch as before.
#   5. No open PR matches this branch name at all (forge reachable, confirmed
#      clean) -> worktree.sh still creates the fresh branch as before.
#
# Companion to test-worktree-stale-merged-branch.sh (#5657) and
# test-worktree-remote-branch-tracking.sh (#4823) — both must keep passing
# unmodified (verified separately); this suite exercises the NEW code path
# that only runs when origin has no ref under $BRANCH_NAME at all.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKTREE_SH="$SCRIPTS_DIR/worktree.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

# Build a throwaway repo with an origin/main ref and NOTHING pushed under
# feature/issue-<issue> at all (neither locally nor on origin) — the "no ref
# anywhere" gap this issue targets. When with_pull_ref=true, also publishes
# refs/pull/<pr_number>/head on origin carrying a unique artifact, modeling
# GitHub's own auto-published PR merge ref (present for every open PR,
# same-repo or cross-repo) WITHOUT ever pushing the branch under its own name
# to origin — i.e. the exact ref the #4823-reuse fetch above cannot find, but
# the forge's PR-number-addressed ref can. Echoes the working-tree path.
setup_repo() {
    local name="$1"
    local with_pull_ref="${2:-false}"
    local pr_number="${3:-}"
    local tmp
    tmp=$(mktemp -d /tmp/loom-wtforge.XXXXXX)
    git init -q -b main "$tmp/origin.git" --bare
    git init -q -b main "$tmp/$name"
    (
        cd "$tmp/$name"
        git config user.email t@t
        git config user.name t
        git commit --allow-empty -q -m init
        git remote add origin "$tmp/origin.git"
        git push -q origin main
        mkdir -p .loom/scripts/lib .loom/hooks
        cp "$WORKTREE_SH" .loom/scripts/worktree.sh
        if [[ -d "$SCRIPTS_DIR/lib" ]]; then
            cp -R "$SCRIPTS_DIR"/lib/* .loom/scripts/lib/ 2>/dev/null || true
        fi
        chmod +x .loom/scripts/worktree.sh

        if [[ "$with_pull_ref" == "true" ]]; then
            git checkout -q -b pr-source-branch
            echo "same-repo-pr-artifact" > pr-artifact.txt
            git add pr-artifact.txt
            git commit -q -m "same-repo PR #$pr_number artifact"
            git push -q origin "HEAD:refs/pull/$pr_number/head"
            git checkout -q main
            git branch -q -D pr-source-branch
        fi
    )
    echo "$tmp/$name"
}

cleanup_repo() {
    local repo="$1"
    [[ -z "$repo" ]] && return 0
    rm -rf "$(dirname "$repo")"
}

# A minimal `gh` stand-in on PATH, modeling
# `pr list --state open --head <branch> --json number,isCrossRepository,headRepository,headRefName,url --limit 5`
# (also answers any stray `pr list ... --state merged` call with "[]", since
# that's the sibling #5657 check's shape - never expected to be reached in
# these scenarios, but harmless if it is). loom-daemon's `forge` passthrough
# shells out to `gh` on PATH too, so this intercepts both call shapes.
#   FAKE_GH_MODE=cross_repo  -> one open PR, head on a FORK
#   FAKE_GH_MODE=same_repo   -> one open PR, head in this same repo
#   FAKE_GH_MODE=none        -> no open PR matches (confirmed clean)
#   FAKE_GH_MODE=no_host     -> gh's own "no known GitHub host" failure
#                               (origin isn't a real forge remote)
#   FAKE_GH_MODE=unavailable -> a genuine forge failure (e.g. rate limit)
install_fake_gh() {
    local pr_number="${1:-999}"
    local fake_bin
    fake_bin="$(mktemp -d /tmp/loom-wtforge-bin.XXXXXX)"
    cat > "$fake_bin/gh" << EOF
#!/bin/bash
if [[ "\$1" == "pr" && "\$2" == "list" ]]; then
    shift 2
    state="" branch=""
    while [[ \$# -gt 0 ]]; do
        case "\$1" in
            --state) state="\$2"; shift 2 ;;
            --head)  branch="\$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    if [[ "\$state" == "merged" ]]; then
        echo "[]"
        exit 0
    fi
    case "\${FAKE_GH_MODE:-none}" in
        cross_repo)
            echo '[{"number": $pr_number, "isCrossRepository": true, "headRepository": {"nameWithOwner": "forkuser/loom"}, "headRefName": "'"\$branch"'", "url": "https://github.com/rjwalters/loom/pull/$pr_number"}]'
            ;;
        same_repo)
            echo '[{"number": $pr_number, "isCrossRepository": false, "headRepository": {"nameWithOwner": "rjwalters/loom"}, "headRefName": "'"\$branch"'", "url": "https://github.com/rjwalters/loom/pull/$pr_number"}]'
            ;;
        none)
            echo "[]"
            ;;
        no_host)
            echo "none of the git remotes configured for this repository point to a known GitHub host. To tell gh about a new GitHub host, please use \`gh auth login\`" >&2
            exit 1
            ;;
        unavailable)
            echo "gh: API rate limit exceeded for this token" >&2
            exit 1
            ;;
    esac
    exit 0
fi
echo "fake gh: unsupported invocation: \$*" >&2
exit 1
EOF
    chmod +x "$fake_bin/gh"
    echo "$fake_bin"
}

# --- Test 1: cross-repo open PR -> refuse, never silently fresh ---
echo "Test 1: open PR's head is on a FORK -> worktree.sh refuses, does not create a shadowing fresh branch"
REPO=$(setup_repo crossrepo1)
FAKE_BIN=$(install_fake_gh 1234)
OUT_LOG="/tmp/wtforge-cross.$$"
RC=0
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=cross_repo ./.loom/scripts/worktree.sh 77 >"$OUT_LOG" 2>&1
) || RC=$?
if [[ "$RC" -ne 0 ]]; then
    pass "worktree.sh exits non-zero when the branch name is already an open cross-repo PR's head"
else
    fail "worktree.sh exited 0 despite a cross-repo PR already claiming this branch name"
fi
if [[ ! -d "$REPO/.loom/worktrees/issue-77" ]]; then
    pass "no worktree was created (no silent fresh main-HEAD branch)"
else
    fail "a worktree was created despite the cross-repo PR conflict"
fi
if grep -qi "1234" "$OUT_LOG" && grep -qi "forkuser/loom" "$OUT_LOG"; then
    pass "output names the conflicting PR number and its fork repo"
else
    fail "output does not name the conflicting PR/fork (see $OUT_LOG)"
    cat "$OUT_LOG"
fi
cleanup_repo "$REPO"
rm -rf "$FAKE_BIN"
rm -f "$OUT_LOG"

# --- Test 2: same-repo open PR, ref not yet fetched -> fetch + reuse ---
echo ""
echo "Test 2: open PR's head is in THIS repo but unfetched -> worktree.sh fetches refs/pull/<n>/head and reuses it"
REPO=$(setup_repo samerepo1 true 999)
FAKE_BIN=$(install_fake_gh 999)
OUT_LOG="/tmp/wtforge-same.$$"
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=same_repo ./.loom/scripts/worktree.sh 77 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)
if [[ -f "$REPO/.loom/worktrees/issue-77/pr-artifact.txt" ]]; then
    pass "worktree contains the open PR's artifact (fetched refs/pull/999/head and reused it, not a fresh branch)"
else
    fail "worktree is missing the open PR's artifact — fell through to a fresh branch instead of reusing"
    cat "$OUT_LOG"
fi
cleanup_repo "$REPO"
rm -rf "$FAKE_BIN"
rm -f "$OUT_LOG"

# --- Test 3: forge query genuinely unavailable -> refuse ---
echo ""
echo "Test 3: forge query fails (not just 'no GitHub remote') -> worktree.sh refuses rather than guessing 'safe'"
REPO=$(setup_repo unavailable1)
FAKE_BIN=$(install_fake_gh)
OUT_LOG="/tmp/wtforge-unavail.$$"
RC=0
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=unavailable ./.loom/scripts/worktree.sh 77 >"$OUT_LOG" 2>&1
) || RC=$?
if [[ "$RC" -ne 0 ]]; then
    pass "worktree.sh exits non-zero when the forge query itself fails"
else
    fail "worktree.sh exited 0 despite being unable to verify via the forge"
fi
if [[ ! -d "$REPO/.loom/worktrees/issue-77" ]]; then
    pass "no worktree was created (did not proceed blind)"
else
    fail "a worktree was created despite the forge query failing"
fi
cleanup_repo "$REPO"
rm -rf "$FAKE_BIN"
rm -f "$OUT_LOG"

# --- Test 4: origin is not a recognized forge remote -> proceed as before ---
echo ""
echo "Test 4: origin has no forge relationship at all (offline/throwaway clone) -> still creates the fresh branch"
REPO=$(setup_repo nohost1)
FAKE_BIN=$(install_fake_gh)
OUT_LOG="/tmp/wtforge-nohost.$$"
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=no_host ./.loom/scripts/worktree.sh 77 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)
if [[ -d "$REPO/.loom/worktrees/issue-77" ]]; then
    pass "worktree was created from base (no forge relationship -> nothing to shadow)"
else
    fail "worktree.sh refused even though there is no forge to check against"
    cat "$OUT_LOG"
fi
cleanup_repo "$REPO"
rm -rf "$FAKE_BIN"
rm -f "$OUT_LOG"

# --- Test 5: forge confirms no open PR matches -> proceed as before ---
echo ""
echo "Test 5: forge reachable, confirms no open PR claims this branch -> still creates the fresh branch"
REPO=$(setup_repo none1)
FAKE_BIN=$(install_fake_gh)
OUT_LOG="/tmp/wtforge-none.$$"
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=none ./.loom/scripts/worktree.sh 77 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)
if [[ -d "$REPO/.loom/worktrees/issue-77" ]]; then
    pass "worktree was created from base (forge confirmed no conflicting PR)"
else
    fail "worktree.sh refused even though the forge confirmed no open PR matches"
    cat "$OUT_LOG"
fi
cleanup_repo "$REPO"
rm -rf "$FAKE_BIN"
rm -f "$OUT_LOG"

# --- Summary ---
echo ""
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
