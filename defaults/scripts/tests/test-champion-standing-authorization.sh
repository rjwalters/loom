#!/usr/bin/env bash
# test-champion-standing-authorization.sh - Regression tests for the Champion
# standing-authorization mechanism (#6850): an optional
# `.loom/config.json` -> `champion.standingAuthorizations` block letting an
# operator pre-authorize an entire merge-risk *class* (e.g. guard-hook edits)
# so criterion #2 (Merge-Risk Judgment) can skip the four-axis judgment for a
# matching PR, given every stated condition mechanically holds.
#
# champion-pr-merge.md's "Standing operator authorization (#6850)" subsection
# is prose an LLM instance reads and executes, not a standalone script (same
# situation as test-docs-only-fast-path.sh and
# test-champion-critical-file-check.sh) -- so this file mirrors the documented
# matcher/validator logic in a local copy and pins the shipped markdown's
# exact commands with `assert_doc_contains`, catching drift between the two.
#
# What this asserts:
#   1. The mirrored glob matcher correctly requires an EXACT SUBSET match
#      (every changed file matches at least one pattern in the class) --
#      one file outside the class's patterns disqualifies the whole PR from
#      that class, mirroring the docs-only fast path's exact-subset rule.
#   2. A malformed class entry (missing id/filePatterns/conditions, or an
#      unrecognized condition string) is treated as invalid -- it never
#      authorizes anything -- without disabling sibling entries in the array.
#   3. No `champion.standingAuthorizations` key, or an empty array, is a
#      structural no-op (mirrors "absent = byte-for-byte unchanged").
#   4. champion-pr-merge.md ships the config schema, the closed condition
#      vocabulary, the fail-safe-on-malformed-entry behavior, and documents
#      this mechanism as orthogonal to `loom:auto-merge-ok`.
#   5. champion-reference.md documents the new mechanism too.
#
# Usage:
#   ./.loom/scripts/tests/test-champion-standing-authorization.sh

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
CHAMPION_MD="$PROMPT_DIR/champion-pr-merge.md"
CHAMPION_REF_MD="$PROMPT_DIR/champion-reference.md"

# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Pin a literal snippet as present verbatim in a doc file -- catches drift
# between this test's mirrored logic and the shipped markdown.
assert_doc_contains() {
    local file="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if grep -qF -- "$needle" "$file"; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg (missing literal in $file: $needle)"
    fi
}

# =====================================================================
# Mirrored, in isolation, from champion-pr-merge.md's "Standing operator
# authorization (#6850)" subsection: the exact-subset glob matcher and the
# closed-vocabulary condition validator. The live doc also re-verifies each
# condition against `gh`/criterion #1/#6 state -- that part is not testable
# hermetically here and is instead pinned via assert_doc_contains below.
# =====================================================================

# class_matches_files <newline-separated-files> <newline-separated-patterns>
# Echoes "MATCH" if every file matches at least one pattern, else
# "NO MATCH: <first non-matching file>".
class_matches_files() {
    local files="$1" patterns="$2"
    local file pattern matched saw_any=0
    while IFS= read -r file; do
        [ -z "$file" ] && continue
        saw_any=1
        matched=0
        while IFS= read -r pattern; do
            [ -z "$pattern" ] && continue
            # shellcheck disable=SC2053
            if [[ "$file" == $pattern ]]; then
                matched=1
                break
            fi
        done <<<"$patterns"
        if [ "$matched" -ne 1 ]; then
            echo "NO MATCH: $file"
            return 0
        fi
    done <<<"$files"
    if [ "$saw_any" -eq 0 ]; then
        echo "NO MATCH: empty file list"
        return 0
    fi
    echo "MATCH"
}

# class_entry_valid <id> <patterns> <conditions>
# Echoes "VALID" or "INVALID: <reason>" -- mirrors the fail-safe checks that
# run before matching: missing id/patterns/conditions, or any condition
# outside the closed vocabulary, invalidates the entry.
class_entry_valid() {
    local id="$1" patterns="$2" conditions="$3" cond
    if [ -z "$id" ] || [ -z "$patterns" ] || [ -z "$conditions" ]; then
        echo "INVALID: missing id/filePatterns/conditions"
        return 0
    fi
    while IFS= read -r cond; do
        [ -z "$cond" ] && continue
        case "$cond" in
            judgeApproval | greenCi) ;;
            *)
                echo "INVALID: unrecognized condition '$cond'"
                return 0
                ;;
        esac
    done <<<"$conditions"
    echo "VALID"
}

echo "--- class_matches_files: exact-subset glob matching ---"

out="$(class_matches_files ".loom/hooks/guard-destructive.sh" ".loom/hooks/guard-*.sh")"
assert_eq "MATCH" "$out" "a single guard-hook file matches its own class pattern"

out="$(class_matches_files "$(printf '.loom/hooks/guard-a.sh\ndefaults/hooks/guard-b.sh')" "$(printf '.loom/hooks/guard-*.sh\ndefaults/hooks/guard-*.sh')")"
assert_eq "MATCH" "$out" "files matching either of two patterns in the same class both match"

out="$(class_matches_files "$(printf '.loom/hooks/guard-a.sh\nsrc/lib.rs')" ".loom/hooks/guard-*.sh")"
assert_eq "NO MATCH: src/lib.rs" "$out" \
    "a guard-hook file plus an unrelated source file is rejected (mixed diff, exact-subset rule)"

out="$(class_matches_files "merge-pr.sh" ".loom/hooks/guard-*.sh")"
assert_eq "NO MATCH: merge-pr.sh" "$out" \
    "a file outside the class's patterns entirely is rejected"

out="$(class_matches_files "" ".loom/hooks/guard-*.sh")"
assert_eq "NO MATCH: empty file list" "$out" \
    "an empty diff (no files at all) is rejected, not silently treated as a match"

out="$(class_matches_files ".loom/hooks/guard-x.sh.bak" ".loom/hooks/guard-*.sh")"
assert_eq "NO MATCH: .loom/hooks/guard-x.sh.bak" "$out" \
    "bash glob semantics: guard-*.sh does NOT match guard-x.sh.bak (the pattern must match the full string, so a trailing suffix disqualifies it)"

echo
echo "--- class_entry_valid: fail-safe on malformed/unrecognized entries ---"

out="$(class_entry_valid "guard-hooks" ".loom/hooks/guard-*.sh" "judgeApproval")"
assert_eq "VALID" "$out" "a well-formed single-condition entry is valid"

out="$(class_entry_valid "guard-hooks" ".loom/hooks/guard-*.sh" "$(printf 'judgeApproval\ngreenCi')")"
assert_eq "VALID" "$out" "a well-formed multi-condition entry is valid"

out="$(class_entry_valid "" ".loom/hooks/guard-*.sh" "judgeApproval")"
assert_eq "INVALID: missing id/filePatterns/conditions" "$out" \
    "an entry with no id is invalid"

out="$(class_entry_valid "guard-hooks" "" "judgeApproval")"
assert_eq "INVALID: missing id/filePatterns/conditions" "$out" \
    "an entry with no filePatterns is invalid"

out="$(class_entry_valid "guard-hooks" ".loom/hooks/guard-*.sh" "")"
assert_eq "INVALID: missing id/filePatterns/conditions" "$out" \
    "an entry with no conditions is invalid"

out="$(class_entry_valid "guard-hooks" ".loom/hooks/guard-*.sh" "reviewedByTwoHumans")"
assert_eq "INVALID: unrecognized condition 'reviewedByTwoHumans'" "$out" \
    "an entry naming a condition outside the closed vocabulary is invalid (never silently treated as satisfied)"

out="$(class_entry_valid "guard-hooks" ".loom/hooks/guard-*.sh" "$(printf 'judgeApproval\nreviewedByTwoHumans')")"
assert_eq "INVALID: unrecognized condition 'reviewedByTwoHumans'" "$out" \
    "one unrecognized condition invalidates the whole entry, even alongside a recognized one"

echo
echo "--- No config / empty array: structural no-op ---"

# jq's own `// []` default is what the shipped doc relies on for the
# no-config-present case -- pin that jq behaves as expected rather than
# re-implementing config parsing here.
result=$(printf '{}' | jq '(.champion.standingAuthorizations // []) | length > 0')
assert_eq "false" "$result" "a config with no champion key at all is treated as zero classes"

result=$(printf '{"champion":{"standingAuthorizations":[]}}' | jq '(.champion.standingAuthorizations // []) | length > 0')
assert_eq "false" "$result" "an explicit empty standingAuthorizations array is treated as zero classes"

result=$(printf '{"champion":{"standingAuthorizations":[{"id":"x"}]}}' | jq '(.champion.standingAuthorizations // []) | length > 0')
assert_eq "true" "$result" "a non-empty standingAuthorizations array is detected"

echo
echo "--- Doc pins: champion-pr-merge.md ships the standing-authorization mechanism ---"

assert_doc_contains "$CHAMPION_MD" \
    "Standing operator authorization (#6850)" \
    "champion-pr-merge.md documents the #6850 standing-authorization mechanism"

assert_doc_contains "$CHAMPION_MD" \
    "champion.standingAuthorizations" \
    "champion-pr-merge.md names the champion.standingAuthorizations config key"

assert_doc_contains "$CHAMPION_MD" \
    'judgeApproval | greenCi' \
    "champion-pr-merge.md defines the closed condition vocabulary"

assert_doc_contains "$CHAMPION_MD" \
    "STANDING_AUTH_RESULT" \
    "champion-pr-merge.md defines the STANDING_AUTH_RESULT variable used in the Decision rule"

assert_doc_contains "$CHAMPION_MD" \
    "fail safe" \
    "champion-pr-merge.md documents fail-safe behavior for a malformed class entry"

assert_doc_contains "$CHAMPION_MD" \
    "orthogonal" \
    "champion-pr-merge.md documents the standing authorization as orthogonal to loom:auto-merge-ok"

assert_doc_contains "$CHAMPION_MD" \
    "Reuses criterion #6's OWN read" \
    "champion-pr-merge.md's greenCi condition reuses criterion #6's read_ci_checks rather than a second, divergent implementation"

echo
echo "--- #12528: the shipped standing_auth_eval, executed with stubbed forge reads ---"

# Extract the REAL function and KEPT_SET_PATTERNS from the doc and run them,
# so the decision is tested rather than mirrored.
EVAL_SRC="$(mktemp)"
CFG_DIR="$(mktemp -d)"
trap 'rm -f "$EVAL_SRC"; rm -rf "$CFG_DIR"' EXIT
awk '/^KEPT_SET_PATTERNS=\(/{p=1} p{print} /^standing_auth_eval any$/{exit}' "$CHAMPION_MD" | sed '$d' >"$EVAL_SRC"

# run_eval <mode> <config-json> <files> <patches-json> <ci: green|red> <labels>
# patches-json is the files-API body the stubbed `gh api` returns.
run_eval() {
    local mode="$1" cfg="$2" files="$3" api="$4" ci="$5" labels="$6"
    printf '%s' "$cfg" >"$CFG_DIR/config.json"
    (
        CONFIG_FILE="$CFG_DIR/config.json"
        PR_NUMBER=1
        FAST_PATH_FILES="$files"
        LABELS="$labels"
        gh() { if [ -n "$api" ]; then printf '%s' "$api"; else return 1; fi; }
        read_ci_checks() {
            NO_CHECKS=false
            if [ "$ci" = "green" ]; then CHECKS='[{"name":"t","bucket":"pass"}]'; else CHECKS='[{"name":"t","bucket":"fail"}]'; fi
        }
        # shellcheck disable=SC1090
        source "$EVAL_SRC"
        standing_auth_eval "$mode"
        echo "$STANDING_AUTH_RESULT:$STANDING_AUTH_ID"
    )
}

CI_CLASS='{"champion":{"standingAuthorizations":[{"id":"ci-steps","filePatterns":[".github/workflows/*.yml"],"conditions":["judgeApproval","greenCi"],"coversCriticalFiles":true,"excludeFilePatterns":[".github/workflows/*publish*"],"forbiddenDiffPatterns":["^\\s*permissions:","secrets\\."]}]}}'
CI_CLASS_NOFLAG='{"champion":{"standingAuthorizations":[{"id":"ci-steps","filePatterns":[".github/workflows/*.yml"],"conditions":["judgeApproval","greenCi"]}]}}'
OKPATCH='[{"filename":".github/workflows/ci.yml","patch":"@@ -1 +1,2 @@\n name: ci\n+# a comment"}]'
PERMPATCH='[{"filename":".github/workflows/ci.yml","patch":"@@ -1 +1,2 @@\n name: ci\n+permissions: write-all"}]'
NOPATCH='[{"filename":".github/workflows/ci.yml"}]'
WF=".github/workflows/ci.yml"

out="$(run_eval critical "$CI_CLASS" "$WF" "$OKPATCH" green "loom:pr")"
assert_eq "ELIGIBLE:ci-steps" "$out" "covering class, whole diff in class, approved + green CI: criterion #3 passes, class named"

out="$(run_eval critical "$CI_CLASS" "$(printf '%s\nCargo.toml' "$WF")" "$OKPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "one file outside the class restores FAIL"

out="$(run_eval critical "$CI_CLASS" ".github/workflows/publish.yml" "$OKPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "an excludeFilePatterns file restores FAIL"

out="$(run_eval critical "$CI_CLASS" "$WF" "$PERMPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "a forbiddenDiffPatterns line restores FAIL"

out="$(run_eval critical "$CI_CLASS" "$WF" "$NOPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "a changed file with no patch (unreadable diff) fails closed"

out="$(run_eval critical "$CI_CLASS" "$WF" "" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "a failed files-API read fails closed"

out="$(run_eval critical "$CI_CLASS" "$WF" "$OKPATCH" red "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "red CI (greenCi does not hold) restores FAIL"

out="$(run_eval critical "$CI_CLASS" "$WF" "$OKPATCH" green "loom:review-requested")"
assert_eq "NOT ELIGIBLE:" "$out" "no Judge approval (judgeApproval does not hold) restores FAIL"

out="$(run_eval critical "$CI_CLASS_NOFLAG" "$WF" "$OKPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "a class without coversCriticalFiles never passes criterion #3"

out="$(run_eval any "$CI_CLASS_NOFLAG" "$WF" "$OKPATCH" green "loom:pr")"
assert_eq "ELIGIBLE:ci-steps" "$out" "criterion #2 behavior of a class without the new keys is unchanged"

BAD_FLAG="${CI_CLASS/\"coversCriticalFiles\":true/\"coversCriticalFiles\":\"true\"}"
out="$(run_eval critical "$BAD_FLAG" "$WF" "$OKPATCH" green "loom:pr")"
assert_eq "NOT ELIGIBLE:" "$out" "a non-boolean coversCriticalFiles does not opt in"

# Kept set: never satisfiable, even when the class names the path and sets the flag.
for kept in defaults/hooks/guard-x.sh defaults/scripts/merge-pr.sh defaults/scripts/install-loom.sh \
    defaults/scripts/resync-installed.sh defaults/scripts/spawn-claude.sh \
    defaults/.claude/commands/loom/champion-pr-merge.md .github/labels.yml db/migrations/001.sql; do
    KEPT_CFG="{\"champion\":{\"standingAuthorizations\":[{\"id\":\"all\",\"filePatterns\":[\"*\"],\"conditions\":[\"judgeApproval\"],\"coversCriticalFiles\":true}]}}"
    out="$(run_eval critical "$KEPT_CFG" "$kept" "" green "loom:pr")"
    assert_eq "NOT ELIGIBLE:" "$out" "kept-set path $kept is never waived, even by a class matching everything"
done

out="$(run_eval any "$KEPT_CFG" "defaults/hooks/guard-x.sh" "" green "loom:pr")"
assert_eq "ELIGIBLE:all" "$out" "kept-set floor is critical-mode only: criterion #2 class behavior is unchanged"

echo
echo "--- Doc pins: #12528 ---"
assert_doc_contains "$CHAMPION_MD" '`coversCriticalFiles` (#12528' "champion-pr-merge.md documents coversCriticalFiles"
assert_doc_contains "$CHAMPION_MD" '`excludeFilePatterns` (#12528' "champion-pr-merge.md documents excludeFilePatterns"
assert_doc_contains "$CHAMPION_MD" '`forbiddenDiffPatterns` (#12528' "champion-pr-merge.md documents forbiddenDiffPatterns"
assert_doc_contains "$CHAMPION_MD" "standing_auth_eval critical" "criterion #3 runs the class check before it emits FAIL"
assert_doc_contains "$CHAMPION_MD" "Revertability counts only what happens on merge or roll (#12528)" "revertability-axis sentence is present"
assert_doc_contains "$CHAMPION_MD" "No standing authorization satisfies the kept set (#12528)" "kept set restated as never satisfiable"
assert_doc_contains "$PROMPT_DIR/champion-critical-file-hold.md" "A covering standing authorization means no hold (#12528)" "champion-critical-file-hold.md documents the covering class"

echo
echo "--- Doc pins: champion-reference.md documents the new mechanism ---"

assert_doc_contains "$CHAMPION_REF_MD" \
    "Standing Operator Authorization for a Merge-Risk Class (#6850)" \
    "champion-reference.md has an Edge Case for the #6850 mechanism"

assert_doc_contains "$CHAMPION_REF_MD" \
    "champion.standingAuthorizations" \
    "champion-reference.md names the champion.standingAuthorizations config key"

assert_doc_contains "$CHAMPION_REF_MD" \
    "#6850" \
    "champion-reference.md's decision matrix references #6850"

echo
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
