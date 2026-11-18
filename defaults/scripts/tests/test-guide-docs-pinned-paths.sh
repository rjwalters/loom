#!/usr/bin/env bash
# test-guide-docs-pinned-paths.sh - Regression suite for issue #13897
#
# Guide's Document Maintenance phase had two structural blind spots that
# combined into a silent, indefinite stall on any repo that keeps its own
# machine-verified evidence records:
#
#   1. create_docs_pr() (Step 5) staged a FIXED file set
#      (`git add WORK_LOG.md WORK_PLAN.md README.md`). A repo that pins a
#      tracked file's content hash into a committed evidence record — CI
#      re-derives the hash and fails if it drifts — could never accept a docs
#      PR editing that file, so the first README-staleness edit authored a PR
#      whose CI could never pass.
#   2. Step 1's open-docs-PR skip was unconditional on that PR's
#      mergeability, so the red PR from (1) then suppressed the ENTIRE phase
#      for hours with nothing but a one-line "still open" echo — while
#      WORK_LOG.md/WORK_PLAN.md went stale behind it.
#
# The #13897 fix, verified here:
#   - guide.docsMaintenance.pinnedPaths (JSON array, default []) is read in
#     the same env > config > default shape as the poolPressure knobs (Step
#     4b); LOOM_GUIDE_DOCS_PINNED_PATHS is the test seam.
#   - stage_docs_paths() stages the three fixed doc paths MINUS pinned ones,
#     announces each skipped pinned path loudly (naming the path and the
#     repo's own regeneration duty), and the work_log/ archive loop skips
#     pinned archive paths too. A pinned-only diff stages nothing and falls
#     through create_docs_pr()'s existing "No document changes to commit."
#     path — no PR.
#   - Step 1, on finding an open docs/guide-update* PR, reads its
#     mergeable/mergeStateStatus and prints a loud WARNING when it cannot
#     land (CONFLICTING/DIRTY/mergeable=false/UNSTABLE). The skip itself
#     stays unconditional — UNKNOWN, BLOCKED-but-mergeable, or a failed read
#     yield the plain skip, NEVER a second PR.
#
# Hermetic: no forge, no network, no gh calls that leave the machine. The
# Step 1 scenarios run the extracted block verbatim against a stub `gh` and a
# stub docs-guide-lock.sh; the staging scenarios run the extracted functions
# verbatim inside a throwaway git repo. Extraction follows
# test-guide-pool-pressure-defer.sh's awk pattern so the suite can never
# silently drift from the actual prompt text.
#
# Usage:
#   ./.loom/scripts/tests/test-guide-docs-pinned-paths.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# guide.md is shipped (installed at .claude/commands/loom/guide.md), so
# resolve it the way each layout actually lays it out: the installed path
# first (consumer repos, and Loom's own dogfooded checkout), falling back
# to the defaults/ source-tree path (a bare source checkout with no
# .claude/commands/loom/ copy yet). See issue #6194 / #6241.
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Local assert_eq kept: takes (actual, expected, msg); shared assert_eq is (expected, actual, msg).
assert_eq() {
    local actual="$1" expected="$2" msg="$3"
    if [[ "$actual" == "$expected" ]]; then pass "$msg"; else fail "$msg (got '$actual', expected '$expected')"; fi
}

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found at $GUIDE_MD"
    exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
    echo -e "${RED}FATAL${NC}: jq is required for this suite"
    exit 2
fi

# ---------------------------------------------------------------------------
# Test 1: guide.md defines and wires the #13897 fix (structure).
# ---------------------------------------------------------------------------
echo "Test 1: guide.md defines and wires the #13897 fix"

assert_grep '^docs_pinned_paths\(\) \{' "$GUIDE_MD" \
    "docs_pinned_paths() is defined"
assert_grep 'guide\.docsMaintenance\.pinnedPaths // \[\] \| join\(" "\)' "$GUIDE_MD" \
    "the pinned set is read in the same jq // default style as the sibling docsMaintenance keys"
assert_grep 'LOOM_GUIDE_DOCS_PINNED_PATHS' "$GUIDE_MD" \
    "the LOOM_GUIDE_DOCS_PINNED_PATHS test seam is documented"
assert_grep '^stage_docs_paths\(\) \{' "$GUIDE_MD" \
    "stage_docs_paths() is defined"
assert_grep '^  stage_docs_paths$' "$GUIDE_MD" \
    "create_docs_pr() calls stage_docs_paths() for the three doc paths"
# The pre-#13897 hardcoded staging line must be gone entirely — a pinned path
# must never be reachable through a leftover literal `git add` list.
if grep -qF 'git -C "$DOCS_WT" add WORK_LOG.md WORK_PLAN.md README.md' "$GUIDE_MD"; then
    fail "(1) the hardcoded three-path git add line is still present (#13897 replaces it with stage_docs_paths)"
else
    pass "(1) the hardcoded three-path git add line is gone (staging goes through stage_docs_paths)"
fi
# ...and the work_log/ archive loop must guard pinned paths too.
if grep -qF '*" $archive_rel "*) continue ;;' "$GUIDE_MD"; then
    pass "(1) the work_log/ archive staging loop skips pinned paths too"
else
    fail "(1) the work_log/ archive staging loop must skip pinned paths (*\" \$archive_rel \"*) continue)"
fi
# Step 1 wiring: mergeability read + WARNING + unconditional skip.
assert_grep 'OPEN_DOCS_PR_STATE=\$\("\$GH_READ" pr view "\$OPEN_DOCS_PR" --json mergeable,mergeStateStatus' "$GUIDE_MD" \
    "Step 1 reads the open docs PR's mergeable/mergeStateStatus"
assert_grep '\*CONFLICTING\*\|\*DIRTY\*' "$GUIDE_MD" \
    "the WARNING case matches CONFLICTING and DIRTY states"
assert_grep '\*UNSTABLE\*' "$GUIDE_MD" \
    "the WARNING case matches UNSTABLE (checks failing)"
assert_grep 'false/\*' "$GUIDE_MD" \
    "the WARNING case matches mergeable=false (GitHub boolean conflict)"
# Step 1's list line itself must be unchanged — test-guide-docs-pr-race.sh
# extracts it verbatim (#5615/#7354); #13897 only adds the state read.
assert_grep 'OPEN_DOCS_PR=\$\("\$GH_READ" pr list --state open --limit 100 --json number,headRefName' "$GUIDE_MD" \
    "Step 1's open-docs-PR list check is unchanged (race-suite compatibility)"

# ---------------------------------------------------------------------------
# Test 2: docs_pinned_paths(), sourced verbatim and executed against config
# fixtures — default, missing config, array entries, env override, and a
# non-array value failing safe to empty.
# ---------------------------------------------------------------------------
echo ""
echo "Test 2: docs_pinned_paths(), executed"

DPP_SRC="$(awk '/^docs_pinned_paths\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ -z "$DPP_SRC" ]]; then
    echo -e "${RED}FATAL${NC}: could not extract docs_pinned_paths() from guide.md"
    exit 2
fi
eval "$DPP_SRC"

CFG_DIR="$(mktemp -d)"
trap 'rm -rf "$CFG_DIR" 2>/dev/null || true' EXIT

# No .loom/config.json at all (cwd has none): empty set.
OUT="$(cd "$CFG_DIR" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && docs_pinned_paths)"
assert_eq "$OUT" "" "(2) missing .loom/config.json yields the empty pinned set"

# Config without the key: default [].
mkdir -p "$CFG_DIR/repo1/.loom"
echo '{}' > "$CFG_DIR/repo1/.loom/config.json"
OUT="$(cd "$CFG_DIR/repo1" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && docs_pinned_paths)"
assert_eq "$OUT" "" "(2) absent guide.docsMaintenance.pinnedPaths key yields the empty pinned set"

# Single and multiple entries, space-joined.
echo '{"guide":{"docsMaintenance":{"pinnedPaths":["README.md"]}}}' > "$CFG_DIR/repo1/.loom/config.json"
OUT="$(cd "$CFG_DIR/repo1" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && docs_pinned_paths)"
assert_eq "$OUT" "README.md" "(2) a single pinned path is read from config"

echo '{"guide":{"docsMaintenance":{"pinnedPaths":["WORK_LOG.md","README.md"]}}}' > "$CFG_DIR/repo1/.loom/config.json"
OUT="$(cd "$CFG_DIR/repo1" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && docs_pinned_paths)"
assert_eq "$OUT" "WORK_LOG.md README.md" "(2) multiple pinned paths are space-joined in config order"

# Env override (the test seam) wins over config.
OUT="$(cd "$CFG_DIR/repo1" && LOOM_GUIDE_DOCS_PINNED_PATHS="README.md" docs_pinned_paths)"
assert_eq "$OUT" "README.md" "(2) LOOM_GUIDE_DOCS_PINNED_PATHS overrides config"

# A non-array pinnedPaths value (a bare string) must fail SAFE: jq errors,
# stderr is suppressed, and the set reads empty — nothing is wrongly excluded.
echo '{"guide":{"docsMaintenance":{"pinnedPaths":"README.md"}}}' > "$CFG_DIR/repo1/.loom/config.json"
OUT="$(cd "$CFG_DIR/repo1" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && docs_pinned_paths)"
assert_eq "$OUT" "" "(2) a non-array pinnedPaths value fails safe to the empty set"

# ---------------------------------------------------------------------------
# Test 3: stage_docs_paths(), sourced verbatim and executed inside a
# throwaway git repo — the actual #13897 regression, run rather than grepped.
# ---------------------------------------------------------------------------
echo ""
echo "Test 3: stage_docs_paths(), executed in a scratch repo"

SDP_SRC="$(awk '/^stage_docs_paths\(\) \{/{flag=1} flag{print} /^\}/{if(flag){exit}}' "$GUIDE_MD")"
if [[ -z "$SDP_SRC" ]]; then
    echo -e "${RED}FATAL${NC}: could not extract stage_docs_paths() from guide.md"
    exit 2
fi
# stage_docs_paths() calls docs_pinned_paths(), already defined by Test 2's
# eval of the same-extraction helper function.
eval "$SDP_SRC"

SCRATCH="$(mktemp -d)"
trap 'rm -rf "$CFG_DIR" "$SCRATCH" 2>/dev/null || true' EXIT
git -C "$SCRATCH" init -q
git -C "$SCRATCH" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
for f in WORK_LOG.md WORK_PLAN.md README.md; do
    echo "base $f" > "$SCRATCH/$f"
done
mkdir -p "$SCRATCH/work_log"
echo "archive body" > "$SCRATCH/work_log/WORK_LOG_2026-10.md"
git -C "$SCRATCH" add -A
git -C "$SCRATCH" -c user.email=t@t -c user.name=t commit -q -m "docs"
export DOCS_WT="$SCRATCH"

modify_all_three() {
    for f in WORK_LOG.md WORK_PLAN.md README.md; do
        echo "edited $f" > "$SCRATCH/$f"
    done
}
staged_list() {
    git -C "$SCRATCH" diff --cached --name-only | sort | paste -sd, -
}
reset_staging() {
    git -C "$SCRATCH" reset -q --mixed HEAD >/dev/null 2>&1
    modify_all_three
}

# 3a. Pinned path excluded from staging; the others staged; the skip is
# announced loudly, naming the path and the regeneration duty.
reset_staging
OUT="$(unset LOOM_GUIDE_DOCS_PINNED_PATHS; cd "$SCRATCH" && LOOM_GUIDE_DOCS_PINNED_PATHS="README.md" stage_docs_paths)"
assert_eq "$(staged_list)" "WORK_LOG.md,WORK_PLAN.md" "(3a) pinned README.md is NOT staged; WORK_LOG.md and WORK_PLAN.md are"
assert_contains "$OUT" "WARNING" "(3a) the skip is announced with a WARNING"
assert_contains "$OUT" "README.md is evidence-pinned" "(3a) the WARNING names the pinned path"
assert_contains "$OUT" "regeneration recipe" "(3a) the WARNING points at the repo's own regeneration recipe"
# ...and the pinned path's edit is still sitting unstaged in the worktree.
assert_eq "$(git -C "$SCRATCH" diff --name-only -- README.md)" "README.md" "(3a) README.md's edit remains unstaged in the worktree"

# 3b. A path with a space in the pinned set must not break staging of the
# three fixed names (none contains a space): fail safe, stage everything.
reset_staging
OUT="$(cd "$SCRATCH" && LOOM_GUIDE_DOCS_PINNED_PATHS="my docs.md" stage_docs_paths)"
assert_eq "$(staged_list)" "README.md,WORK_LOG.md,WORK_PLAN.md" "(3b) a pinned path containing a space excludes nothing (fail safe)"

# 3c. Empty pinned set: all three staged, no warning.
reset_staging
OUT="$(cd "$SCRATCH" && LOOM_GUIDE_DOCS_PINNED_PATHS="" stage_docs_paths)"
assert_eq "$(staged_list)" "README.md,WORK_LOG.md,WORK_PLAN.md" "(3c) an empty pinned set stages all three"
assert_not_contains "$OUT" "WARNING" "(3c) no WARNING when nothing is pinned"

# 3d. Pinned-only diff opens NO PR: everything the tick touched is pinned, so
# nothing is staged and create_docs_pr()'s `git diff --cached --quiet` check
# takes its existing "No document changes to commit." early return.
git -C "$SCRATCH" reset -q --mixed HEAD >/dev/null 2>&1
for f in WORK_LOG.md WORK_PLAN.md README.md; do echo "edited $f" > "$SCRATCH/$f"; done
OUT="$(cd "$SCRATCH" && LOOM_GUIDE_DOCS_PINNED_PATHS="WORK_LOG.md WORK_PLAN.md README.md" stage_docs_paths)"
assert_eq "$(staged_list)" "" "(3d) an all-pinned diff stages nothing"
if git -C "$SCRATCH" diff --cached --quiet; then
    pass "(3d) the pinned-only diff reaches create_docs_pr()'s 'No document changes to commit.' path (no PR)"
else
    fail "(3d) a pinned-only diff must leave the staged diff empty (no PR could be opened)"
fi
assert_contains "$OUT" "WARNING" "(3d) each skipped pinned path is still announced"

# 3e. README-only diff with README pinned — the exact #13897 live incident
# shape (a README-staleness edit on an evidence-pinned README): no PR.
git -C "$SCRATCH" reset -q --mixed HEAD >/dev/null 2>&1
modify_all_three
git -C "$SCRATCH" checkout -q -- WORK_LOG.md WORK_PLAN.md
echo "stale readme" > "$SCRATCH/README.md"
OUT="$(cd "$SCRATCH" && LOOM_GUIDE_DOCS_PINNED_PATHS="README.md" stage_docs_paths)"
assert_eq "$(staged_list)" "" "(3e) a README-only diff on a pinned README stages nothing (no PR)"
assert_contains "$OUT" "README.md is evidence-pinned" "(3e) the WARNING names the pinned README"

# 3f. Config-sourced pinning (not the env seam) reaches stage_docs_paths()
# through docs_pinned_paths(): same exclusion as 3a, read from
# .loom/config.json inside the scratch repo.
mkdir -p "$SCRATCH/.loom"
echo '{"guide":{"docsMaintenance":{"pinnedPaths":["README.md"]}}}' > "$SCRATCH/.loom/config.json"
git -C "$SCRATCH" reset -q --mixed HEAD >/dev/null 2>&1
modify_all_three
OUT="$(cd "$SCRATCH" && unset LOOM_GUIDE_DOCS_PINNED_PATHS && stage_docs_paths)"
assert_eq "$(staged_list)" "WORK_LOG.md,WORK_PLAN.md" "(3f) pinnedPaths read from .loom/config.json excludes README.md from staging"

# ---------------------------------------------------------------------------
# Test 4: Step 1's skip block, extracted verbatim and executed against a stub
# `gh` (mergeability fixtures) and a stub docs-guide-lock.sh.
# ---------------------------------------------------------------------------
echo ""
echo "Test 4: Step 1's mergeability-aware skip, executed"

# Extract the whole `if [ -n "$OPEN_DOCS_PR" ]; then ... fi` block: from the
# first line matching the open-PR guard through the next line that is exactly
# `fi` at column 0. The block contains no nested if/fi, so the first ^fi ends
# it.
STEP1_BLOCK="$(awk '/^if \[ -n "\$OPEN_DOCS_PR" \]; then$/{flag=1} flag{print} flag && /^fi$/{exit}' "$GUIDE_MD")"
if [[ -z "$STEP1_BLOCK" || "$STEP1_BLOCK" != *fi ]]; then
    echo -e "${RED}FATAL${NC}: could not extract Step 1's skip block from guide.md"
    exit 2
fi
# Sanity: the extracted block still skips unconditionally (release + return).
assert_contains "$STEP1_BLOCK" "docs-guide-lock.sh release" "(4) the skip block still releases the docs-guide lock"
assert_contains "$STEP1_BLOCK" "  return" "(4) the skip block still returns (skips the phase) in every branch"

STUB_DIR="$(mktemp -d)"
trap 'rm -rf "$CFG_DIR" "$SCRATCH" "$STUB_DIR" 2>/dev/null || true' EXIT

# Stub gh: answers exactly the `pr view <n> --json mergeable,mergeStateStatus
# --jq FILTER` shape Step 1 issues, applying the REAL jq filter from the
# prompt's command line to $LOOM_TEST_PR_VIEW_JSON. LOOM_TEST_PR_VIEW_FAIL=1
# models a failed read (gh erroring).
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$1" == "pr" && "$2" == "view" ]]; then
  if [[ "${LOOM_TEST_PR_VIEW_FAIL:-0}" == "1" ]]; then exit 3; fi
  shift 2
  filter=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --jq) filter="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  jq -r "$filter" <<<"${LOOM_TEST_PR_VIEW_JSON:?stub gh: LOOM_TEST_PR_VIEW_JSON not set}"
  exit 0
fi
echo "stub gh: unhandled args: $*" >&2
exit 3
STUB
chmod +x "$STUB_DIR/gh"

# Stub docs-guide-lock.sh: the block calls ./.loom/scripts/docs-guide-lock.sh
# relative to cwd, so the scratch cwd carries a stub that records releases.
RUN_DIR="$STUB_DIR/run"
mkdir -p "$RUN_DIR/.loom/scripts"
cat > "$RUN_DIR/.loom/scripts/docs-guide-lock.sh" <<'STUB'
#!/usr/bin/env bash
echo "release:$PWD" >> "${LOOM_TEST_LOCK_LOG:?stub lock: LOOM_TEST_LOCK_LOG not set}"
STUB
chmod +x "$RUN_DIR/.loom/scripts/docs-guide-lock.sh"

export LOOM_TEST_LOCK_LOG="$STUB_DIR/lock.log"
export PATH="$STUB_DIR:$PATH"
export GH_READ="gh"

# run_step1 <state-fixture-json|-> : eval the extracted block verbatim inside
# a function (it `return`s), with OPEN_DOCS_PR preset; captures the block's
# output and proves the phase was skipped (the post-eval line is unreachable
# while the return fires).
run_step1() {
    # Both "unused" locals are consumed by the eval'd Step 1 block below.
    # shellcheck disable=SC2034
    local OPEN_DOCS_PR="123" OPEN_DOCS_PR_STATE=""
    (
        cd "$RUN_DIR" || exit 1
        eval "$STEP1_BLOCK"
        echo "FELL_THROUGH_TO_PR_CREATION"
    )
}

: > "$LOOM_TEST_LOCK_LOG"

# 4a. GitHub conflict: mergeable=false, mergeStateStatus=DIRTY → WARNING
# naming the PR and the state, and still a skip.
export LOOM_TEST_PR_VIEW_JSON='{"mergeable": false, "mergeStateStatus": "DIRTY"}'
unset LOOM_TEST_PR_VIEW_FAIL
OUT="$(run_step1)"
assert_contains "$OUT" "WARNING" "(4a) mergeable=false/DIRTY emits the loud WARNING"
assert_contains "$OUT" "#123" "(4a) the WARNING names the PR number"
assert_contains "$OUT" "false/DIRTY" "(4a) the WARNING names the merge state"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4a) the skip still happens (no path to a second PR)"
assert_eq "$(wc -l < "$LOOM_TEST_LOCK_LOG" | tr -d '[:space:]')" "1" "(4a) the lock was released exactly once"

# 4b. Gitea conflict: mergeable="CONFLICTING" (enum), no mergeStateStatus.
export LOOM_TEST_PR_VIEW_JSON='{"mergeable": "CONFLICTING", "mergeStateStatus": null}'
OUT="$(run_step1)"
assert_contains "$OUT" "WARNING" "(4b) Gitea mergeable=CONFLICTING emits the WARNING"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4b) the skip still happens"

# 4c. Checks failing: mergeable=true, mergeStateStatus=UNSTABLE → WARNING.
export LOOM_TEST_PR_VIEW_JSON='{"mergeable": true, "mergeStateStatus": "UNSTABLE"}'
OUT="$(run_step1)"
assert_contains "$OUT" "WARNING" "(4c) UNSTABLE (checks failing) emits the WARNING"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4c) the skip still happens"

# 4d. BLOCKED but mergeable (the normal awaiting-Judge-review state): plain
# one-line skip, NO warning.
export LOOM_TEST_PR_VIEW_JSON='{"mergeable": true, "mergeStateStatus": "BLOCKED"}'
OUT="$(run_step1)"
assert_contains "$OUT" "still open. Skipping document maintenance" "(4d) a mergeable BLOCKED PR gets the plain skip"
assert_not_contains "$OUT" "WARNING" "(4d) no WARNING for the normal review-gated state"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4d) the skip still happens"

# 4e. UNKNOWN (GitHub computing mergeability asynchronously: null): plain
# skip, no warning, never a second PR.
export LOOM_TEST_PR_VIEW_JSON='{"mergeable": null, "mergeStateStatus": "CLEAN"}'
OUT="$(run_step1)"
assert_contains "$OUT" "still open. Skipping document maintenance" "(4e) an UNKNOWN reading yields the plain skip"
assert_not_contains "$OUT" "WARNING" "(4e) no WARNING on UNKNOWN data"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4e) never 'go ahead and open a second PR'"

# 4f. Failed read (gh errors, state unreadable): plain skip, no warning.
export LOOM_TEST_PR_VIEW_JSON='{}'
export LOOM_TEST_PR_VIEW_FAIL=1
OUT="$(run_step1)"
assert_contains "$OUT" "still open. Skipping document maintenance" "(4f) a failed state read yields the plain skip"
assert_not_contains "$OUT" "WARNING" "(4f) no WARNING on a failed read"
assert_not_contains "$OUT" "FELL_THROUGH_TO_PR_CREATION" "(4f) the skip still happens — no second PR"
unset LOOM_TEST_PR_VIEW_FAIL

# ---------------------------------------------------------------------------
echo ""
if [[ "$TESTS_FAILED" -eq 0 ]]; then
    echo -e "${GREEN}All $TESTS_RUN tests passed${NC}"
    exit 0
else
    echo -e "${RED}$TESTS_FAILED of $TESTS_RUN tests failed${NC}"
    exit 1
fi
