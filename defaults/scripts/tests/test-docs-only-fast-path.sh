#!/usr/bin/env bash
# test-docs-only-fast-path.sh - Regression tests for the docs-only fast path
# (#6134): Judge/Champion's mechanical eligibility check that lets a PR whose
# diff is confined to WORK_LOG.md/WORK_PLAN.md/README.md skip the full code
# evaluation + merge-risk judgment cycle, while still landing safely.
#
# Judge's "Docs-Only Fast Path" section and Champion's merge-risk criterion "Docs-only
# fast path" subsection are prose an LLM instance reads and executes, not a
# standalone script (same situation as test-champion-critical-file-check.sh)
# — so this file mirrors the documented `docs_only_fast_path_check()`
# function in a local copy and pins the shipped markdown's exact commands
# with `assert_doc_contains`, catching drift between the two.
#
# What this asserts:
#   1. The mirrored eligibility check correctly returns ELIGIBLE for diffs
#      confined to any non-empty subset of {WORK_LOG.md, WORK_PLAN.md,
#      README.md}.
#   2. It correctly rejects ANY diff that includes a file outside that exact
#      allowlist — including a nested README.md/WORK_LOG.md (path match, not
#      substring), an empty diff, and the two edge cases the issue's Test
#      Plan calls out explicitly: `.github/workflows/*.yml` and
#      `.loom/config.json` alongside otherwise-docs-only changes.
#   3. Judge's and Champion's shipped markdown both define an identical
#      allowlist and both independently re-derive the file list via the
#      paginated files API (never `gh pr view --json files`, and never by
#      trusting a marker/label).
#   4. Guide's create_docs_pr() emits the informational marker but the
#      shipped Judge/Champion markdown never gates eligibility on it.
#
# Usage:
#   ./.loom/scripts/tests/test-docs-only-fast-path.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"

# Two `..` reaches repo-root/.claude/commands/loom for an INSTALLED copy
# (SCRIPTS_DIR is .loom/scripts there); one `..` reaches defaults/.claude/
# commands/loom when running inside this source repo (SCRIPTS_DIR is
# defaults/scripts) -- the two layouts differ in depth, so probe both rather
# than hard-coding one (#6725).
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../../.claude/commands/loom" && pwd)"
else
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../.claude/commands/loom" && pwd)"
fi
JUDGE_MD="$PROMPT_DIR/judge.md"
CHAMPION_MD="$PROMPT_DIR/champion-pr-merge.md"
GUIDE_MD="$PROMPT_DIR/guide.md"

# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Pin a literal snippet as present verbatim in a doc file — catches drift
# between this test's mirrored function and the shipped markdown.

# Pin a literal snippet's ABSENCE from a doc file — catches a regression back
# to a truncating/trust-the-marker shortcut.

# =====================================================================
# The docs-only fast-path eligibility check, mirrored verbatim from BOTH
# judge.md's "Docs-Only Fast Path" section and champion-pr-merge.md's
# "Docs-only fast path" subsection of the merge-risk criterion — the two shipped copies
# are intentionally identical, and the doc pins below confirm both actually
# match this mirror.
# =====================================================================
DOCS_FAST_PATH_ALLOWLIST=("WORK_LOG.md" "WORK_PLAN.md" "README.md")
# Archive files (#12636): ONLY the month-rotated archives, month 01-12, one
# directory deep. Never a directory or *.md allowance.
DOCS_FAST_PATH_ARCHIVE_RE='^(work_log/WORK_LOG|work_plan/WORK_PLAN)_[0-9]{4}-(0[1-9]|1[0-2])\.md$'

docs_only_fast_path_check() {
    # Reads a newline-separated file list on stdin. Echoes "ELIGIBLE" if
    # every file is an exact match against DOCS_FAST_PATH_ALLOWLIST, or
    # "NOT ELIGIBLE: <file>" for the first file that is not.
    local file matched
    local saw_any=0
    while IFS= read -r file; do
        [ -z "$file" ] && continue
        saw_any=1
        matched=0
        for allowed in "${DOCS_FAST_PATH_ALLOWLIST[@]}"; do
            if [ "$file" = "$allowed" ]; then
                matched=1
                break
            fi
        done
        if [ "$matched" -ne 1 ] && [[ "$file" =~ $DOCS_FAST_PATH_ARCHIVE_RE ]]; then
            matched=1
        fi
        if [ "$matched" -ne 1 ]; then
            echo "NOT ELIGIBLE: $file"
            return 0
        fi
    done
    if [ "$saw_any" -eq 0 ]; then
        echo "NOT ELIGIBLE: empty file list"
        return 0
    fi
    echo "ELIGIBLE"
}

echo "--- docs_only_fast_path_check: eligible diffs ---"

out="$(printf '%s\n' "WORK_LOG.md" | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "a single-file WORK_LOG.md-only diff is eligible"

out="$(printf 'WORK_LOG.md\nWORK_PLAN.md\n' | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "WORK_LOG.md + WORK_PLAN.md is eligible"

out="$(printf 'WORK_LOG.md\nWORK_PLAN.md\nREADME.md\n' | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "all three allowlisted files together are eligible"

out="$(printf 'README.md\n' | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "a single-file README.md-only diff is eligible"

echo
echo "--- docs_only_fast_path_check: rejects any file outside the exact allowlist ---"

out="$(printf 'WORK_LOG.md\nsrc/lib.rs\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: src/lib.rs" "$out" \
    "a docs file plus a source file is rejected (mixed diff)"

out="$(printf 'WORK_LOG.md\ndocs/README.md\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: docs/README.md" "$out" \
    "a nested docs/README.md is rejected (path match, not substring/basename match)"

out="$(printf 'WORK_LOG.md\nmcp-loom/README.md\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: mcp-loom/README.md" "$out" \
    "a nested mcp-loom/README.md is rejected"

out="$(printf 'notes/WORK_LOG.md\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: notes/WORK_LOG.md" "$out" \
    "a non-root-level WORK_LOG.md is rejected"

out="$(printf '' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: empty file list" "$out" \
    "an empty diff (no files at all) is rejected, not silently treated as eligible"

echo
echo "--- docs_only_fast_path_check: issue #6134 Test Plan edge cases ---"

# "a docs-only PR that also modifies .github/workflows/*.yml ... must NOT
# qualify for the fast path even though those aren't 'code' in the
# traditional sense".
out="$(printf 'WORK_LOG.md\nWORK_PLAN.md\nREADME.md\n.github/workflows/ci.yml\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: .github/workflows/ci.yml" "$out" \
    "all three docs files plus .github/workflows/ci.yml is rejected"

# "... or .loom/config.json must NOT qualify for the fast path".
out="$(printf 'WORK_LOG.md\n.loom/config.json\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: .loom/config.json" "$out" \
    "WORK_LOG.md plus .loom/config.json is rejected"

# The order in which the disqualifying file appears must not matter — the
# loop must not stop at the first ALLOWED file and declare victory early.
out="$(printf '.loom/config.json\nWORK_LOG.md\nWORK_PLAN.md\nREADME.md\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: .loom/config.json" "$out" \
    ".loom/config.json first in the list is still caught (not masked by later allowed files)"

echo
echo "--- docs_only_fast_path_check: smuggling can't be won by a mislabeled/marked PR ---"

# A file list carrying something that merely *looks* docs-adjacent by name
# (e.g. a file whose name contains WORK_LOG as a substring but isn't the
# exact root file) must still be rejected — the check is exact-match, not
# substring/glob.
out="$(printf 'WORK_LOG.md.bak\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: WORK_LOG.md.bak" "$out" \
    "a file merely containing the allowlisted name as a substring is rejected"

echo
echo "--- docs_only_fast_path_check: month-rotated archive files (#12636) ---"

out="$(printf 'WORK_LOG.md\nwork_log/WORK_LOG_2026-08.md\n' | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "WORK_LOG.md plus a correctly named WORK_LOG archive is eligible"

out="$(printf 'WORK_PLAN.md\nwork_plan/WORK_PLAN_2026-12.md\n' | docs_only_fast_path_check)"
assert_eq "ELIGIBLE" "$out" "WORK_PLAN.md plus a correctly named WORK_PLAN archive is eligible"

for bad in 'work_log/notes.md' 'docs/work_log/WORK_LOG_2026-08.md' \
    'work_log/WORK_LOG_2026-13.md' 'work_log/WORK_LOG_2026-00.md' \
    'work_log/WORK_LOG_2026-8.md' 'work_log/WORK_LOG_2026-08.md.bak' \
    'work_log/sub/WORK_LOG_2026-08.md' 'work_plan/WORK_LOG_2026-08.md' \
    'work_log/WORK_PLAN_2026-08.md' 'work_log.md' 'work_log/'; do
    out="$(printf 'WORK_LOG.md\n%s\n' "$bad" | docs_only_fast_path_check)"
    assert_eq "NOT ELIGIBLE: $bad" "$out" "off-pattern path '$bad' stays ineligible"
done

out="$(printf 'work_log/WORK_LOG_2026-08.md\nsrc/main.rs\n' | docs_only_fast_path_check)"
assert_eq "NOT ELIGIBLE: src/main.rs" "$out" "an archive file mixed with a non-docs file is ineligible"

echo
echo "--- closing-reference condition (#14182): a docs-only diff that closes an issue is ineligible ---"

# Mirror of the shipped docs_only_fast_path_closing_check(), extracted from
# judge.md so this suite exercises the documented code, not a hand copy. `gh`
# is stubbed: GH_STUB_OUT is its stdout, GH_STUB_RC its exit status.
gh() { printf '%s' "${GH_STUB_OUT-}"; return "${GH_STUB_RC:-0}"; }
closing_fn_block() {
    awk '/^docs_only_fast_path_closing_check\(\) \{/{p=1} p{print} p && /^}$/{exit}' "$1"
}
eval "$(closing_fn_block "$JUDGE_MD")"

GH_STUB_RC=0 GH_STUB_OUT='closers=' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "ELIGIBLE" "$out" "a PR with an empty closing set stays eligible"

GH_STUB_RC=0 GH_STUB_OUT='closers=42' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "NOT ELIGIBLE: closes #42" "$out" "a PR closing one issue is ineligible"

GH_STUB_RC=0 GH_STUB_OUT='closers=42,77' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "NOT ELIGIBLE: closes #42, #77" "$out" "a PR closing several issues is ineligible"

GH_STUB_RC=1 GH_STUB_OUT='' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "NOT ELIGIBLE: closing-reference read failed" "$out" \
    "a failed read fails closed (not treated as 'no closers')"

GH_STUB_RC=1 GH_STUB_OUT='closers=' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "NOT ELIGIBLE: closing-reference read failed" "$out" \
    "a non-zero exit fails closed even when stdout looks like an empty set"

GH_STUB_RC=0 GH_STUB_OUT='' out="$(docs_only_fast_path_closing_check 1)"
assert_eq "NOT ELIGIBLE: closing-reference read returned no proof" "$out" \
    "an empty-because-errored read (exit 0, no 'closers=' proof) fails closed"

unset -f gh

JUDGE_CLOSING="$(closing_fn_block "$JUDGE_MD" | grep -v '^[[:space:]]*#')"
CHAMPION_CLOSING="$(closing_fn_block "$CHAMPION_MD" | grep -v '^[[:space:]]*#')"
assert_contains "$JUDGE_CLOSING" "docs_only_fast_path_closing_check() {" "judge.md ships docs_only_fast_path_closing_check()"
assert_eq "$JUDGE_CLOSING" "$CHAMPION_CLOSING" \
    "judge.md and champion-pr-merge.md closing-check code is identical (comments aside)"

assert_doc_contains "$JUDGE_MD" \
    'ELIGIBILITY=$(docs_only_fast_path_closing_check "$PR_NUMBER")' \
    "judge.md folds the closing check into ELIGIBILITY"
assert_doc_contains "$CHAMPION_MD" \
    'FAST_PATH_RESULT=$(docs_only_fast_path_closing_check "$PR_NUMBER")' \
    "champion-pr-merge.md folds the closing check into FAST_PATH_RESULT"
assert_doc_contains "$JUDGE_MD" "closingIssuesReferences" "judge.md reads closingIssuesReferences"
assert_doc_contains "$CHAMPION_MD" "closingIssuesReferences" "champion-pr-merge.md reads closingIssuesReferences"

# Inline PROSE restatements of the eligibility condition (#14321). The LLM
# executing these prompts reads the lead-in / dispatch step before the code
# block, so prose narrower than the code block's gate (file list only) would
# grant a fast path the code refuses. Pin the closing-issue clause so adding a
# condition to the code block without updating its prose turns this red.
assert_doc_contains "$CHAMPION_MD" \
    "and the PR has no closing-issue" \
    "champion-pr-merge.md fast-path lead-in states the no-closing-issue condition (#14182)"
assert_doc_contains "$JUDGE_MD" \
    "and the PR has no closing-issue reference" \
    "judge.md Primary Queue step 4b states the no-closing-issue condition (#14182)"

echo
echo "--- Judge and Champion allowlist + archive pattern are byte-identical (#12636) ---"

fast_path_block() {
    # Allowlist, archive regex and function body, up to the closing brace.
    awk '/^DOCS_FAST_PATH_ALLOWLIST=/{p=1} p{print} p && /^}$/{exit}' "$1" \
        | grep -v '^[[:space:]]*#'
}
JUDGE_BLOCK="$(fast_path_block "$JUDGE_MD")"
CHAMPION_BLOCK="$(fast_path_block "$CHAMPION_MD")"
assert_eq "$JUDGE_BLOCK" "$CHAMPION_BLOCK" \
    "judge.md and champion-pr-merge.md allowlist+check code is identical (comments aside)"

ARCHIVE_RE_LINE="DOCS_FAST_PATH_ARCHIVE_RE='^(work_log/WORK_LOG|work_plan/WORK_PLAN)_[0-9]{4}-(0[1-9]|1[0-2])\\.md\$'"
assert_doc_contains "$JUDGE_MD" "$ARCHIVE_RE_LINE" "judge.md pins the narrow archive regex"
assert_doc_contains "$CHAMPION_MD" "$ARCHIVE_RE_LINE" "champion-pr-merge.md pins the narrow archive regex"
assert_doc_contains "$0" "$ARCHIVE_RE_LINE" "this suite's mirror uses the same archive regex"

echo
echo "--- Doc pins: judge.md ships the mechanical, marker-independent eligibility check ---"

assert_doc_contains "$JUDGE_MD" \
    'DOCS_FAST_PATH_ALLOWLIST=("WORK_LOG.md" "WORK_PLAN.md" "README.md")' \
    "judge.md defines the exact 3-file allowlist"

assert_doc_contains "$JUDGE_MD" \
    'FILES=$(gh api "repos/{owner}/{repo}/pulls/$PR_NUMBER/files" --paginate --jq '"'"'.[].filename'"'"')' \
    "judge.md fetches the changed-file list via the paginated REST endpoint (not the truncating gh pr view --json files)"

assert_doc_contains "$JUDGE_MD" \
    'docs_only_fast_path_check() {' \
    "judge.md defines docs_only_fast_path_check()"

assert_doc_contains "$JUDGE_MD" \
    "loom:docs-fast-path-evaluation" \
    "judge.md's fast-path approval carries the loom:docs-fast-path-evaluation marker"

assert_doc_contains "$JUDGE_MD" \
    "#6134" \
    "judge.md documents the #6134 fast path"

echo
echo "--- Doc pins: champion-pr-merge.md independently re-derives eligibility, never trusts Judge's marker ---"

assert_doc_contains "$CHAMPION_MD" \
    'DOCS_FAST_PATH_ALLOWLIST=("WORK_LOG.md" "WORK_PLAN.md" "README.md")' \
    "champion-pr-merge.md defines the same exact 3-file allowlist"

assert_doc_contains "$CHAMPION_MD" \
    'docs_only_fast_path_check() {' \
    "champion-pr-merge.md defines its own docs_only_fast_path_check()"

assert_doc_contains "$CHAMPION_MD" \
    'FAST_PATH_FILES=$(gh api "repos/{owner}/{repo}/pulls/$PR_NUMBER/files" --paginate --jq '"'"'.[].filename'"'"')' \
    "champion-pr-merge.md fetches the changed-file list via the paginated REST endpoint for its own fast-path check"

assert_doc_contains "$CHAMPION_MD" \
    "Never trust a marker, label, or the PR body's stated intent" \
    "champion-pr-merge.md explicitly states it never trusts Judge's marker/label for the fast path"

assert_doc_contains "$CHAMPION_MD" \
    "#6134" \
    "champion-pr-merge.md documents the #6134 fast path"

echo
echo "--- Doc pins: neither Judge nor Champion gates eligibility on a label/marker alone ---"

# Regression guard for AC2 ("verify the diff's file list server-side... not
# just trust the PR's stated intent"): the fast path must never be phrased
# as "if the marker/label is present, skip verification".
assert_doc_lacks "$JUDGE_MD" \
    'if grep -q "loom:docs-only-fast-path"' \
    "judge.md never short-circuits eligibility on a grep for the marker alone"

assert_doc_lacks "$CHAMPION_MD" \
    'if grep -q "loom:docs-only-fast-path"' \
    "champion-pr-merge.md never short-circuits eligibility on a grep for the marker alone"

echo
echo "--- Doc pins: guide.md's create_docs_pr() marker is informational only ---"

assert_doc_contains "$GUIDE_MD" \
    "<!-- loom:docs-only-fast-path -->" \
    "guide.md's create_docs_pr() emits the informational fast-path marker"

assert_doc_contains "$GUIDE_MD" \
    "Judge and Champion never trust it" \
    "guide.md documents that the marker is informational, not authoritative"

# Regression guard for #14176: a closing keyword sitting directly before an
# issue reference in the docs-PR body (`drops closed #286`) makes GitHub
# parse it as a closing link, so merging a docs-only PR would close an
# unrelated issue. The heredoc must carry the keyword-safety note, the
# post-create backstop must read closingIssuesReferences and close the PR
# when it is non-empty, and the heredoc itself must stay free of
# closing-keyword adjacency — the note included, since it is posted
# verbatim (its examples use "<keyword> #N" placeholders instead).
echo
echo "--- Doc pins: docs-PR body keyword safety (#14176) ---"

# Mirror of the PROMPT_DIR probe above (#6725): installed layout keeps
# .agents/skills at the repo root, source-tree layout under defaults/.
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    GUIDE_SKILL_MD="$SCRIPTS_DIR/../../.agents/skills/loom-guide/SKILL.md"
else
    GUIDE_SKILL_MD="$SCRIPTS_DIR/../.agents/skills/loom-guide/SKILL.md"
fi

heredoc_free_of_closing_keyword() { # file description
    local region
    region="$(sed -n "/cat <<'PRBODY'/,/^PRBODY$/p" "$1")"
    if grep -Eq '(close[sd]?|fix(es|ed)?|resolve[sd]?) #[0-9]' <<<"$region"; then
        fail "$2 (found closing-keyword adjacency in $1's PRBODY heredoc)"
    else
        pass "$2"
    fi
}

for f in "$GUIDE_MD" "$GUIDE_SKILL_MD"; do
    assert_doc_contains "$f" \
        "Keyword safety (#14176)" \
        "$(basename "$f") PRBODY heredoc carries the closing-keyword-safety note"
    assert_doc_contains "$f" \
        "closingIssuesReferences" \
        "$(basename "$f") post-create backstop reads closingIssuesReferences"
    heredoc_free_of_closing_keyword "$f" \
        "$(basename "$f") PRBODY heredoc is free of closing-keyword adjacency"
done

# Both shipped mirrors must stay byte-identical across the hunks #14176
# touches: the PRBODY heredoc (with the note) through the post-create
# backstop and the lock release that must still run afterward.
extract_docs_pr_region() {
    awk '/cat <<'"'"'PRBODY'"'"'/{inregion=1}
         inregion {print}
         inregion && /docs-guide-lock\.sh release/{exit}' "$1"
}
assert_eq "$(extract_docs_pr_region "$GUIDE_MD")" \
          "$(extract_docs_pr_region "$GUIDE_SKILL_MD")" \
          "guide.md and SKILL.md docs-PR hunks stay byte-identical"

echo
echo "--- WORK_LOG record gate (#13760): new records verify against the base repository ---"

# Extract the shipped function from judge.md so this suite exercises the
# documented code, not a hand copy (same extraction the closing check above
# uses). No column-0 `}` may appear inside the function before its end.
worklog_fn_block() {
    awk '/^docs_only_fast_path_worklog_check\(\) \{/{p=1} p{print} p && /^\}$/{exit}' "$1"
}
WL_FN="$(worklog_fn_block "$JUDGE_MD")"
[[ -n "$WL_FN" ]] || { echo "FATAL: docs_only_fast_path_worklog_check() not found in judge.md"; exit 1; }
assert_contains "$(printf '%s\n' "$WL_FN" | grep -v '^[[:space:]]*#')" "docs_only_fast_path_worklog_check() {" \
    "judge.md ships docs_only_fast_path_worklog_check()"

# Stub `gh`: the PR's base repository is owner/mine (a second, foreign repo
# owner/other exists but the gate must NEVER query it); the diff comes from
# WL_DIFF; the issues/N reads are recorded to WL_CALL_LOG so the tests assert
# the target actually queried. FAIL_LOOKUP=1 simulates a temporary forge
# failure on the record reads.
WL_CALL_LOG="$(mktemp)"
gh() {
    case "$1" in
        pr)
            case "$2" in
                view) printf '%s' "${WL_BASE_OUT-owner/mine}"; return "${WL_BASE_RC:-0}" ;;
                diff) printf '%s' "${WL_DIFF:-}"; return "${WL_DIFF_RC:-0}" ;;
            esac ;;
        api)
            printf '%s\n' "$2" >> "$WL_CALL_LOG"
            [[ "${FAIL_LOOKUP:-}" == 1 ]] && return 1
            case "$2" in
                repos/owner/mine/issues/5) printf 'Add widget\ttrue'; return 0 ;;
                repos/owner/mine/issues/7) printf 'Fix gizmo\tfalse'; return 0 ;;
                *) return 1 ;;
            esac ;;
    esac
    return 1
}
eval "$WL_FN"

# A WORK_LOG-only diff adding one merged PR and one closed issue of this repo.
GOOD_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR #5**: Add widget
+- **Issue #7** (closed): Fix gizmo'

# Pure archive rotation: every added record is matched by an identical removal
# in the same diff, so nothing is new and nothing needs verifying.
ROTATION_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,4 +1,2 @@
-- **PR #5**: Add widget
-- **Issue #7** (closed): Fix gizmo
diff --git a/work_log/WORK_LOG_2026-10.md b/work_log/WORK_LOG_2026-10.md
--- /dev/null
+++ b/work_log/WORK_LOG_2026-10.md
@@ -0,0 +1,2 @@
+- **PR #5**: Add widget
+- **Issue #7** (closed): Fix gizmo'

# Rotation plus one genuinely NEW record (added, never removed).
ROTATION_PLUS_NEW_DIFF="$ROTATION_DIFF
diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -2,1 +4,2 @@
+- **PR #9**: Not in this repo"

# Full-literal fixtures (never derived via ${var/pat/rep} — that pattern is a
# GLOB and matches across newlines, silently mangling multi-line diffs; and
# assignment-only statements like `VAR=x out="$(fn)"` persist the assignment,
# so every invocation below puts its env INSIDE the command substitution).
ABSENT_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR #407**: sim: probe the two Row 4b band-edge kickback rejects (supplement)
+- **Issue #7** (closed): Fix gizmo'

TITLE_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR #5**: Totally different title
+- **Issue #7** (closed): Fix gizmo'

ISSUE_AS_PR_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **Issue #5** (closed): Add widget
+- **Issue #7** (closed): Fix gizmo'

PR_AS_ISSUE_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR #5**: Add widget
+- **PR #7**: Fix gizmo'

FOREIGN_REPO_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR owner/other#5**: Add widget
+- **Issue #7** (closed): Fix gizmo'

MIXED_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,5 @@
+### 2026-11-19
+
+- **PR #5**: Add widget
+- **PR #99**: Not there'

UNPARSEABLE_DIFF='diff --git a/WORK_LOG.md b/WORK_LOG.md
--- a/WORK_LOG.md
+++ b/WORK_LOG.md
@@ -1,2 +1,4 @@
+### 2026-11-19
+
+- **PR #zz**: Garbage'

: >"$WL_CALL_LOG"
out="$(WL_DIFF="$GOOD_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "ELIGIBLE" "$out" "a diff adding a matching PR and closed issue is eligible"
assert_eq "repos/owner/mine/issues/5
repos/owner/mine/issues/7" "$(cat "$WL_CALL_LOG")" \
    "the gate read the base repository owner/mine, once per distinct number"
if grep -q 'owner/other' "$WL_CALL_LOG"; then
    fail "the gate queried the foreign repository"
else
    pass "the gate never queried the foreign repository"
fi

out="$(FAIL_LOOKUP=1 WL_DIFF="$GOOD_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #5 does not exist in owner/mine (or the read failed)" "$out" \
    "a temporary forge failure on the target read is not eligible (fails closed)"

out="$(WL_DIFF="" docs_only_fast_path_worklog_check 1)"
assert_eq "ELIGIBLE" "$out" "a WORK_PLAN/README-only diff (no canonical records) is eligible — no-op preserved"

: >"$WL_CALL_LOG"
out="$(WL_DIFF="$ROTATION_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "ELIGIBLE" "$out" "an exact archive rotation (added records all matched by removals) is eligible"
[[ -s "$WL_CALL_LOG" ]] && fail "a pure rotation still queried the forge" || pass "a pure rotation made zero forge reads"

out="$(WL_DIFF="$ROTATION_PLUS_NEW_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #9 does not exist in owner/mine (or the read failed)" "$out" \
    "records moved to a monthly archive are skipped, but a new record alongside them is still verified"

# Wrong-repo listings: a number absent from the target, and a number present
# in BOTH repos but with a different title or a different kind than recorded.
out="$(WL_DIFF="$ABSENT_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #407 does not exist in owner/mine (or the read failed)" "$out" \
    "a foreign repo's real PR number (absent here) is not eligible"

out="$(WL_DIFF="$TITLE_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #5 title mismatch in owner/mine (record 'Totally different title', forge 'Add widget')" "$out" \
    "a number present in the target under a different title is not eligible"

out="$(WL_DIFF="$ISSUE_AS_PR_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #5 is not an issue in owner/mine: - **Issue #5** (closed): Add widget" "$out" \
    "a number present in the target with the wrong kind (issue vs PR) is not eligible"

out="$(WL_DIFF="$PR_AS_ISSUE_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #7 is not a PR in owner/mine: - **PR #7**: Fix gizmo" "$out" \
    "a closed issue recorded as a PR is not eligible"

# Source-supplied repository identity cannot override the target binding:
# a foreign row carrying the SAME number and title as the real target item.
out="$(WL_DIFF="$FOREIGN_REPO_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record claims repository owner/other, not the base owner/mine: - **PR owner/other#5**: Add widget" "$out" \
    "a foreign-repo-identified record is refused even when number and title coincide"

# Mixed valid/invalid: one bad entry refuses the whole approval.
out="$(WL_DIFF="$MIXED_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: WORK_LOG record #99 does not exist in owner/mine (or the read failed)" "$out" \
    "one invalid record among valid ones bars the fast path"

# A canonical-shaped line with an unparseable record body fails closed.
out="$(WL_DIFF="$UNPARSEABLE_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: unparseable WORK_LOG record: - **PR #zz**: Garbage" "$out" \
    "a canonical-shaped line with a non-numeric number fails closed"

# Reads that fail must not read as eligible: the target and the diff.
out="$(WL_BASE_RC=1 WL_DIFF="$GOOD_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: base repository read failed" "$out" "a failed base-repository read is not eligible"
out="$(WL_BASE_OUT='' WL_DIFF="$GOOD_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: base repository read failed" "$out" "an empty base-repository read is not eligible"
out="$(WL_DIFF_RC=1 WL_DIFF="$GOOD_DIFF" docs_only_fast_path_worklog_check 1)"
assert_eq "NOT ELIGIBLE: PR diff read failed" "$out" "a failed diff read is not eligible"

unset -f gh

echo
echo "--- Doc pins: judge.md wires the record gate into the fast path (#13760) ---"

assert_doc_contains "$JUDGE_MD" \
    'docs_only_fast_path_worklog_check() {' \
    "judge.md defines docs_only_fast_path_worklog_check()"
assert_doc_contains "$JUDGE_MD" \
    'ELIGIBILITY=$(docs_only_fast_path_worklog_check "$PR_NUMBER")' \
    "judge.md folds the WORK_LOG record check into ELIGIBILITY"
assert_doc_contains "$JUDGE_MD" \
    "#13760" \
    "judge.md documents the #13760 record gate"
assert_doc_contains "$JUDGE_MD" \
    "and every WORK_LOG record the diff adds verifies against the base repository" \
    "judge.md Primary Queue step 4b states the record-verification condition"
assert_doc_contains "$JUDGE_MD" \
    'gh pr diff "$pr"' \
    "judge.md reads the exact reviewed diff (not Guide's receipts or the PR prose)"
assert_doc_contains "$JUDGE_MD" \
    "baseRepository" \
    "judge.md derives the target from the PR's base repository"

# The Guide-side gate is the counterpart — guide.md must ship it and wire it.
assert_doc_contains "$GUIDE_MD" \
    'verify_work_log_provenance() {' \
    "guide.md defines verify_work_log_provenance()"
assert_doc_contains "$GUIDE_MD" \
    "#13760" \
    "guide.md documents the #13760 provenance gate"

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
