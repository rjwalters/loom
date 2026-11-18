#!/usr/bin/env bash
# test-guide-work-plan-roadmap-convention.sh - Regression test for issue #14009
#
# Guide's WORK_PLAN.md is a generated *backlog snapshot*; a repo's hand-written
# ROADMAP.md is the *direction* document. Before #14009, guide.md called
# WORK_PLAN.md a "Prioritized roadmap", had no first-creation rule (two Guide
# passes on 2AMLogic/pdks, #78 and #93, reached opposite conclusions — one
# deferred as an "operator scope decision", the next created both files), no
# recorded opt-out location, and no README-layout refresh on first creation.
#
# This suite pins the written convention (grep-level, mirroring how
# test-guide-operator-hold-exclude.sh pins role text) on EVERY shipped copy of
# the Guide role prompt, so the source and the generated skill copy cannot
# drift apart:
#   1. the Document Maintenance table row calls WORK_PLAN.md a "Backlog
#      snapshot", never a bare "Prioritized roadmap";
#   2. the "WORK_PLAN.md vs a hand-written ROADMAP.md" subsection exists and
#      states the create-by-default rule (never defer) and the opt-out
#      location (guide.docsMaintenance);
#   3. Step 3's first-creation preamble links the repo's roadmap;
#   4. Step 4 carries the first-creation README-layout staleness trigger;
#   5. negative control: no "Prioritized roadmap" wording survives anywhere
#      in either copy (that wording is the ambiguity the issue was filed on);
#   6. the bootstrap template (imagine.md) and this repo's own WORK_PLAN.md
#      preamble carry the snapshot-vs-direction distinction too;
#   7. defaults/docs/work-docs-archive.md carries a pointer to the convention.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }
grep_has() { # file pattern description
    if grep -q -- "$2" "$1" 2>/dev/null; then pass "$3"; else fail "$3 (pattern not found in $1)"; fi
}
grep_lacks() { # file pattern description
    if grep -q -- "$2" "$1" 2>/dev/null; then fail "$3 (forbidden pattern found in $1)"; else pass "$3"; fi
}

for copy in "defaults/.claude/commands/loom/guide.md" "defaults/.agents/skills/loom-guide/SKILL.md"; do
    f="$REPO_ROOT/$copy"; echo "Copy: $copy"
    [[ -f "$f" ]] || { fail "$copy exists"; continue; }

    # 1. Table row names the snapshot role.
    grep_has "$f" '| \*\*WORK_PLAN.md\*\* | Backlog snapshot from current GitHub label state |' \
        "$copy: doc table row calls WORK_PLAN.md a backlog snapshot"

    # 2. The convention subsection exists and answers Q2 (create-by-default,
    #    never defer) and Q3 (opt-out location).
    grep_has "$f" 'WORK_PLAN.md vs a hand-written ROADMAP.md' \
        "$copy: convention subsection heading present"
    grep_has "$f" 'creates them without asking' \
        "$copy: create-by-default rule stated"
    grep_has "$f" 'Never defer first creation' \
        "$copy: never-defer rule stated"
    grep_has "$f" 'guide\.docsMaintenance' \
        "$copy: opt-out location (guide.docsMaintenance) named"

    # 3. Step 3 first-creation preamble links the repo roadmap.
    grep_has "$f" 'First creation (#14009)' \
        "$copy: Step 3 first-creation block present"
    grep_has "$f" 'Direction lives in the hand-written roadmap' \
        "$copy: first-creation preamble distinguishes direction document"

    # 4. Step 4 first-creation README-layout trigger.
    grep_has "$f" 'FIRST-CREATION TRIGGER' \
        "$copy: Step 4 first-creation staleness trigger present"

    # 5. Negative control: the ambiguous wording is gone.
    grep_lacks "$f" 'Prioritized roadmap' \
        "$copy: no bare 'Prioritized roadmap' wording survives"
done

# 6. Bootstrap template and this repo's live preamble.
echo "Copy: defaults/.claude/commands/loom/imagine.md"
grep_has "$REPO_ROOT/defaults/.claude/commands/loom/imagine.md" \
    'Backlog snapshot from current GitHub label state, maintained by the Guide role' \
    "imagine.md: WORK_PLAN template preamble uses the snapshot wording"
grep_lacks "$REPO_ROOT/defaults/.claude/commands/loom/imagine.md" \
    'Prioritized roadmap' \
    "imagine.md: no bare 'Prioritized roadmap' wording survives"

echo "Copy: WORK_PLAN.md (this repo's own generated file)"
grep_has "$REPO_ROOT/WORK_PLAN.md" \
    'Backlog snapshot from current GitHub label state, maintained by the Guide role' \
    "WORK_PLAN.md: live preamble uses the snapshot wording"
grep_has "$REPO_ROOT/WORK_PLAN.md" \
    'docs/ROADMAP\.md' \
    "WORK_PLAN.md: live preamble links the repo roadmap (one exists here)"
grep_lacks "$REPO_ROOT/WORK_PLAN.md" \
    'Prioritized roadmap' \
    "WORK_PLAN.md: no bare 'Prioritized roadmap' wording survives"

# 7. work-docs tooling doc carries the pointer, not a second copy.
echo "Copy: defaults/docs/work-docs-archive.md"
grep_has "$REPO_ROOT/defaults/docs/work-docs-archive.md" \
    'WORK_PLAN.md vs a hand-written ROADMAP.md' \
    "work-docs-archive.md: pointer to the convention present"

echo "Tests run: $TESTS_RUN"
[[ $TESTS_FAILED -eq 0 ]] && echo "All tests passed" || { echo "Tests failed: $TESTS_FAILED"; exit 1; }
