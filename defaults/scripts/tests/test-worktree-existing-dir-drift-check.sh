#!/usr/bin/env bash
# test-worktree-existing-dir-drift-check.sh — Tests for the drift check on the
# "worktree directory already exists, registered with git" fast path (#6257)
#
# Regression coverage for the incident on #5609: a Judge session reused an
# existing builder worktree (`.loom/worktrees/issue-5609`) that was one commit
# behind the PR's actual pushed tip, had ~230 lines of uncommitted stale WIP
# sitting in the working tree, and whose local branch's upstream tracking ref
# was wrongly set to `origin/main` instead of `origin/feature/issue-5609`.
#
# Root cause: worktree.sh's "worktree directory already exists" fast path
# (`if git worktree list | grep -q "$WORKTREE_PATH"`) only ever compared the
# worktree's HEAD to BASE_REF (the default branch) to decide whether to
# "preserve existing work" or reset a stale worktree — it never fetched or
# compared against the branch's OWN upstream (origin/$BRANCH_NAME), and never
# touched upstream tracking at all. This is a completely different code path
# from the "local branch exists, no worktree dir yet" reuse path (#6095/#6100,
# covered by test-worktree-local-branch-upstream-tracking.sh) — that fix never
# ran here, so a worktree left with stale HEAD and/or wrong upstream tracking
# was silently "preserved" and handed straight to a Judge/Doctor session with
# no signal that it no longer matched the branch's actual pushed tip.
#
# Coverage:
#   1. Worktree one commit behind the pushed branch tip, wrong upstream
#      (origin/main), AND uncommitted changes (the exact incident shape):
#      worktree.sh warns about the drift, corrects the upstream, and does NOT
#      destroy the uncommitted work (still preserved for the caller).
#   2. Worktree with a local commit ahead of the pushed tip (unpushed work)
#      and no uncommitted changes: no false-positive "may be stale" warning.
#   3. Worktree already correctly synced (HEAD matches origin's tip, upstream
#      already correct, no uncommitted changes): no-op, no warning of any
#      kind (regression guard against false positives on the common case).
#   4. (#8287) Local branch reset back to the base (0 commits ahead of
#      main, no uncommitted changes — "stale" by the OLD main-only
#      criterion) while a LIVE origin/feature/issue-N still carries the
#      branch's real commit: worktree.sh resets/checks out at the remote
#      tip, never at main — the Doctor-on-#8190 incident this closes.
#   5. (#8287) Same stale-local shape, but origin's tip IS already the head
#      of a merged PR (#5657): worktree.sh still falls back to resetting at
#      main — the merged-tip skip keeps working on this code path too.
#
# Pattern follows test-worktree-local-branch-upstream-tracking.sh: throwaway
# bare origin + repo in a mktemp dir, copy worktree.sh + lib/, but here the
# worktree itself is first materialized via a REAL `./.loom/scripts/worktree.sh
# <N>` call (so the fast path under test — "directory already exists,
# registered with git" — actually fires on the second invocation, exactly as
# it would for a reused builder worktree), then mutated to the drift shape
# under test before invoking worktree.sh a second time.

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

# Build a throwaway repo with a `feature/issue-<n>` branch pushed to origin,
# then materialize its worktree via a real (first) `worktree.sh <n>` call —
# so the worktree is registered with git exactly as an earlier
# worktree.sh/Builder pass would have left it, with correct tracking. Echoes
# "<repo-path> <worktree-relative-path>".
#
# Resolves the mktemp root to its physical path (pwd -P): on macOS /tmp is a
# symlink to /private/tmp, and worktree.sh's orphan-cleanup compares `git
# worktree list` paths (physical) against a resolved path — a symlinked temp
# root would make the just-registered worktree look unregistered and get
# spuriously deleted, defeating the point of this reuse-path test (mirrors
# test-worktree-sentinel-reinvoke.sh's TMP_ROOT handling).
setup_repo_with_worktree() {
    local name="$1"
    local issue="$2"
    local tmp
    tmp=$(cd "$(mktemp -d /tmp/loom-wtdrift.XXXXXX)" && pwd -P)
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

        git checkout -q -b "feature/issue-$issue"
        echo "builder-work" > work.txt
        git add work.txt
        git commit -q -m "builder work"
        git push -q -u origin "feature/issue-$issue"
        git checkout -q main

        # First (real) worktree.sh invocation - materializes and registers
        # the worktree exactly as a Builder pass would.
        ./.loom/scripts/worktree.sh "$issue" >/dev/null 2>&1
    )
    echo "$tmp/$name .loom/worktrees/issue-$issue"
}

# Push one more commit to origin/feature/issue-<n> WITHOUT touching the
# existing worktree (which already has that branch checked out) — via a
# throwaway second clone, simulating a later push from a different
# session/worktree.
push_followup_commit() {
    local repo="$1"
    local issue="$2"
    local origin
    origin="$(git -C "$repo" remote get-url origin)"
    local clone_dir
    clone_dir=$(mktemp -d /tmp/loom-wtdrift-clone.XXXXXX)
    git clone -q "$origin" "$clone_dir" >/dev/null 2>&1
    (
        cd "$clone_dir"
        git config user.email t@t
        git config user.name t
        git checkout -q "feature/issue-$issue"
        echo "later-push" > later.txt
        git add later.txt
        git commit -q -m "later push from another session"
        git push -q origin "feature/issue-$issue"
    )
    rm -rf "$clone_dir"
}

cleanup_repo() {
    local repo="$1"
    [[ -z "$repo" ]] && return 0
    rm -rf "$(dirname "$repo")"
}

# Reset the worktree's LOCAL branch tip back to origin/main in place, WITHOUT
# touching origin/feature/issue-<n> — simulating a worktree whose local branch
# never advanced (or lost its commit) while the branch's real content still
# lives on the remote. This is "0 commits ahead of main, no uncommitted
# changes" by the pre-#8287 staleness criterion, with the PR's actual content
# reachable only via origin/feature/issue-<n>. Also removes the `.loom-managed`
# sentinel the first `worktree.sh` invocation left behind: it is untracked and
# this throwaway fixture repo has no .gitignore for it, so leaving it in place
# would make `git status --porcelain` permanently non-empty and force every
# re-invocation down the "preserve existing work" branch instead of the
# staleness check under test (worktree.sh re-creates it on every exit path).
rewind_worktree_to_main() {
    local wt="$1"
    git -C "$wt" reset -q --hard origin/main
    rm -f "$wt/.loom-managed"
}

# A minimal `gh` stand-in on PATH, modeling
# `pr list --head <branch> --state merged --json headRefOid,number --limit 1`
# — same fixture shape as test-worktree-stale-merged-branch.sh's
# install_fake_gh. FAKE_GH_MODE=merged reports a merged PR whose headRefOid is
# the CURRENT tip of origin/<branch> (the #5657 skip case); FAKE_GH_MODE=off
# makes every call fail (forge unavailable).
install_fake_gh() {
    local repo="$1"
    local fake_bin
    fake_bin="$(dirname "$repo")/fakebin"
    mkdir -p "$fake_bin"
    cat > "$fake_bin/gh" << EOF
#!/bin/bash
if [[ "\${FAKE_GH_MODE:-none}" == "off" ]]; then
    echo "gh: fake forge unavailable in this test" >&2
    exit 1
fi
if [[ "\$1" == "pr" && "\$2" == "list" ]]; then
    shift 2
    branch=""
    while [[ \$# -gt 0 ]]; do
        case "\$1" in
            --head) branch="\$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    if [[ "\${FAKE_GH_MODE:-none}" == "merged" && -n "\$branch" ]]; then
        sha="\$(git -C "$repo" rev-parse --verify -q "refs/remotes/origin/\$branch" 2>/dev/null || echo "")"
        if [[ -n "\$sha" ]]; then
            echo "[{\"headRefOid\": \"\$sha\", \"number\": 999}]"
            exit 0
        fi
    fi
    echo "[]"
    exit 0
fi
echo "fake gh: unsupported invocation: \$*" >&2
exit 1
EOF
    chmod +x "$fake_bin/gh"
    echo "$fake_bin"
}

# --- Test 1: behind pushed tip + wrong upstream + uncommitted changes (the incident) ---
echo "Test 1: worktree one commit behind pushed tip, wrong upstream, uncommitted WIP -> worktree.sh warns and corrects upstream without destroying WIP"
read -r REPO WT_REL <<< "$(setup_repo_with_worktree incident 301)"
WT="$REPO/$WT_REL"

# Simulate: another session pushed a follow-up commit this worktree never saw.
push_followup_commit "$REPO" 301

# Simulate: upstream tracking somehow got mis-set to origin/main (the #6095
# incident shape) after the worktree was created correctly.
git -C "$WT" branch --set-upstream-to=origin/main feature/issue-301

# Simulate: stale uncommitted WIP sitting in the working tree.
echo "stale-wip-line" >> "$WT/work.txt"

OUT_LOG="/tmp/wtdrift-incident.$$"
(
    cd "$REPO"
    ./.loom/scripts/worktree.sh 301 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)

if grep -qi "may be stale" "$OUT_LOG"; then
    pass "worktree.sh warns that the worktree may be stale"
else
    fail "worktree.sh did not warn about staleness"
    cat "$OUT_LOG"
fi

if grep -qi "uncommitted changes" "$OUT_LOG"; then
    pass "worktree.sh flags the uncommitted changes alongside the drift warning"
else
    fail "worktree.sh did not mention the uncommitted changes in its drift warning"
fi

WT_UPSTREAM=$(git -C "$WT" rev-parse --abbrev-ref 'feature/issue-301@{u}' 2>/dev/null || echo "")
if [[ "$WT_UPSTREAM" == "origin/feature/issue-301" ]]; then
    pass "worktree.sh corrected the upstream to origin/feature/issue-301 (was origin/main)"
else
    fail "worktree's upstream is '$WT_UPSTREAM', expected 'origin/feature/issue-301'"
fi

if grep -q "stale-wip-line" "$WT/work.txt"; then
    pass "uncommitted WIP was NOT destroyed by the drift check"
else
    fail "uncommitted WIP was lost"
fi

WT_HEAD=$(git -C "$WT" rev-parse HEAD)
ORIGIN_TIP=$(git -C "$REPO" rev-parse origin/feature/issue-301)
if [[ "$WT_HEAD" != "$ORIGIN_TIP" ]]; then
    pass "worktree.sh did not silently pull/reset HEAD on its own (still behind, as expected for a warn-only check)"
else
    fail "worktree.sh unexpectedly moved HEAD to the remote tip"
fi
cleanup_repo "$REPO"
rm -f "$OUT_LOG"

# --- Test 2: local commit ahead of pushed tip, no uncommitted changes -> no false positive ---
echo ""
echo "Test 2: worktree has an unpushed local commit ahead of origin's tip, no uncommitted changes -> no 'may be stale' false positive"
read -r REPO WT_REL <<< "$(setup_repo_with_worktree ahead 302)"
WT="$REPO/$WT_REL"

# Add a local commit in the worktree that has NOT been pushed.
(
    cd "$WT"
    echo "unpushed-local-commit" > unpushed.txt
    git add unpushed.txt
    git commit -q -m "unpushed local work"
)

OUT_LOG="/tmp/wtdrift-ahead.$$"
(
    cd "$REPO"
    ./.loom/scripts/worktree.sh 302 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)

if grep -qi "may be stale" "$OUT_LOG"; then
    fail "worktree.sh false-positive warned about staleness for a worktree that is genuinely AHEAD, not behind"
    cat "$OUT_LOG"
else
    pass "worktree.sh did not false-positive warn for a worktree ahead of origin (unpushed local commit)"
fi
if grep -qi "preserving existing work" "$OUT_LOG"; then
    pass "worktree.sh still reports preserving the existing (ahead) work"
else
    fail "worktree.sh did not report preserving the ahead work"
fi
cleanup_repo "$REPO"
rm -f "$OUT_LOG"

# --- Test 3: already correctly synced -> pure no-op, no warnings at all ---
echo ""
echo "Test 3: worktree already matches origin's tip, upstream already correct, no uncommitted changes -> no-op"
read -r REPO WT_REL <<< "$(setup_repo_with_worktree synced 303)"
WT="$REPO/$WT_REL"

# Nothing mutated - this worktree is exactly as worktree.sh's first
# invocation left it: HEAD == origin/feature/issue-303, upstream already
# correct, clean tree.

OUT_LOG="/tmp/wtdrift-synced.$$"
(
    cd "$REPO"
    ./.loom/scripts/worktree.sh 303 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)

if grep -qi "may be stale\|correcting to\|has no upstream" "$OUT_LOG"; then
    fail "worktree.sh printed a drift/correction warning for an already-synced worktree"
    cat "$OUT_LOG"
else
    pass "worktree.sh made no drift/correction noise for the already-synced case"
fi
WT_UPSTREAM=$(git -C "$WT" rev-parse --abbrev-ref 'feature/issue-303@{u}' 2>/dev/null || echo "")
if [[ "$WT_UPSTREAM" == "origin/feature/issue-303" ]]; then
    pass "worktree's upstream remains origin/feature/issue-303 (unaffected)"
else
    fail "worktree's upstream is '$WT_UPSTREAM', expected unchanged 'origin/feature/issue-303'"
fi
cleanup_repo "$REPO"
rm -f "$OUT_LOG"

# --- Test 4 (#8287): stale local branch + LIVE remote branch -> resets at the remote tip, never main ---
echo ""
echo "Test 4 (#8287): local branch rewound to main (0 ahead, no uncommitted changes) while origin/feature/issue-N is a live, unmerged branch -> worktree.sh resets/checks out at the remote tip, not main"
read -r REPO WT_REL <<< "$(setup_repo_with_worktree stale8287 401)"
WT="$REPO/$WT_REL"
FAKE_BIN=$(install_fake_gh "$REPO")
ORIGIN_TIP=$(git -C "$REPO" rev-parse origin/feature/issue-401)

rewind_worktree_to_main "$WT"

OUT_LOG="/tmp/wtdrift-stale8287.$$"
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=none ./.loom/scripts/worktree.sh 401 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)

if [[ -f "$WT/work.txt" ]]; then
    pass "worktree recovered the branch's real content from origin (not reset to bare main)"
else
    fail "worktree lost the branch's content — it was reset to main instead of the live remote tip"
    cat "$OUT_LOG"
fi
WT_HEAD=$(git -C "$WT" rev-parse HEAD 2>/dev/null || echo "")
if [[ "$WT_HEAD" == "$ORIGIN_TIP" ]]; then
    pass "worktree HEAD equals origin/feature/issue-401's tip (reset target was the remote branch)"
else
    fail "worktree HEAD ($WT_HEAD) does not equal origin/feature/issue-401's tip ($ORIGIN_TIP)"
fi
if grep -q "origin/feature/issue-401" "$OUT_LOG"; then
    pass "output names origin/feature/issue-401 as the staleness reference / reset target"
else
    fail "output never mentions origin/feature/issue-401 as the reference used (see $OUT_LOG)"
    cat "$OUT_LOG"
fi
cleanup_repo "$REPO"
rm -f "$OUT_LOG"

# --- Test 5 (#8287): stale local branch + origin tip already MERGED -> the #5657 skip still applies here ---
echo ""
echo "Test 5 (#8287): same stale-local shape, but origin/feature/issue-N is already the head of a merged PR -> worktree.sh still falls back to resetting at main (#5657 skip unaffected)"
read -r REPO WT_REL <<< "$(setup_repo_with_worktree stale8287merged 402)"
WT="$REPO/$WT_REL"
FAKE_BIN=$(install_fake_gh "$REPO")
MAIN_TIP=$(git -C "$REPO" rev-parse origin/main)

rewind_worktree_to_main "$WT"

OUT_LOG="/tmp/wtdrift-stale8287merged.$$"
(
    cd "$REPO"
    PATH="$FAKE_BIN:$PATH" FAKE_GH_MODE=merged ./.loom/scripts/worktree.sh 402 >"$OUT_LOG" 2>&1 || { echo "FAILED"; cat "$OUT_LOG"; }
)

if [[ ! -f "$WT/work.txt" ]]; then
    pass "worktree was NOT reset onto the already-merged branch's dead content"
else
    fail "worktree picked up the already-merged branch's content — the #5657 skip regressed on this code path"
    cat "$OUT_LOG"
fi
WT_HEAD=$(git -C "$WT" rev-parse HEAD 2>/dev/null || echo "")
if [[ "$WT_HEAD" == "$MAIN_TIP" ]]; then
    pass "worktree HEAD equals origin/main's tip (fell back to the base ref, as #5657 requires)"
else
    fail "worktree HEAD ($WT_HEAD) does not equal origin/main's tip ($MAIN_TIP)"
fi
cleanup_repo "$REPO"
rm -f "$OUT_LOG"

# --- Summary ---
echo ""
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
