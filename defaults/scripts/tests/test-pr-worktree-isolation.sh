#!/usr/bin/env bash
# test-pr-worktree-isolation.sh - Regression test for issue #3358
#
# Verifies that:
#   1. pr-worktree.sh creates a worktree at .loom/worktrees/pr-<N>/ with the
#      .loom-managed sentinel (PR-mode entry).
#   2. doctor.md (defaults/) documents the branch-isolation requirement,
#      including the #9401 occupied-worktree yield (live process, no lease).
#   3. pr-worktree.sh surfaces a clear diagnostic when the PR branch is already
#      checked out elsewhere, and cleans up the partial pr-<N> worktree (#6264,
#      #6594).
#   4. pr-worktree.sh catches that collision up front — before any pr-<N>
#      worktree is created at all — when `gh pr view` resolves the head branch
#      (#6594).
#   5. pr-worktree.sh on a non-colliding PR runs `gh pr checkout` inside the new
#      pr-<N> worktree, never in the orchestrator checkout (#3358): a mock `gh`
#      does a real `git checkout`, so a mis-directed `cd` visibly moves the
#      orchestrator's branch/HEAD. The helper (not the test) writes the sentinel.
#   6. judge.md and `pr-worktree.sh --help` agree on the Judge fallback (#11194).
#   7. When the PR branch is checked out in the PRIMARY clone, pr-worktree.sh
#      creates pr-<N> detached at the PR head sha and exits 0 (#14391), both via
#      the up-front check and via the checkout-failure path.
#
# Tests 3-5 run the real pr-worktree.sh against a throwaway git repo with a
# mocked `gh`, since there is no live forge in CI. Branch-shape classification
# is deliberately NOT asserted here: it is no longer shell logic. It lives in
# `issue_number_of` (loom-daemon/src/merge_pr/cleanup_paths.rs, ported out of
# merge-pr.sh in #8191) and the #3358 strict-regex regression cases are covered
# by that module's own unit tests (loom-daemon/src/merge_pr/cleanup_paths/tests.rs).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/../.." && pwd)"

PR_WORKTREE_SH="$SCRIPTS_DIR/pr-worktree.sh"
# doctor.md is shipped (installed at .claude/commands/loom/doctor.md), so
# resolve it the way each layout actually lays it out: the installed path
# first (consumer repos, and Loom's own dogfooded checkout), falling back
# to the defaults/ source-tree path (a bare source checkout with no
# .claude/commands/loom/ copy yet). See issue #6194 / #6241.
if [[ -f "$REPO_ROOT/.claude/commands/loom/doctor.md" ]]; then
    DOCTOR_MD="$REPO_ROOT/.claude/commands/loom/doctor.md"
else
    DOCTOR_MD="$REPO_ROOT/defaults/.claude/commands/loom/doctor.md"
fi

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

if [[ ! -f "$DOCTOR_MD" ]]; then
    echo -e "${RED}FATAL${NC}: doctor.md not found at $DOCTOR_MD"
    exit 1
fi

# --- Test 1: pr-worktree.sh exists and has the sentinel write block ---
echo "Test 1: pr-worktree.sh ships sentinel write logic"
if [[ -x "$PR_WORKTREE_SH" ]]; then
    pass "pr-worktree.sh exists and is executable"
else
    fail "pr-worktree.sh missing or not executable at $PR_WORKTREE_SH"
fi
assert_grep "Loom-managed worktree marker" "$PR_WORKTREE_SH" \
    "pr-worktree.sh writes the sentinel marker"
assert_grep "# PR: " "$PR_WORKTREE_SH" \
    "pr-worktree.sh records PR number in sentinel (mirrors worktree.sh issue convention)"
assert_grep "worktrees/pr-" "$PR_WORKTREE_SH" \
    "pr-worktree.sh uses .loom/worktrees/pr-<N>/ convention"

# --- Test 2: doctor.md documents PR Branch Isolation ---
echo ""
echo "Test 2: doctor.md documents the worktree-isolation rule"
assert_grep "PR Branch Isolation" "$DOCTOR_MD" \
    "doctor.md has a 'PR Branch Isolation' section"
assert_grep "pr-worktree\.sh" "$DOCTOR_MD" \
    "doctor.md references pr-worktree.sh helper"
assert_grep "\^feature/issue-" "$DOCTOR_MD" \
    "doctor.md documents the feature/issue-<N> regex heuristic"

# #9401: a live process in the PR worktree with no PR lease = yield, not reset.
assert_grep "^### Occupied worktree: yield, never reset \(#9401\)" "$DOCTOR_MD" \
    "doctor.md has the 'Occupied worktree' yield section (#9401)"
assert_grep "After \`lease pr claim\` succeeds and \*\*before\*\* \`worktree\.sh\`" "$DOCTOR_MD" \
    "occupied-worktree check runs after the lease claim, before worktree setup"
assert_grep "readlink \"/proc/\\\$p/cwd\".*\|\| lsof -a -p" "$DOCTOR_MD" \
    "occupied-worktree probe reads /proc cwd with an lsof fallback"
assert_grep "\[ \"\\\$q\" = \"\\\$S\" \] \|\|" "$DOCTOR_MD" \
    "occupied-worktree probe excludes the Doctor's own session"
assert_grep "^post \`Doctor yielding: live process in <WT> \(pid <pid>: <command>\) with no PR$" "$DOCTOR_MD" \
    "yield comment names the worktree, pid and command"
assert_grep "lease; worktree left untouched\.\` and stop — no snapshot, reset or checkout\." "$DOCTOR_MD" \
    "yield stops without touching the worktree"
assert_grep "\*\*Occupied → yield\*\*: \`lease pr release\`, drop any \`loom:treating\` you added" "$DOCTOR_MD" \
    "yield releases the lease and the doctor's own loom:treating"
assert_grep "\*\*Cannot probe\*\* \(no \`pgrep\`, or neither \`/proc\` nor \`lsof\`\): treat a dirty" "$DOCTOR_MD" \
    "documents the no-pgrep/no-lsof fallback (dirty = occupied)"
occ_line=$(grep -n "^### Occupied worktree" "$DOCTOR_MD" | head -1 | cut -d: -f1)
pick_line=$(grep -n "^# PR worktree selection" "$DOCTOR_MD" | head -1 | cut -d: -f1)
if [[ -n "$occ_line" && -n "$pick_line" && "$occ_line" -lt "$pick_line" ]]; then
    pass "occupied-worktree check precedes worktree selection/checkout"
else
    fail "occupied-worktree check precedes worktree selection/checkout (occ=$occ_line pick=$pick_line)"
fi

# Execute the documented probe with only a simulated process in a
# disposable directory. The real host's process list is irrelevant.
probe_dir=$(mktemp -d)
awk -v h="### Occupied worktree" 'index($0,h)==1{f=1} f&&/^```bash$/{g=1;next} g&&/^```$/{exit} g' "$DOCTOR_MD" > "$probe_dir/probe.txt"
if [[ -s "$probe_dir/probe.txt" ]]; then
    mkdir -p "$probe_dir/bin" "$probe_dir/.loom/worktrees/pr-1"
    cat > "$probe_dir/bin/pgrep" <<'EOF'
#!/bin/bash
echo 4199
EOF
    cat > "$probe_dir/bin/readlink" <<'EOF'
#!/bin/bash
[[ "$1" = /proc/4199/cwd ]] || exit 1
(cd .loom/worktrees/pr-1 && pwd -P)
EOF
    cat > "$probe_dir/bin/ps" <<'EOF'
#!/bin/bash
case "$*" in
    '-o ppid= -p 4199') echo 1 ;;
    '-o pid=,command= -p 4199') echo '4199 fixture-process' ;;
    *) exit 1 ;;
esac
EOF
    chmod +x "$probe_dir/bin/"*
    run_probe() {
        (cd "$probe_dir" && PATH="$probe_dir/bin:$PATH" \
            WORKTREE_PATH=".loom/worktrees/$1" LOOM_AGENT_SESSION_PID=4400 bash probe.txt)
    }
    assert_eq "" "$(run_probe nonexistent-1)" "occupied-worktree probe prints nothing for a nonexistent worktree"
    assert_eq "4199 fixture-process" "$(run_probe pr-1)" "occupied-worktree probe reports a foreign process in the selected worktree"
else
    fail "could not extract the occupied-worktree probe snippet from doctor.md"
fi
rm -rf "$probe_dir"

# --- Test 3: detached-HEAD collision is diagnosed clearly, not silent (#6264) ---
echo ""
echo "Test 3: pr-worktree.sh surfaces a clear diagnostic on the branch-collision failure (#6264), and cleans up after itself (#6594)"

# Throwaway repo fixture shared by Tests 3, 4 and 5: a tiny origin + clone so the
# real pr-worktree.sh runs real git commands without touching the real
# workspace.
TMP=$(mktemp -d /tmp/loom-pr-iso-test.XXXXXX)
trap 'rm -rf "$TMP"; cd "$REPO_ROOT" 2>/dev/null || true' EXIT

git init -q -b main "$TMP/origin.git" --bare
git init -q -b main "$TMP/repo"
cd "$TMP/repo"
git config user.email t@t
git config user.name t
git commit --allow-empty -q -m init
git remote add origin "$TMP/origin.git"
git push -q origin main

# Reproduces (with a mocked `gh`, since this test has no live forge) the exact
# mechanism confirmed by hand against a real PR while curating/building #6264:
# `gh pr checkout <N> --force` fails loudly (git refuses the same branch
# checked out in two worktrees at once). The fix under test is NOT changing
# that git-level refusal (impossible — it's a structural git invariant) but
# making pr-worktree.sh's failure message name the colliding worktree
# explicitly (#6264) AND, since this mock's `gh pr view` returns nothing (see
# below), exercising the #6594 fallback cleanup path: when the up-front
# collision check can't resolve the PR's head branch, the collision is only
# discovered via the `gh pr checkout` failure, and the partially-created
# pr-<N> worktree must be removed rather than left behind.
#
# Runs inside the throwaway "$TMP/repo" built above, against its origin remote.
COLLIDE_BRANCH="feature/issue-998"
git update-ref "refs/heads/$COLLIDE_BRANCH" HEAD
# Simulate an existing builder worktree already holding the branch — the
# real-world collision source (a same-host issue-<N> builder worktree).
git worktree add -q ".loom/worktrees/issue-998" "$COLLIDE_BRANCH"

MOCKBIN=$(mktemp -d /tmp/loom-pr-iso-mockbin.XXXXXX)
trap 'rm -rf "$TMP" "$MOCKBIN"; cd "$REPO_ROOT" 2>/dev/null || true' EXIT
cat > "$MOCKBIN/gh" <<'MOCKEOF'
#!/usr/bin/env bash
# Minimal `gh pr checkout --force` stand-in reproducing the exact failure
# `git worktree add`/`checkout` itself produces when the target branch is
# already checked out elsewhere — see the real repro in the #6264 PR
# description. `gh pr view` (used by pr-worktree.sh's #6594 up-front
# collision check) is deliberately left unhandled here (falls through to the
# generic `exit 0` with no stdout) to exercise the fallback path where that
# check can't resolve the head branch and the collision is only caught later,
# via the checkout failure below. Any other subcommand is a no-op success.
if [[ "$1" == "pr" && "$2" == "checkout" ]]; then
    echo "fatal: 'feature/issue-998' is already used by worktree at '$LOOM_TEST_COLLIDING_WT'" >&2
    echo "failed to run git: exit status 128" >&2
    exit 1
fi
exit 0
MOCKEOF
chmod +x "$MOCKBIN/gh"

COLLIDING_WT_ABS="$(cd ".loom/worktrees/issue-998" && pwd)"

set +e
PR_OUT=$(PATH="$MOCKBIN:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_COLLIDING_WT="$COLLIDING_WT_ABS" \
    "$PR_WORKTREE_SH" 998 2>&1)
PR_RC=$?
set -e
if [[ "$PR_RC" -ne 0 ]]; then
    pass "pr-worktree.sh exits non-zero on the branch-collision failure"
else
    fail "pr-worktree.sh exited 0 despite the checkout failure — got: $PR_OUT"
fi
if grep -q "already checked out in another worktree" <<<"$PR_OUT"; then
    pass "failure message explicitly names the collision (not just the raw git error)"
else
    fail "expected an explicit collision diagnostic; got: $PR_OUT"
fi
if grep -qF "$COLLIDING_WT_ABS" <<<"$PR_OUT"; then
    pass "failure message names the specific colliding worktree path"
else
    fail "expected the colliding worktree path ($COLLIDING_WT_ABS) in the output; got: $PR_OUT"
fi
if grep -qi "do NOT evaluate code in it as-is" <<<"$PR_OUT"; then
    pass "failure message warns against evaluating the detached worktree as-is"
else
    fail "expected an explicit do-not-evaluate warning; got: $PR_OUT"
fi

# #6594: the pr-998 worktree directory is now removed (via `git worktree
# remove`, not left behind on disk on a detached HEAD) before pr-worktree.sh
# exits — this is the behavior change under test in this issue. It replaces
# the pre-#6594 "left behind" assertions that used to live here.
if [[ -d ".loom/worktrees/pr-998" ]]; then
    fail "expected .loom/worktrees/pr-998 to be removed after the failed checkout (#6594 cleanup)"
else
    pass "the pr-998 worktree directory is removed after the failed checkout (#6594 cleanup)"
fi
if git worktree list --porcelain | grep -q "worktrees/pr-998$"; then
    fail "expected no dangling 'git worktree list' entry for pr-998 after cleanup"
else
    pass "no dangling 'git worktree list' entry for pr-998 after cleanup"
fi

# --- Test 4: branch collision is caught up front, before any worktree is
#     created at all, when 'gh pr view' can resolve the PR's head branch (#6594) ---
echo ""
echo "Test 4: pr-worktree.sh catches the collision up front and never creates a pr-<N> worktree (#6594)"

# Still inside "$TMP/repo". Use PR 997 / feature/issue-997 to avoid colliding
# with Test 3's 998 fixture above.
COLLIDE_BRANCH2="feature/issue-997"
git update-ref "refs/heads/$COLLIDE_BRANCH2" HEAD
git worktree add -q ".loom/worktrees/issue-997" "$COLLIDE_BRANCH2"
COLLIDING_WT2_ABS="$(cd ".loom/worktrees/issue-997" && pwd)"

MOCKBIN2=$(mktemp -d /tmp/loom-pr-iso-mockbin2.XXXXXX)
trap 'rm -rf "$TMP" "$MOCKBIN" "$MOCKBIN2"; cd "$REPO_ROOT" 2>/dev/null || true' EXIT
cat > "$MOCKBIN2/gh" <<'MOCKEOF'
#!/usr/bin/env bash
# Stand-in for `gh` supporting exactly the two subcommands pr-worktree.sh's
# up-front collision check (#6594) needs: `gh pr view --json headRefName`
# resolves the PR's head branch to the (already checked-out-elsewhere)
# collision branch. `gh pr checkout` writes a marker file if it is ever
# invoked, so the test can assert the up-front check short-circuited before
# reaching it — reaching checkout here would mean the up-front check failed
# to catch a collision it should have caught.
if [[ "$1" == "pr" && "$2" == "view" ]]; then
    echo "$LOOM_TEST_HEAD_BRANCH"
    exit 0
fi
if [[ "$1" == "pr" && "$2" == "checkout" ]]; then
    touch "$LOOM_TEST_CHECKOUT_MARKER"
    exit 0
fi
exit 0
MOCKEOF
chmod +x "$MOCKBIN2/gh"

CHECKOUT_MARKER="$TMP/checkout-was-called"
set +e
PR_OUT2=$(PATH="$MOCKBIN2:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$COLLIDE_BRANCH2" \
    LOOM_TEST_CHECKOUT_MARKER="$CHECKOUT_MARKER" "$PR_WORKTREE_SH" 997 2>&1)
PR_RC2=$?
set -e

if [[ "$PR_RC2" -ne 0 ]]; then
    pass "pr-worktree.sh exits non-zero on the up-front collision check"
else
    fail "pr-worktree.sh exited 0 despite the up-front collision — got: $PR_OUT2"
fi
if grep -q "already checked out in another worktree" <<<"$PR_OUT2"; then
    pass "up-front failure message explicitly names the collision"
else
    fail "expected an explicit up-front collision diagnostic; got: $PR_OUT2"
fi
if grep -qF "$COLLIDING_WT2_ABS" <<<"$PR_OUT2"; then
    pass "up-front failure message names the specific colliding worktree path"
else
    fail "expected the colliding worktree path ($COLLIDING_WT2_ABS) in the output; got: $PR_OUT2"
fi
if [[ -d ".loom/worktrees/pr-997" ]]; then
    fail "expected no .loom/worktrees/pr-997 directory to ever be created (up-front detection should prevent it)"
else
    pass "no .loom/worktrees/pr-997 directory created — collision caught before worktree creation"
fi
if [[ -f "$CHECKOUT_MARKER" ]]; then
    fail "expected 'gh pr checkout' to never be called — the up-front check should short-circuit first"
else
    pass "'gh pr checkout' was never called — up-front detection short-circuited before it"
fi

# --- Test 5: success path never moves the orchestrator checkout (#3358) ---
echo ""
echo "Test 5: pr-worktree.sh checks the PR branch out in pr-<N>, leaving the orchestrator checkout untouched (#3358)"

# Still inside "$TMP/repo", on main. PR 999 / fix/foo-bar collides with nothing.
# The PR branch must be a real commit AHEAD of main: with `update-ref ... HEAD`
# a mis-directed checkout would move the branch name but not the SHA, which
# would leave the SHA assertion below unable to fail.
OK_BRANCH="fix/foo-bar"
git checkout -q -b "$OK_BRANCH"
git commit --allow-empty -q -m "pr 999 change"
git checkout -q main
ORCH_BRANCH_BEFORE="$(git rev-parse --abbrev-ref HEAD)"
ORCH_SHA_BEFORE="$(git rev-parse HEAD)"

MOCKBIN3=$(mktemp -d /tmp/loom-pr-iso-mockbin3.XXXXXX)
trap 'rm -rf "$TMP" "$MOCKBIN" "$MOCKBIN2" "$MOCKBIN3"; cd "$REPO_ROOT" 2>/dev/null || true' EXIT
cat > "$MOCKBIN3/gh" <<'MOCKEOF'
#!/usr/bin/env bash
# `gh pr view` resolves the head branch; `gh pr checkout` performs a REAL
# `git checkout` in its cwd, so wherever pr-worktree.sh runs it is where HEAD
# moves. (Unlike the Test 3 mock, which only prints a canned failure.)
if [[ "$1" == "pr" && "$2" == "view" ]]; then
    echo "$LOOM_TEST_HEAD_BRANCH"
    exit 0
fi
if [[ "$1" == "pr" && "$2" == "checkout" ]]; then
    git checkout -q "$LOOM_TEST_HEAD_BRANCH"
    exit $?
fi
exit 0
MOCKEOF
chmod +x "$MOCKBIN3/gh"

set +e
PR_OUT3=$(PATH="$MOCKBIN3:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$OK_BRANCH" \
    "$PR_WORKTREE_SH" 999 2>&1)
PR_RC3=$?
set -e

if [[ "$PR_RC3" -eq 0 ]]; then
    pass "pr-worktree.sh exits 0 for a non-colliding PR"
else
    fail "pr-worktree.sh failed for a non-colliding PR (rc=$PR_RC3): $PR_OUT3"
fi
ORCH_BRANCH_AFTER="$(git rev-parse --abbrev-ref HEAD)"
ORCH_SHA_AFTER="$(git rev-parse HEAD)"
if [[ "$ORCH_BRANCH_AFTER" == "$ORCH_BRANCH_BEFORE" ]]; then
    pass "orchestrator checkout stays on '$ORCH_BRANCH_BEFORE' (#3358)"
else
    fail "orchestrator checkout moved from '$ORCH_BRANCH_BEFORE' to '$ORCH_BRANCH_AFTER' (#3358 regression)"
fi
if [[ "$ORCH_SHA_AFTER" == "$ORCH_SHA_BEFORE" ]]; then
    pass "orchestrator HEAD SHA is unchanged"
else
    fail "orchestrator HEAD moved from $ORCH_SHA_BEFORE to $ORCH_SHA_AFTER (#3358 regression)"
fi
PR999_BRANCH="$(git -C ".loom/worktrees/pr-999" rev-parse --abbrev-ref HEAD 2>/dev/null || echo MISSING)"
if [[ "$PR999_BRANCH" == "$OK_BRANCH" ]]; then
    pass "pr-999 worktree is on the PR branch ($OK_BRANCH)"
else
    fail "pr-999 worktree is on '$PR999_BRANCH', expected '$OK_BRANCH'"
fi
if grep -qx "# PR: 999" ".loom/worktrees/pr-999/.loom-managed" 2>/dev/null; then
    pass "pr-999 worktree has the .loom-managed sentinel written by the helper"
else
    fail "pr-999 worktree is missing the helper-written .loom-managed sentinel with '# PR: 999'"
fi

# --- Test 7: branch held by the primary clone -> detached pr-<N> (#14391) ---
echo ""
echo "Test 7: pr-worktree.sh creates a detached pr-<N> when the primary clone holds the PR branch (#14391)"

cd "$TMP/repo"
PRIMARY_ABS="$(pwd -P)"

MOCKBIN4=$(mktemp -d /tmp/loom-pr-iso-mockbin4.XXXXXX)
trap 'rm -rf "$TMP" "$MOCKBIN" "$MOCKBIN2" "$MOCKBIN3" "$MOCKBIN4"; cd "$REPO_ROOT" 2>/dev/null || true' EXIT
cat > "$MOCKBIN4/gh" <<'MOCKEOF'
#!/usr/bin/env bash
# LOOM_TEST_VIEW=none makes `gh pr view` resolve nothing (exercises the
# checkout-failure path); otherwise it returns the head branch or, for
# --json headRefOid, the head sha. `gh pr checkout` fails the way git does
# when the primary clone holds the branch.
if [[ "$1" == "pr" && "$2" == "view" ]]; then
    [[ "${LOOM_TEST_VIEW:-}" == "none" ]] && exit 1
    if [[ "$*" == *headRefOid* ]]; then echo "$LOOM_TEST_HEAD_SHA"; else echo "$LOOM_TEST_HEAD_BRANCH"; fi
    exit 0
fi
if [[ "$1" == "pr" && "$2" == "checkout" ]]; then
    echo "fatal: '$LOOM_TEST_HEAD_BRANCH' is already used by worktree at '$LOOM_TEST_PRIMARY'" >&2
    exit 1
fi
exit 0
MOCKEOF
chmod +x "$MOCKBIN4/gh"

# 7a: up-front path (gh pr view resolves the branch and the sha).
P_BRANCH="feature/issue-996"
git checkout -q -b "$P_BRANCH" main
git commit --allow-empty -q -m "pr 996 change"
P_SHA="$(git rev-parse HEAD)"   # primary clone stays on $P_BRANCH
set +e
PR_OUT7=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$P_BRANCH" \
    LOOM_TEST_HEAD_SHA="$P_SHA" LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 996 2>&1)
PR_RC7=$?
set -e
if [[ "$PR_RC7" -eq 0 ]]; then
    pass "primary-clone holder: pr-worktree.sh exits 0"
else
    fail "primary-clone holder: expected exit 0, got $PR_RC7: $PR_OUT7"
fi
if grep -q "Detached at $P_SHA" <<<"$PR_OUT7" && grep -q "primary clone" <<<"$PR_OUT7"; then
    pass "primary-clone holder: output says it detached, at which sha, and why"
else
    fail "primary-clone holder: missing detach explanation; got: $PR_OUT7"
fi
if [[ -z "$(git -C .loom/worktrees/pr-996 symbolic-ref -q HEAD || true)" ]] \
    && [[ "$(git -C .loom/worktrees/pr-996 rev-parse HEAD)" == "$P_SHA" ]]; then
    pass "primary-clone holder: pr-996 is detached at the PR head sha"
else
    fail "primary-clone holder: pr-996 is not detached at $P_SHA"
fi
if grep -qx "# PR: 996" ".loom/worktrees/pr-996/.loom-managed" 2>/dev/null; then
    pass "primary-clone holder: .loom-managed sentinel written"
else
    fail "primary-clone holder: .loom-managed sentinel missing"
fi
if [[ "$(git rev-parse --abbrev-ref HEAD)" == "$P_BRANCH" ]]; then
    pass "primary-clone holder: primary clone checkout is untouched"
else
    fail "primary-clone holder: primary clone moved off $P_BRANCH"
fi

# 7b: re-run reuses the existing pr-996 and follows a new PR head sha.
git commit --allow-empty -q -m "pr 996 follow-up"
P_SHA2="$(git rev-parse HEAD)"
set +e
PR_OUT7B=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$P_BRANCH" \
    LOOM_TEST_HEAD_SHA="$P_SHA2" LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 996 2>&1)
PR_RC7B=$?
set -e
if [[ "$PR_RC7B" -eq 0 ]] && grep -q "reusing" <<<"$PR_OUT7B" \
    && [[ "$(git -C .loom/worktrees/pr-996 rev-parse HEAD)" == "$P_SHA2" ]]; then
    pass "re-run reuses the detached pr-996 and moves it to the new head sha"
else
    fail "re-run did not reuse/refresh pr-996 (rc=$PR_RC7B): $PR_OUT7B"
fi

# 7c: checkout-failure path (gh pr view unresolved); head sha comes from the
#     pull/<N>/head ref fetched from origin.
Q_BRANCH="feature/issue-995"
git checkout -q -b "$Q_BRANCH" main
git commit --allow-empty -q -m "pr 995 change"
Q_SHA="$(git rev-parse HEAD)"
git push -q origin "$Q_SHA:refs/pull/995/head"
set +e
PR_OUT7C=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_VIEW=none LOOM_TEST_HEAD_BRANCH="$Q_BRANCH" \
    LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 995 2>&1)
PR_RC7C=$?
set -e
if [[ "$PR_RC7C" -eq 0 ]] && [[ "$(git -C .loom/worktrees/pr-995 rev-parse HEAD 2>/dev/null)" == "$Q_SHA" ]] \
    && grep -q "Detached at $Q_SHA" <<<"$PR_OUT7C"; then
    pass "checkout-failure path: primary-clone holder also yields a detached pr-995"
else
    fail "checkout-failure path: expected detached pr-995 at $Q_SHA (rc=$PR_RC7C): $PR_OUT7C"
fi
if [[ "$(git worktree list --porcelain | grep -c 'worktrees/pr-995$')" -eq 1 ]]; then
    pass "checkout-failure path: exactly one pr-995 worktree registered"
else
    fail "checkout-failure path: unexpected pr-995 worktree registrations"
fi
git checkout -q main

# 7d: primary clone AND the builder's issue worktree both hold the branch ->
#     the builder wins: exit 1, builder path named, no detached pr-<N> created.
B_BRANCH="feature/issue-994"
git checkout -q -b "$B_BRANCH" main
git commit --allow-empty -q -m "pr 994 change"
B_SHA="$(git rev-parse HEAD)"
git worktree add -q --force .loom/worktrees/issue-994 "$B_BRANCH"
set +e
PR_OUT7D=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$B_BRANCH" \
    LOOM_TEST_HEAD_SHA="$B_SHA" LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 994 2>&1)
PR_RC7D=$?
set -e
if [[ "$PR_RC7D" -eq 1 ]] && grep -q "worktrees/issue-994" <<<"$PR_OUT7D" \
    && [[ ! -e .loom/worktrees/pr-994 ]]; then
    pass "builder + primary both hold the branch: exit 1, builder path named, no pr-994 created"
else
    fail "builder + primary: expected exit 1 naming issue-994 and no pr-994 (rc=$PR_RC7D): $PR_OUT7D"
fi
git worktree remove --force .loom/worktrees/issue-994
git checkout -q main

# 7e: existing detached pr-996 is NOT reused once the builder holds the branch.
git worktree add -q .loom/worktrees/issue-996 "$P_BRANCH"
set +e
PR_OUT7E=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$P_BRANCH" \
    LOOM_TEST_HEAD_SHA="$P_SHA2" LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 996 2>&1)
PR_RC7E=$?
set -e
if [[ "$PR_RC7E" -eq 1 ]] && grep -q "worktrees/issue-996" <<<"$PR_OUT7E"; then
    pass "existing detached pr-996 + builder holder: exit 1, builder path named"
else
    fail "reuse path: expected exit 1 naming issue-996 (rc=$PR_RC7E): $PR_OUT7E"
fi
git worktree remove --force .loom/worktrees/issue-996

# 7f: a failed refresh of the detached pr-996 returns nonzero and keeps the local edit.
git checkout -q -b tmp-14391-b "$P_SHA2"
echo base > t14391.txt; git add t14391.txt; git commit -q -m "base t"
T_B="$(git rev-parse HEAD)"
echo new > t14391.txt; git commit -q -am "new t"
T_C="$(git rev-parse HEAD)"
git checkout -q main
git -C .loom/worktrees/pr-996 checkout -q --detach "$T_B"
echo local > .loom/worktrees/pr-996/t14391.txt
set +e
PR_OUT7F=$(PATH="$MOCKBIN4:$PATH" LOOM_DEFAULT_BRANCH=main LOOM_TEST_HEAD_BRANCH="$P_BRANCH" \
    LOOM_TEST_HEAD_SHA="$T_C" LOOM_TEST_PRIMARY="$PRIMARY_ABS" "$PR_WORKTREE_SH" 996 2>&1)
PR_RC7F=$?
set -e
if [[ "$PR_RC7F" -ne 0 ]] && [[ "$(cat .loom/worktrees/pr-996/t14391.txt)" == "local" ]]; then
    pass "failed detached refresh: nonzero exit and local edit preserved"
else
    fail "failed refresh: expected nonzero + preserved edit (rc=$PR_RC7F): $PR_OUT7F"
fi

cd "$REPO_ROOT"

# --- Test 6: Judge guidance and helper --help agree on the fallback (#11194) ---
echo ""
echo "Test 6: judge.md and pr-worktree.sh --help agree (no fabricated judge-* checkout)"
JUDGE_MD="$REPO_ROOT/defaults/.claude/commands/loom/judge.md"
[[ -f "$REPO_ROOT/.claude/commands/loom/judge.md" ]] && JUDGE_MD="$REPO_ROOT/.claude/commands/loom/judge.md"
HELP_OUT=$("$PR_WORKTREE_SH" --help 2>&1)
assert_grep "pr-worktree\.sh <PR_NUMBER>" "$JUDGE_MD" \
    "judge.md routes the no-builder-checkout case through pr-worktree.sh"
assert_grep "Never.*invent a .judge-\*" "$JUDGE_MD" \
    "judge.md forbids inventing a judge-* checkout after a helper failure"
if grep -qE "Judge's supported review checkout" <<<"$HELP_OUT" \
    && grep -qE "names the holder" <<<"$HELP_OUT" \
    && grep -qE "invent a judge-\* directory" <<<"$HELP_OUT"; then
    pass "pr-worktree.sh --help states the Judge fallback and its collision failure behavior"
else
    fail "pr-worktree.sh --help does not match the Judge fallback guidance"
fi
if grep -qE "use worktree\.sh instead" <<<"$HELP_OUT"; then
    fail "pr-worktree.sh --help still redirects issue PRs to worktree.sh"
else
    pass "pr-worktree.sh --help no longer contradicts judge.md"
fi

# --- Test 8: run the documented selection against disposable checkouts ---
echo ""
echo "Test 8: Judge and Doctor check head repository metadata before selection"
SELECT="$TMP/selection"
mkdir -p "$SELECT/bin"
cat > "$SELECT/bin/gh" <<'EOF'
#!/usr/bin/env bash
case "$*" in
    "pr view 71 --repo fixture/loom --json isCrossRepository,headRefName,headRepository")
        [[ "$FIXTURE_JSON" != unreadable ]] || exit 1
        printf '%s\n' "$FIXTURE_JSON"
        ;;
    "pr view 71 --json headRefName --jq .headRefName")
        jq -r .headRefName <<<"$FIXTURE_JSON"
        ;;
    "pr checkout 71 --repo fixture/loom")
        printf 'checkout %s\n' "${PWD##*/}" >>"$FIXTURE_CALLS"
        ;;
    *)
        printf 'unexpected forge command\n' >>"$FIXTURE_CALLS"
        exit 1
        ;;
esac
EOF
chmod +x "$SELECT/bin/gh"

for role in judge doctor; do
    prompt="$REPO_ROOT/defaults/.claude/commands/loom/$role.md"
    [[ -f "$prompt" ]] || prompt="$REPO_ROOT/.claude/commands/loom/$role.md"
    awk '/^# PR worktree selection$/{capture=1;next}
         /^# End PR worktree selection$/{exit}
         capture && /^```$/{fence=1;next}
         capture && /^```bash$/{fence=0;next}
         capture && !fence' "$prompt" >"$SELECT/$role.sh"
    cat >>"$SELECT/$role.sh" <<'EOF'
printf 'entered %s\n' "${PWD##*/}" >>"$FIXTURE_CALLS"
EOF
done

run_selection() {
    local role="$1" name="$2" json="$3" holder="$4" alternate="$5" expected="$6"
    local repo="$SELECT/$role-$name" calls rc=0 main
    mkdir -p "$repo/.loom/scripts" "$repo/.loom/worktrees"
    git init -q -b main "$repo"
    git -C "$repo" -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false \
        commit -q --allow-empty -m fixture
    git -C "$repo" remote add origin "$TMP/origin.git"
    git -C "$repo" branch feature/issue-71
    main="$(git -C "$repo" rev-parse HEAD)"
    ln -s "$SCRIPTS_DIR/lib" "$repo/.loom/scripts/lib"
    if [[ "$holder" = present ]]; then
        git -C "$repo" worktree add -q "$repo/.loom/worktrees/issue-71" feature/issue-71
    fi
    cat >"$repo/.loom/scripts/worktree.sh" <<'EOF'
#!/usr/bin/env bash
set -e
printf 'issue helper\n' >>"$FIXTURE_CALLS"
if [[ ! -d .loom/worktrees/issue-71 ]]; then
    git worktree add -q .loom/worktrees/issue-71 feature/issue-71
fi
EOF
    cat >"$repo/.loom/scripts/pr-worktree.sh" <<'EOF'
#!/usr/bin/env bash
set -e
printf 'PR helper\n' >>"$FIXTURE_CALLS"
case "$FIXTURE_ALT" in
    real) exec "$FIXTURE_PR_HELPER" "$@" ;;
    failed) exit 1 ;;
    detached) git worktree add -q --detach .loom/worktrees/pr-71 HEAD ;;
    *) git worktree add -q -b fixture-pr .loom/worktrees/pr-71 HEAD ;;
esac
EOF
    chmod +x "$repo/.loom/scripts/"*.sh
    calls="$repo/calls"
    (cd "$repo" && PATH="$SELECT/bin:$PATH" PR_NUMBER=71 PR_REPO=fixture/loom \
        LOOM_WORKTREE_ROOT='' LOOM_DEFAULT_BRANCH=main \
        FIXTURE_JSON="$json" FIXTURE_CALLS="$calls" FIXTURE_ALT="$alternate" \
        FIXTURE_PR_HELPER="$PR_WORKTREE_SH" bash "$SELECT/$role.sh") >"$repo/trace" 2>&1 || rc=$?
    assert_eq "$expected" "$(cat "$calls" 2>/dev/null || true)" "$role $name records only the expected actions"
    assert_eq "$main" "$(git -C "$repo" rev-parse HEAD)" "$role $name preserves the primary HEAD"
    if [[ "$expected" = *"entered "* ]]; then
        assert_eq 0 "$rc" "$role $name succeeds"
    else
        assert_eq 1 "$rc" "$role $name stops before proceeding"
    fi
    if [[ "$holder" = present ]]; then
        assert_eq "$main" "$(git -C "$repo/.loom/worktrees/issue-71" rev-parse HEAD)" \
            "$role $name preserves the existing issue HEAD"
    fi
}
same='{"headRefName":"feature/issue-71","isCrossRepository":false,"headRepository":{"nameWithOwner":"Fixture/Loom"}}'
fork='{"headRefName":"feature/issue-71","isCrossRepository":true,"headRepository":{"nameWithOwner":"another/loom"}}'
ad_hoc='{"headRefName":"fix/ad-hoc","isCrossRepository":false,"headRepository":{"nameWithOwner":"fixture/loom"}}'
# Match the shared Forge slug validator's per-segment bounds and dot rules.
long_segment=$(printf '%101s' '' | tr ' ' 'a')
max_segment=${long_segment%a}
fork_named() {
    jq -cn --arg slug "$1" '{headRefName:"feature/issue-71",isCrossRepository:true,headRepository:{nameWithOwner:$slug}}'
}
for role in judge doctor; do
    run_selection "$role" fork-collision "$fork" present real 'PR helper'
    run_selection "$role" valid-dotted-repo "$(fork_named 'another/.github')" absent ready $'PR helper\nentered pr-71'
    run_selection "$role" max-length-slug "$(fork_named "$max_segment/$max_segment")" absent ready $'PR helper\nentered pr-71'
    run_selection "$role" invalid-owner-dot "$(fork_named '.team/loom')" present ready ''
    run_selection "$role" invalid-owner-hyphen "$(fork_named '-team/loom')" present ready ''
    run_selection "$role" invalid-repo-hyphen "$(fork_named 'another/-loom')" present ready ''
    run_selection "$role" invalid-repo-single-dot "$(fork_named 'another/.')" present ready ''
    run_selection "$role" invalid-repo-double-dot "$(fork_named 'another/..')" present ready ''
    run_selection "$role" overlong-owner "$(fork_named "$long_segment/loom")" present ready ''
    run_selection "$role" overlong-repo "$(fork_named "another/$long_segment")" present ready ''
    run_selection "$role" fork-without-holder "$fork" absent ready $'PR helper\nentered pr-71'
    run_selection "$role" ad-hoc "$ad_hoc" absent ready $'PR helper\nentered pr-71'
    run_selection "$role" helper-failure "$fork" absent failed 'PR helper'
    run_selection "$role" unreadable unreadable absent ready ''
    run_selection "$role" malformed-json '{' absent ready ''
    run_selection "$role" absent-branch '{"isCrossRepository":false,"headRepository":{"nameWithOwner":"fixture/loom"}}' present ready ''
    run_selection "$role" absent-flag '{"headRefName":"feature/issue-71","headRepository":{"nameWithOwner":"fixture/loom"}}' present ready ''
    run_selection "$role" absent-repository '{"headRefName":"feature/issue-71","isCrossRepository":false}' present ready ''
    run_selection "$role" malformed-repository "${same/Fixture\/Loom/Fixture}" present ready ''
    run_selection "$role" inconsistent-same "${fork/true/false}" present ready ''
    run_selection "$role" inconsistent-cross "${same/false/true}" present ready ''
    run_selection "$role" malformed-boolean '{"headRefName":"feature/issue-71","isCrossRepository":"false","headRepository":{"nameWithOwner":"fixture/loom"}}' present ready ''
    run_selection "$role" invalid-ref "${same/issue-71/issue-71..}" present ready ''
done
run_selection judge existing-managed "$same" present ready 'entered issue-71'
run_selection judge missing-managed "$same" absent ready $'PR helper\nentered pr-71'
run_selection doctor managed "$same" absent ready $'issue helper\ncheckout issue-71\nentered issue-71'
run_selection judge detached "$ad_hoc" absent detached $'PR helper\nentered pr-71'
run_selection doctor detached "$ad_hoc" absent detached 'PR helper'

# --- Summary ---
echo ""
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
