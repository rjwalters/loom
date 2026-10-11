#!/usr/bin/env bash
# test-judge-payload-drop-guard.sh - Behavioral coverage for issues #8298
# and #9282.
#
# THE FAILURE MODE THIS GUARDS AGAINST
#
# judge.md's "If DIRTY: Attempt Automated Rebase" workflow runs
# `git rebase origin/main` and, on exit 0, proceeds straight to
# `git push --force-with-lease`. The incident: two independent single-line
# edits on adjacent lines of `.loom/install-metadata.json` (`loom_version`
# and `loom_commit`), a path carrying `merge=ours`, so the replayed PR commit
# resolves to nothing -- `git rebase` reports success (exit 0, no conflict)
# but the resulting tree is byte-identical to `origin/main`, discarding the
# PR's actual payload.
#
# Since #9282 the block no longer carries its own empty-diff predicate: it
# asks `loom-daemon forge rebase-guard`, which also reports a hand-made merge
# resolution the rebase dropped. This suite covers the WIRING -- what the
# block does with each of the verb's three answers -- through a stub on PATH.
# What the verb itself answers for which git shape is covered by the Rust
# fixtures in loom-daemon/src/rebase_guard/tests.rs.
#
# WHAT THIS SUITE DOES
#
# Deliberately NOT a prose-existence assertion (see
# test-judge-tdd-merge-base.sh's header for the citation). EXTRACTS the real
# fenced ```bash``` block shipped under "If DIRTY: Attempt Automated Rebase"
# in judge.md and RUNS it (real `git`, stubbed `gh`/`post-verdict.sh`)
# against purpose-built git fixtures:
#
#   1. The exact repro shape (#8298): a rebase that silently empties the PR
#      must be blocked -- the push must never happen, and the SAME
#      merge-conflict fallback (`loom:merge-conflict` +
#      `loom:changes-requested`) already used for "rebase failed (complex
#      conflicts)" must fire.
#   2. The common case: a genuinely successful rebase (real content
#      difference vs origin/main afterward) must be UNAFFECTED -- push still
#      proceeds, no false positive.
#   3. RESOLUTION_DISCARDED (#9282) blocks the push the same way.
#   4. An unanswered guard (a daemon predating the verb) warns loudly and
#      does NOT block the push.
#
# Hermetic: local git fixtures (a local bare "origin") under a private
# TMPDIR. No forge, no network.
#
# Usage:
#   ./.loom/scripts/tests/test-judge-payload-drop-guard.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"

# Installed layout (.loom/scripts/tests -> two `..` to repo root) vs. this
# source repo (defaults/scripts/tests -> one `..`). Probe both (#6725).
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../../.claude/commands/loom" && pwd)"
else
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../.claude/commands/loom" && pwd)"
fi
JUDGE_MD="$PROMPT_DIR/judge.md"

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
        fail "$msg (missing '$needle' in: ${haystack//$'\n'/ | })"
    fi
}

assert_not_contains() {
    local haystack="$1" needle="$2" msg="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        pass "$msg"
    else
        fail "$msg (unexpectedly found '$needle')"
    fi
}

[[ -f "$JUDGE_MD" ]] || { echo "FATAL: subject not found: $JUDGE_MD" >&2; exit 1; }

# =====================================================================
# Extract the shipped ```bash``` fence under "If DIRTY: Attempt Automated
# Rebase" -- the real code a Judge session runs, not a local mirror.
# =====================================================================
FENCE_SRC="$(awk '
    /^### If DIRTY: Attempt Automated Rebase$/ { insec = 1 }
    insec && /^```bash$/ && !infence { infence = 1; depth = 1; next }
    infence && /^```bash$/ { depth++; print; next }
    infence && /^```$/ {
        depth--
        if (depth == 0) exit
        print
        next
    }
    infence { print }
' "$JUDGE_MD")"

if [[ -z "$FENCE_SRC" || "$FENCE_SRC" != *"forge rebase-guard --base origin/main"* ]]; then
    echo "FATAL: could not extract the DIRTY-rebase fence (with the #9282 rebase-guard call) from $JUDGE_MD" >&2
    exit 1
fi
pass "extracted the shipped DIRTY-rebase block (with the rebase-guard call) from judge.md"
assert_not_contains "$FENCE_SRC" "PRE_REBASE_AHEAD" \
    "the block no longer carries its own inline empty-diff predicate (#9282)"

# The first line is a literal `PR_NUMBER=<number>` placeholder for a human/
# agent to fill in -- replace it with a real value so the block is runnable,
# leaving everything else byte-for-byte as shipped.
FENCE_SRC="$(printf '%s\n' "$FENCE_SRC" | sed '1s/^PR_NUMBER=<number>$/PR_NUMBER=999/')"
if [[ "${FENCE_SRC%%$'\n'*}" != "PR_NUMBER=999" ]]; then
    echo "FATAL: judge.md's DIRTY-rebase fence no longer starts with 'PR_NUMBER=<number>' -- update this test's substitution" >&2
    exit 1
fi

if ! bash -n <(printf '%s\n' "$FENCE_SRC"); then
    echo "FATAL: the shipped DIRTY-rebase block is not valid bash" >&2
    exit 1
fi
pass "the shipped DIRTY-rebase block parses as bash"

# =====================================================================
# Shared fixture helpers
# =====================================================================
run_dirty_block() {
    # Runs the extracted block in $1 (a repo dir already checked out on the
    # PR branch, with origin fetched). Stubs `gh`, `post-verdict.sh` and
    # `loom-daemon`; everything else (fetch/rebase/diff/push) is real git.
    local repo="$1"
    (
        cd "$repo" || exit 1
        export PATH="$repo/.loom/stub-bin:$PATH"
        bash -c "$FENCE_SRC"
    )
}

make_fixture() {
    # make_fixture <workdir> -- sets up a bare "origin" plus a working repo
    # on branch feature/issue-999, with .loom/worktrees/issue-999 symlinked
    # back to the repo root (so the block's own worktree-cd is a no-op) and
    # stubs for `gh` and `./.loom/scripts/post-verdict.sh`.
    local base="$1"
    local repo="$base/repo"
    mkdir -p "$base/origin.git"
    git init -q --bare "$base/origin.git"
    git init -q "$repo"
    (
        cd "$repo" || exit 1
        git config user.email "test@loom.test"
        git config user.name "Loom Test"
        git config commit.gpgsign false
        git remote add origin "$base/origin.git"
    )

    mkdir -p "$repo/.loom/worktrees" "$repo/.loom/scripts" "$repo/.loom/stub-bin"
    ln -s "../.." "$repo/.loom/worktrees/issue-999"

    cat > "$repo/.loom/scripts/post-verdict.sh" <<'STUB'
#!/usr/bin/env bash
echo "post-verdict.sh called: $*" >> "$(dirname "$0")/../../post-verdict.calls"
exit 0
STUB
    chmod +x "$repo/.loom/scripts/post-verdict.sh"

    cat > "$repo/.loom/stub-bin/gh" <<'STUB'
#!/usr/bin/env bash
LOG="$(git rev-parse --show-toplevel 2>/dev/null)/gh.calls"
echo "$*" >> "$LOG"
case "$*" in
    *"--json mergeStateStatus"*) echo "DIRTY" ;;
    *"--json headRefName"*) echo "feature/issue-999" ;;
    *) exit 0 ;;
esac
STUB
    chmod +x "$repo/.loom/stub-bin/gh"

    # `loom-daemon forge rebase-guard` stand-in. GUARD_MODE picks the answer:
    #   emptied (default) -- the verb's EMPTIED predicate in miniature: blocked
    #                        (rc 0) when HEAD's tree equals the base's, else
    #                        safe (rc 1);
    #   discarded         -- RESOLUTION_DISCARDED, rc 0;
    #   predates          -- clap's "unrecognized subcommand", rc 2.
    cat > "$repo/.loom/stub-bin/loom-daemon" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$(git rev-parse --show-toplevel 2>/dev/null)/daemon.calls"
[[ "$1 $2 $3" == "forge rebase-guard --base" ]] || exit 64
case "${GUARD_MODE:-emptied}" in
    discarded) echo "RESOLUTION_DISCARDED 0123abc"; echo "BLOCKER: stub" >&2; exit 0 ;;
    predates) echo "error: unrecognized subcommand 'rebase-guard'" >&2; exit 2 ;;
esac
if git diff --quiet "$4" HEAD; then echo "EMPTIED"; echo "BLOCKER: stub" >&2; exit 0; fi
exit 1
STUB
    chmod +x "$repo/.loom/stub-bin/loom-daemon"

    printf '%s' "$repo"
}

WORKROOT="$(mktemp -d)"
trap 'rm -rf "$WORKROOT"' EXIT

# =====================================================================
# Case 1: the #8298 repro shape -- adjacent single-line JSON edits on a
# `merge=ours` path, so the replayed PR commit resolves to nothing and the
# rebase drops it while reporting success.
# =====================================================================
echo
echo "--- #8298 repro: rebase silently drops the PR's payload ---"

BASE1="$WORKROOT/case1"
REPO1="$(make_fixture "$BASE1")"

echo "install-metadata.json merge=ours" > "$REPO1/.gitattributes"
(cd "$REPO1" && git config merge.ours.driver true)
cat > "$REPO1/install-metadata.json" <<'EOF'
{
  "loom_version": "1.1",
  "loom_commit": "aaaa"
}
EOF
(cd "$REPO1" && git add -A && git commit -qm base && git push -q origin HEAD:main)

(cd "$REPO1" && git checkout -qb feature/issue-999)
cat > "$REPO1/install-metadata.json" <<'EOF'
{
  "loom_version": "1.1",
  "loom_commit": "bbbb"
}
EOF
(cd "$REPO1" && GIT_AUTHOR_DATE="2026-01-01T00:00:00" GIT_COMMITTER_DATE="2026-01-01T00:00:00" \
    git commit -qam "chore: resync installed Loom surfaces (PR)" && \
    git push -qu origin feature/issue-999)

(cd "$REPO1" && git checkout -q main)
cat > "$REPO1/install-metadata.json" <<'EOF'
{
  "loom_version": "1.2",
  "loom_commit": "aaaa"
}
EOF
(cd "$REPO1" && GIT_AUTHOR_DATE="2026-01-02T00:00:00" GIT_COMMITTER_DATE="2026-01-02T00:00:00" \
    git commit -qam "chore: bump version (main)" && git push -q origin HEAD:main)

(cd "$REPO1" && git checkout -q feature/issue-999)
PRE_AHEAD="$(cd "$REPO1" && git rev-list --count origin/main..HEAD 2>/dev/null || echo "?")"
assert_eq "1" "$PRE_AHEAD" "sanity: the PR branch carries one real commit ahead of origin/main before the block runs"

OUT1="$(run_dirty_block "$REPO1" 2>&1)"

assert_contains "$OUT1" "EMPTIED" \
    "the guard's blocked answer is surfaced"
assert_contains "$OUT1" "Not pushed - falling back to change request" \
    "the block reports that it did not push"
assert_not_contains "$OUT1" "Rebase successful - proceeding with evaluation" \
    "the success path never runs"
assert_eq "0" "$(cd "$REPO1" && git rev-list --count origin/main..HEAD)" \
    "sanity: git really did collapse the branch to origin/main's content (the underlying hazard)"
assert_contains "$(cat "$REPO1/daemon.calls" 2>/dev/null)" "forge rebase-guard --base origin/main" \
    "the block asked loom-daemon forge rebase-guard"
assert_eq "1" "$(git --git-dir="$BASE1/origin.git" rev-list --count main..feature/issue-999)" \
    "the emptied branch was NOT pushed: origin still holds the PR's commit"

CALLS1="$(cat "$REPO1/post-verdict.calls" 2>/dev/null || echo "")"
assert_contains "$CALLS1" "changes-requested" \
    "falls back through the EXISTING post-verdict.sh changes-requested path (not a new one)"

GH_CALLS1="$(cat "$REPO1/gh.calls" 2>/dev/null || echo "")"
assert_contains "$GH_CALLS1" "loom:merge-conflict" \
    "reuses the existing loom:merge-conflict label"
assert_contains "$GH_CALLS1" "loom:changes-requested" \
    "reuses the existing loom:changes-requested label"

# =====================================================================
# Case 2: no regression -- a genuine successful rebase (real content
# difference vs origin/main afterward) must be unaffected.
# =====================================================================
echo
echo "--- no regression: a normal DIRTY rebase with real post-rebase content still pushes ---"

BASE2="$WORKROOT/case2"
REPO2="$(make_fixture "$BASE2")"

echo "unrelated base content" > "$REPO2/other.txt"
(cd "$REPO2" && git add -A && git commit -qm base && git push -q origin HEAD:main)

(cd "$REPO2" && git checkout -qb feature/issue-999)
echo "PR-only change" >> "$REPO2/other.txt"
(cd "$REPO2" && git commit -qam "pr: add a line" && git push -qu origin feature/issue-999)

(cd "$REPO2" && git checkout -q main)
echo "main-only change" > "$REPO2/main-only.txt"
(cd "$REPO2" && git add -A && git commit -qam "main: unrelated file" && git push -q origin HEAD:main)

(cd "$REPO2" && git checkout -q feature/issue-999)
PRE_AHEAD2="$(cd "$REPO2" && git rev-list --count origin/main..HEAD 2>/dev/null || echo "?")"
assert_eq "1" "$PRE_AHEAD2" "sanity: the PR branch carries one real commit ahead of origin/main before the block runs"

# `git push --force-with-lease` in the extracted block would try to reach
# the real "origin" -- that IS our local bare repo here, so let it push for
# real and assert on the result instead of stubbing it away.
OUT2="$(run_dirty_block "$REPO2" 2>&1)"

assert_contains "$OUT2" "Rebase successful - proceeding with evaluation" \
    "the normal success path runs when the rebase genuinely changes content"
assert_not_contains "$OUT2" "Not pushed - falling back to change request" \
    "the guard does NOT block a real, non-empty rebase result (no false positive)"
assert_not_contains "$OUT2" "WARNING: rebase-guard gave no answer" \
    "a safe answer (rc 1) is not reported as unanswered"

POST_REBASE_DIFF_EMPTY2=0
(cd "$REPO2" && git diff --quiet origin/main HEAD) && POST_REBASE_DIFF_EMPTY2=1
assert_eq "0" "$POST_REBASE_DIFF_EMPTY2" \
    "sanity: the rebased branch genuinely still differs from origin/main"

CALLS2="$(cat "$REPO2/post-verdict.calls" 2>/dev/null || echo "")"
assert_eq "" "$CALLS2" "the merge-conflict fallback is never invoked on the common-case path"

REMOTE_PR_HEAD2="$(git --git-dir="$BASE2/origin.git" rev-parse feature/issue-999)"
LOCAL_HEAD2="$(cd "$REPO2" && git rev-parse HEAD)"
assert_eq "$REMOTE_PR_HEAD2" "$LOCAL_HEAD2" \
    "the push actually landed on the (local, hermetic) origin -- the common case still pushes"

# make_behind_fixture <workdir> -- the Case 2 shape: one real PR commit, main
# moved in an unrelated file, so the rebase is clean and non-empty.
make_behind_fixture() {
    local repo
    repo="$(make_fixture "$1")"
    (
        cd "$repo" || exit 1
        echo "unrelated base content" > other.txt
        git add -A && git commit -qm base && git push -q origin HEAD:main
        git checkout -qb feature/issue-999
        echo "PR-only change" >> other.txt
        git commit -qam "pr: add a line" && git push -qu origin feature/issue-999
        git checkout -q main
        echo "main-only change" > main-only.txt
        git add -A && git commit -qam "main: unrelated file" && git push -q origin HEAD:main
        git checkout -q feature/issue-999
    ) >/dev/null
    printf '%s' "$repo"
}

# =====================================================================
# Case 3 (#9282): RESOLUTION_DISCARDED blocks the push exactly like EMPTIED,
# on a rebase whose result is clean and non-empty.
# =====================================================================
echo
echo "--- #9282: a dropped hand-made merge resolution blocks the push ---"

BASE3="$WORKROOT/case3"
REPO3="$(make_behind_fixture "$BASE3")"
REMOTE_BEFORE3="$(git --git-dir="$BASE3/origin.git" rev-parse feature/issue-999)"

OUT3="$(GUARD_MODE=discarded run_dirty_block "$REPO3" 2>&1)"

assert_contains "$OUT3" "RESOLUTION_DISCARDED" "the guard's blocked answer is surfaced"
assert_not_contains "$OUT3" "Rebase successful - proceeding with evaluation" \
    "the success path never runs"
assert_eq "$REMOTE_BEFORE3" "$(git --git-dir="$BASE3/origin.git" rev-parse feature/issue-999)" \
    "the push never happened: origin's PR head is unchanged"
assert_contains "$(cat "$REPO3/post-verdict.calls" 2>/dev/null)" "changes-requested" \
    "falls back through the existing post-verdict.sh changes-requested path"
assert_contains "$(cat "$REPO3/gh.calls" 2>/dev/null)" "loom:merge-conflict" \
    "reuses the existing loom:merge-conflict label"

# =====================================================================
# Case 4 (#9282): an UNANSWERED guard does not block. A role prompt newer
# than the installed binary must not strand every rebase in the fleet -- but
# it must say, loudly, that the push went out unchecked.
# =====================================================================
echo
echo "--- #9282: a daemon predating the verb warns and does not block ---"

BASE4="$WORKROOT/case4"
REPO4="$(make_behind_fixture "$BASE4")"

OUT4="$(GUARD_MODE=predates run_dirty_block "$REPO4" 2>&1)"

assert_contains "$OUT4" "WARNING: rebase-guard gave no answer (rc=2) - pushing UNCHECKED (#9282)" \
    "the unanswered guard is reported loudly, with its exit code"
assert_contains "$OUT4" "Rebase successful - proceeding with evaluation" \
    "the push still proceeds (non-blocking by design)"
assert_eq "$(cd "$REPO4" && git rev-parse HEAD)" "$(git --git-dir="$BASE4/origin.git" rev-parse feature/issue-999)" \
    "the rebased head landed on origin"
assert_eq "" "$(cat "$REPO4/post-verdict.calls" 2>/dev/null || echo "")" \
    "no change request is posted for an unanswered guard"

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
