#!/usr/bin/env bash
# test-guide-skip-docs.sh - Regression test for issue #14116
#
# `guide.docsMaintenance.skipDocs` is the recorded per-file opt-out for Guide's
# WORK_LOG.md / WORK_PLAN.md generation (the "tracked separately" placeholder
# #14009 left in the roadmap-convention subsection). This suite pins, on EVERY
# shipped copy of the Guide role prompt (source + generated skill copy):
#   1. the placeholder text is gone and the convention names skipDocs, and
#      rule 2's create-by-default cross-references the opt-out;
#   2. update_work_log() and update_work_plan() each call the skip check for
#      their own file as their first statement (before any forge query /
#      render), and rotate_work_log() does too;
#   3. behaviour: the real docs_maintenance_skipped() function, extracted from
#      the prompt, honours a scratch .loom/config.json — one file skipped
#      leaves the other unaffected; both skipped; empty/absent/unknown/
#      non-array/malformed config skips nothing.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }
grep_has() { if grep -q -- "$2" "$1" 2>/dev/null; then pass "$3"; else fail "$3 (pattern not found in $1)"; fi; }
grep_lacks() { if grep -q -- "$2" "$1" 2>/dev/null; then fail "$3 (forbidden pattern found in $1)"; else pass "$3"; fi; }

# first_stmt FILE FUNC -> the first non-comment, non-blank line of FUNC's body
# (and, for the guard, the line after it, which must be the echo/return pair —
# so we return the first TWO lines joined for a stable match).
first_stmt() {
    awk -v fn="$2" '
        $0 ~ "^"fn"\\(\\) \\{" { infn=1; next }
        infn && /^[[:space:]]*#/ { next }
        infn && /^[[:space:]]*$/ { next }
        infn { print; if (++n == 1) exit }
    ' "$1"
}

command -v jq >/dev/null 2>&1 || { echo "SKIP: jq not installed"; exit 0; }

for copy in "defaults/.claude/commands/loom/guide.md" "defaults/.agents/skills/loom-guide/SKILL.md"; do
    f="$REPO_ROOT/$copy"; echo "Copy: $copy"
    [[ -f "$f" ]] || { fail "$copy exists"; continue; }

    # 1. Placeholder gone; key named.
    grep_lacks "$f" 'tracked separately' "$copy: 'tracked separately' placeholder removed"
    grep_lacks "$f" 'once #14116 lands' "$copy: 'once #14116 lands' placeholder removed"
    grep_lacks "$f" 'Until one lands' "$copy: 'Until one lands' placeholder removed"
    grep_has "$f" 'guide\.docsMaintenance\.skipDocs' "$copy: convention names guide.docsMaintenance.skipDocs"
    grep_has "$f" 'neither created' "$copy: convention says a skipped file is neither created nor updated"
    grep_has "$f" '`skipDocs` — rule 3)' "$copy: rule 2 create-by-default cross-references the skipDocs opt-out (rule 3)"

    # 2. Honour sites: guard is the first statement of each function.
    grep_has "$f" "jq -r '(.guide.docsMaintenance.skipDocs // \[\])" "$copy: skipDocs read via the sibling-key jq idiom"
    [[ "$(first_stmt "$f" update_work_log)" == *"docs_maintenance_skipped WORK_LOG.md"* ]] \
        && pass "$copy: update_work_log() early-returns on skipped WORK_LOG.md" \
        || fail "$copy: update_work_log() must start with docs_maintenance_skipped WORK_LOG.md"
    [[ "$(first_stmt "$f" update_work_plan)" == *"docs_maintenance_skipped WORK_PLAN.md"* ]] \
        && pass "$copy: update_work_plan() early-returns on skipped WORK_PLAN.md (first creation included)" \
        || fail "$copy: update_work_plan() must start with docs_maintenance_skipped WORK_PLAN.md"
    grep_has "$f" 'docs_maintenance_skipped WORK_LOG.md && return 1' "$copy: rotate_work_log() leaves a skipped WORK_LOG.md untouched"

    # 3. Behaviour of the real function.
    fn="$(sed -n '/^docs_maintenance_skipped() {/,/^}/p' "$f")"
    if [[ -z "$fn" ]]; then fail "$copy: docs_maintenance_skipped() extractable"; continue; fi
    scratch="$(mktemp -d)"; mkdir -p "$scratch/.loom"
    check() { # config-json expected-log expected-plan description
        if [[ "$1" == "-" ]]; then rm -f "$scratch/.loom/config.json"; else printf '%s' "$1" >"$scratch/.loom/config.json"; fi
        local log plan
        log=$(cd "$scratch" && bash -c "$fn"$'\n''docs_maintenance_skipped WORK_LOG.md && echo yes || echo no')
        plan=$(cd "$scratch" && bash -c "$fn"$'\n''docs_maintenance_skipped WORK_PLAN.md && echo yes || echo no')
        if [[ "$log" == "$2" && "$plan" == "$3" ]]; then pass "$copy: $4"; else fail "$copy: $4 (log=$log plan=$plan)"; fi
    }
    check '{"guide":{"docsMaintenance":{"skipDocs":["WORK_PLAN.md"]}}}' no yes  "skipping WORK_PLAN.md leaves WORK_LOG.md active"
    check '{"guide":{"docsMaintenance":{"skipDocs":["WORK_LOG.md"]}}}'  yes no  "skipping WORK_LOG.md leaves WORK_PLAN.md active"
    check '{"guide":{"docsMaintenance":{"skipDocs":["WORK_LOG.md","WORK_PLAN.md"]}}}' yes yes "both skipped"
    check '{"guide":{"docsMaintenance":{"skipDocs":[]}}}' no no "empty skipDocs skips nothing"
    check '{"guide":{"docsMaintenance":{}}}' no no "absent skipDocs skips nothing"
    check '{"guide":{"docsMaintenance":{"skipDocs":["README.md","bogus",3,null]}}}' no no "unrecognised entries are ignored"
    check '{"guide":{"docsMaintenance":{"skipDocs":"WORK_PLAN.md"}}}' no no "non-array value skips nothing"
    check 'not json' no no "malformed config skips nothing"
    check - no no "missing config skips nothing"
    rm -rf "$scratch"
done

echo "Tests run: $TESTS_RUN"
[[ $TESTS_FAILED -eq 0 ]] && echo "All tests passed" || { echo "Tests failed: $TESTS_FAILED"; exit 1; }
