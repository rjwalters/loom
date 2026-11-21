#!/usr/bin/env bash
# test-guide-blocked-unblock-limit.sh - Regression test for issue #10553.
#
# guide.md's unblock sweep listed `loom:blocked` issues with no --limit, so
# gh's default of 30 silently skipped every issue past the first 30. Verifies:
#   1. Both loom:blocked listings pass an explicit --limit well above 30.
#   2. check_and_unblock(), run against a gh stub that honors --limit
#      (default 30) and 45 fixtures, examines all 45 issues.
#   3. A listing that hits the limit logs a truncation warning.
#
# Hermetic: the issue and PR forge calls use fixture JSON; `jq` is real.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
TESTS_RUN=0; TESTS_PASSED=0; TESTS_FAILED=0
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found at $GUIDE_MD"; exit 1
fi
command -v jq >/dev/null 2>&1 || { echo "SKIP: jq not available"; exit 0; }

echo "Test 1: every loom:blocked issue listing passes an explicit --limit > 30"
LISTINGS="$(grep -c 'issue list --label "loom:blocked"' "$GUIDE_MD")"
WITH_LIMIT="$(grep 'issue list --label "loom:blocked"' "$GUIDE_MD" | grep -c -- '--limit')"
if [[ "$LISTINGS" -ge 2 && "$LISTINGS" == "$WITH_LIMIT" ]]; then
    pass "all $LISTINGS loom:blocked listings pass --limit"
else
    fail "expected every loom:blocked listing to pass --limit (listings=$LISTINGS, with limit=$WITH_LIMIT)"
fi

echo ""
echo "Test 2: check_and_unblock() examines all 45 fixtures (not just 30)"
FN="$(awk '/^check_and_unblock\(\) \{/{f=1} f{print} /^\}/{if(f){exit}}' "$GUIDE_MD")"
if [[ -z "$FN" ]]; then
    fail "could not extract check_and_unblock() from guide.md"
else
    TMPROOT="$(mktemp -d)"; trap 'rm -rf "$TMPROOT"' EXIT
    GH_STUB="$TMPROOT/gh"
    cat > "$GH_STUB" <<'STUB'
#!/usr/bin/env bash
# `issue list` honors --limit (default 30) over $FIXTURE_JSON, like real gh.
set -uo pipefail
if [[ "${1:-}" == "issue" && "${2:-}" == "list" ]]; then
  shift 2; limit=30
  while [[ $# -gt 0 ]]; do
    case "$1" in --limit) limit="$2"; shift 2 ;; *) shift ;; esac
  done
  printf '%s' "$FIXTURE_JSON" | jq -c ".[0:$limit]"
  exit 0
fi
echo "stub gh: unexpected call: $*" >&2
exit 1
STUB
    chmod +x "$GH_STUB"
    # The parse_dependencies hook logs one line per issue the loop reaches.
    FIXTURE="$(jq -nc '[range(1;46) | {number: ., title: "t\(.)", body: "no deps"}]')"
    run_fn() {
      FIXTURE_JSON="$FIXTURE" GH_READ="$GH_STUB" SEEN="$TMPROOT/seen" FN_SRC="$FN" \
        bash -c '
          : > "$SEEN"
          check_and_unblock_prs() { :; }
          eval "$FN_SRC"
          # Count issues the loop reaches via a parse_dependencies hook.
          parse_dependencies() { echo x >> "$SEEN"; }
          check_and_unblock' 2>"$TMPROOT/stderr" >/dev/null
    }
    run_fn
    SEEN_N="$(wc -l < "$TMPROOT/seen")"
    if [[ "$SEEN_N" -eq 45 ]]; then pass "all 45 blocked issues were examined"
    else fail "expected 45 issues examined, got $SEEN_N (a 30-cap is in effect)"; fi
    if grep -q 'WARNING' "$TMPROOT/stderr"; then fail "unexpected truncation warning below the limit"
    else pass "no truncation warning when the listing is under the limit"; fi

    echo ""
    echo "Test 3: a listing that hits the limit logs a truncation warning"
    FIXTURE="$(jq -nc '[range(1;1001) | {number: ., title: "t", body: "x"}]')"
    run_fn
    if grep -q 'WARNING.*limit' "$TMPROOT/stderr"; then pass "truncation warning logged when count == limit"
    else fail "expected truncation warning on stderr (got: $(cat "$TMPROOT/stderr"))"; fi
fi

echo ""
echo "Test 4: the parked PR sweep reaches older PRs and respects current holds"
(
    TMPROOT="$(mktemp -d)"; trap 'rm -rf "$TMPROOT"' EXIT
    DOC="$REPO_ROOT/defaults/docs/park-record.md"
    for fn in check_and_unblock_prs pr_has_superseding_block pr_trusted_marker pr_conflict_snapshot pr_has_reason_only_park pr_park_episode pr_park_clearance_marker route_parked_pr_conflict previous_review_label; do
        eval "$(sed -n "/^$fn() {\$/,/^}\$/p" "$DOC")"
    done
    cd "$TMPROOT" || exit 1
    mkdir -p .loom/scripts
    export WRITES="$PWD/writes" COMMENTS="$PWD/comments"
    : > "$WRITES"; : > "$COMMENTS"
    cat > .loom/scripts/post-comment.sh <<'STUB'
#!/usr/bin/env bash
jq -nc --argjson number "$1" --arg body "$4" '{number:$number,body:$body}' >> "$COMMENTS"
STUB
    chmod +x .loom/scripts/post-comment.sh

    fixture=$(jq -nc '[range(1;116) | {number:.,title:"t",body:(. | tostring),
      labels:(if . == 102 then [{name:"loom:blocked"},{name:"loom:frozen"}] else [{name:"loom:blocked"}] end)}]')
    export GH_READ=gh
    VIEWS="$PWD/views"
    own_diff='defect in the diff'
    gh() {
        case "$1 $2" in
            'pr list')
                local limit=30
                while [[ $# -gt 1 ]]; do
                    if [[ "$1" == --limit ]]; then limit="$2"; break; fi
                    shift
                done
                if [[ -s "$WRITES" ]]; then
                    jq -c --argjson limit "$limit" '[.[] | select(.number != 91)][0:$limit]' <<<"$fixture"
                else
                    jq -c --argjson limit "$limit" '.[0:$limit]' <<<"$fixture"
                fi ;;
            'pr view')
                local number="$3" extra='' mergeable=MERGEABLE status=BLOCKED review='' body="$3"
                printf '%s\n' "$number" >> "$VIEWS"
                case "$number" in
                    91) extra=loom:changes-requested ;;
                    92) extra=loom:operator ;;
                    97) return 1 ;;
                    98) mergeable=UNKNOWN; status=UNKNOWN ;;
                    99) mergeable=CONFLICTING; status=DIRTY ;;
                    100) review=CHANGES_REQUESTED ;;
                    101) body='new park added since listing' ;;
                    103) extra=loom:operator-blocked ;;
                    105) extra=loom:frozen ;;
                    106) review='FUTURE_UNRECOGNIZED_VALUE' ;;
                esac
                jq -nc --arg body "$body" --arg extra "$extra" --arg mergeable "$mergeable" \
                    --arg status "$status" --arg review "$review" '
                    {state:"OPEN",body:$body,labels:[{name:"loom:blocked"}],mergeable:$mergeable,
                     mergeStateStatus:$status,reviewDecision:$review} |
                    if $extra != "" then .labels += [{name:$extra}] else . end' ;;
            'pr edit') printf '%s\n' "$*" >> "$WRITES" ;;
            'api --paginate') return 0 ;;
            *) return 1 ;;
        esac
    }
    loom-daemon() {
        case "$1 $2" in
            'park-record parse')
                local number
                number=$(cat)
                case "$number" in
                    91) echo '{"records":[{"blocker":6343},{"blocker":6348},{"blocker":6354}]}' ;;
                    95) jq -nc --arg reason "$own_diff" '{records:[{blocker:6343},{blocker:null,reason:$reason}]}' ;;
                    96) echo '{"records":[{"blocker":6343},{"blocker":7000,"blocker_repo":"example-org/other"}]}' ;;
                    104) echo '{"records":[{"blocker":null,"reason":"ci-gate"}]}' ;;
                    92|93|94|97|98|99|100|101|102|103|105|106) echo '{"records":[{"blocker":6343}]}' ;;
                    *) echo '{"records":[]}' ;;
                esac ;;
            'forge trusted-comments') jq -sc --argjson pr "$4" '[.[] | select(.number == $pr)]' "$COMMENTS" ;;
            'champion-hold classify')
                local state=foreign-park detail=-
                case "$4" in
                    93) state=champion-hold; detail=ratification ;;
                    94) state=champion-hold; detail=critical-file ;;
                esac
                printf 'LOOM-CHAMPION-HOLD state=%s pr=%s detail=%s\n' "$state" "$4" "$detail" ;;
            *) return 1 ;;
        esac
    }
    resolve_dep_states() {
        case "$1" in
            $'6343\n6348\n6354') printf 'true\n#6343 #6348 #6354\n' ;; # one closed manually, two on merge
            6343) printf 'true\n#6343\n' ;;
            *) printf 'false\n\n' ;; # a cross-repo blocker is still open
        esac
    }

    check_and_unblock_prs >/dev/null
    [[ $(cat "$WRITES") == 'pr edit 91 --remove-label loom:blocked --add-label loom:changes-requested' ]] || exit 1
    check_and_unblock_prs >/dev/null
    own_diff='a different defect still needs attention'
    check_and_unblock_prs >/dev/null
    [[ $(wc -l < "$WRITES") -eq 1 ]] || exit 1
    if grep -Eq '^102$' "$VIEWS"; then exit 1; fi
    grep -Eq '^105$' "$VIEWS" || exit 1
    jq -es '
      [.[] | select(.body | startswith("<!-- loom:guide-pr-superseding:")) | .number] | sort
      == [92,93,94,95,95,99,100,103]' "$COMMENTS" >/dev/null
)
if [[ $? -eq 0 ]]; then pass "PR beyond the default limit re-enters Doctor once eligible, held rows keep their labels and one notice";
else fail "PR sweep missed the older eligible PR, released a held row, or duplicated a notice"; fi

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"; exit 1; fi
echo "All tests passed"
exit 0
