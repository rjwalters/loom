#!/usr/bin/env bash
# test-guide-park-record-precedence.sh - Regression test for issues #13414/#14683
#
# Guide's unblock sweep lifted operator-ruled parks because unchecked
# acceptance-criterion `- [ ] #N` lines counted as gates. A park record now wins.
# parse_dependencies / has_restore_note are extracted VERBATIM from each shipped
# copy and run against fixtures (and a stubbed loom-daemon). For #14683 the
# whole check_and_unblock is extracted the same way and driven on the incident
# body (closed prose `Blocked by #333` over a live park record naming an open
# blocker): the label must stay; a control run with the blocker closed lifts it.
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }
assert_eq() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (got '$1', expected '$2')"; fi; }
command -v jq >/dev/null 2>&1 || { echo "SKIP: jq not available"; exit 0; }

# The stub dispatches on its arguments (#13895): one catch-all answer made the
# `park-record parse` call in parse_dependencies read has_restore_note's `[]`.
#   forge trusted-comments  -> $STUB_COMMENTS
#   park-record parse       -> $STUB_PARK_OUT, exit $STUB_PARK_RC, following the
#                              real contract: 0 = a live record exists (stdout =
#                              its blockers, one per line, possibly none),
#                              1 = no live record, anything else = daemon failure.
#   forge parent split-state -> NOT_SPLIT, exit 1 (an ordinary issue).
#   anything else           -> exit 2, so an unexpected call fails loudly.
STUB_COMMENTS='[]'
STUB_PARK_OUT=''; STUB_PARK_RC=2
loom-daemon() {
    case "$1 ${2:-}" in
        "forge trusted-comments") printf '%s\n' "$STUB_COMMENTS" ;;
        "park-record parse")
            cat >/dev/null
            [[ -z "$STUB_PARK_OUT" ]] || printf '%s\n' "$STUB_PARK_OUT"
            return "$STUB_PARK_RC" ;;
        "forge parent")
            [[ "$3" == "split-state" ]] && { echo "NOT_SPLIT"; return 1; }
            echo "stub loom-daemon: unexpected call: $*" >&2; return 2 ;;
        *) echo "stub loom-daemon: unexpected call: $*" >&2; return 2 ;;
    esac
}

# --- #14683: the label-removal point ----------------------------------------
# The incident body: closed prose `Blocked by #333` ABOVE a live operator park
# record naming still-open example-org/tool-repo#558 (plus an older `(unstated)`
# curator record). The daemon stub answers what the real parser answers on this
# body — exactly the qualified ref — which the park_record unit test locks in
# against the verbatim fixture.
TMPD="$(mktemp -d)"; trap 'rm -rf "$TMPD"' EXIT
CALLS="$TMPD/calls"   # every `gh issue edit` and posted comment, one per line
post_comment() { printf '%s\n' "$*" >> "$CALLS"; }
check_and_unblock_prs() { :; }        # tail calls inside check_and_unblock,
convert_awaiting_revision_parks() { :; }  # out of scope here
release_due_transient_parks() { :; }
INCIDENT_BODY="$(printf '%s\n' \
    'Blocked by #333' \
    '<!-- loom:park Blocked by: example-org/tool-repo#558 by=human at=2026-10-29T03:00:18Z reason="operator ruling q93: the hold now waits on this open blocker" -->' \
    '<!-- loom:park Blocked by: (unstated) by=curator at=2026-10-15T23:32:21Z reason="tracker only while #558 owns the overlapping repair; redispatch needs a distinct measured residual (operator triage 2026-10-15)" -->')"
BLOCKED_JSON="$(jq -nc --arg body "$INCIDENT_BODY" \
    '[{number:334, title:"incident fixture", body:$body, labels:[{name:"loom:blocked"}]}]')"
# gh stub: the loom:blocked listing ($GH_READ routes here), blocker state
# ($3 = number; #333 closed, #558 = $DEP_558), no linked PRs, no label history;
# `issue edit` is recorded.
gh() {
    case "$1 $2" in
        "issue list") printf '%s\n' "$BLOCKED_JSON" ;;
        "issue view")
            case "$*" in
                *closedByPullRequestsReferences*) return 0 ;;
            esac
            case "$3" in
                333) echo CLOSED ;;
                558) echo "$DEP_558" ;;
                *)   echo OPEN ;;
            esac ;;
        "issue edit") printf '%s\n' "$*" >> "$CALLS" ;;
        api*) return 0 ;;
        *) echo "stub gh: unexpected call: $*" >&2; return 1 ;;
    esac
}

for copy in "defaults/.claude/commands/loom/guide.md" "defaults/.agents/skills/loom-guide/SKILL.md"; do
    f="$REPO_ROOT/$copy"; echo "Copy: $copy"
    [[ -f "$f" ]] || { fail "$copy exists"; continue; }
    PD="$(sed -n '/^parse_dependencies() {$/,/^}$/p' "$f")"
    HR="$(sed -n '/^has_restore_note() {/,/^}$/p' "$f")"
    [[ -n "$PD" && -n "$HR" ]] || { fail "extract functions from $copy"; continue; }
    eval "$PD"; eval "$HR"

    AC=$'## Acceptance Criteria\n- [ ] #100: something closed\n'
    PARK=$'<!-- loom:park Blocked by: #200 by=builder at=2026-10-31T00:00:00Z reason="see #300" -->'
    UNSTATED=$'<!-- loom:park Blocked by: (unstated) by=daemon at=x -->\n- [ ] #100'
    # A marker demoted to a code span by `park-record retract` (#13895), plus its audit line.
    DEMOTED=$'`<!-- loom:park Blocked by: #200 by=builder at=2026-10-31T00:00:00Z -->`\nPark withdrawn: #200 \xe2\x80\x94 by=guide at=2026-11-15T12:00:00Z reason="not a gate"\n'

    # Daemon answers (exit 0): its blocker list is the dependency list.
    STUB_PARK_OUT='200'; STUB_PARK_RC=0
    assert_eq "$(parse_dependencies "$AC$PARK")" "200" "park record wins over AC checkbox; reason refs ignored"
    # Two lines, built with printf: an escape written next to the owner would
    # read as part of its name to check-vendored-private-refs.sh.
    TWO_DEPS="$(printf '%s\n%s' '200' 'example-org/tool-repo#7')"
    STUB_PARK_OUT="$TWO_DEPS"
    assert_eq "$(parse_dependencies "$AC$PARK")" "$TWO_DEPS" "daemon blocker list passed through, cross-repo ref intact"
    assert_eq "$(parse_dependencies "$AC$PARK" | wc -l | tr -d ' ')" "2" "daemon blocker list keeps one blocker per line"
    STUB_PARK_OUT=''
    assert_eq "$(parse_dependencies "$UNSTATED")" "" "unstated park record gates nothing from AC"
    STUB_PARK_OUT='201'
    assert_eq "$(parse_dependencies "$DEMOTED$PARK$AC")" "201" "daemon answer wins over the legacy grep (which would read #200)"

    # Exit 1 = no live record: zero gates, and no fall-through to the generic patterns.
    STUB_PARK_OUT=''; STUB_PARK_RC=1
    assert_eq "$(parse_dependencies "$DEMOTED$AC")" "" "demoted (code-span) marker declares nothing; AC not read"

    # Any other exit (old binary, crash): the legacy grep path still answers.
    STUB_PARK_OUT='999'; STUB_PARK_RC=2
    assert_eq "$(parse_dependencies "$AC$PARK")" "200" "daemon failure: legacy grep reads the park record"
    assert_eq "$(parse_dependencies "$UNSTATED")" "" "daemon failure: unstated park record gates nothing from AC"
    assert_eq "$(parse_dependencies "$DEMOTED$AC")" "" "daemon failure: demoted marker still gates nothing"
    STUB_PARK_OUT=''

    # No `loom:park` text at all: the daemon is not consulted.
    assert_eq "$(parse_dependencies "$AC")" "100" "no park record: checkbox dep still parsed"
    assert_eq "$(parse_dependencies $'- [ ] example-org/tool-repo#55: x')" "example-org/tool-repo#55" "cross-repo ref keeps owner prefix"

    STUB_COMMENTS='[{"body":"old"},{"body":"Restored hold.\n<!-- loom:hold-restored -->"}]'
    assert_eq "$(has_restore_note 1)" "true" "latest trusted comment restore note => skip"
    STUB_COMMENTS='[{"body":"<!-- loom:hold-restored -->"},{"body":"later comment"}]'
    assert_eq "$(has_restore_note 1)" "false" "restore note superseded by later comment"

    # #14683: drive the WHOLE unblock decision on the incident body. The gate
    # helpers are extracted verbatim alongside check_and_unblock so the run
    # exercises the same code the sweep ships.
    CU="$(awk '/^check_and_unblock\(\) \{/{f=1} f{print} /^\}/{if(f){exit}}' "$f")"
    HS="$(sed -n '/^has_superseding_block() {$/,/^}$/p' "$f")"
    HP="$(sed -n '/^has_permanent_block() {/,/^}$/p' "$f")"
    WA="$(sed -n '/^was_previously_approved() {$/,/^}$/p' "$f")"
    if [[ -n "$CU" && -n "$HS" && -n "$HP" && -n "$WA" ]]; then
        pass "extracted check_and_unblock + gate helpers verbatim"
    else
        fail "extract check_and_unblock + gate helpers from $copy"; continue
    fi
    eval "$HS"; eval "$HP"; eval "$WA"
    # post-comment.sh is invoked by path; shadow it with the recorder by
    # rewriting the call in the extracted text only (split-parent test pattern).
    CU="${CU//.\/.loom\/scripts\/post-comment.sh/post_comment}"
    eval "$CU"

    # shellcheck disable=SC2034  # consumed inside the verbatim-extracted check_and_unblock ("$GH_READ")
    GH_READ=gh
    run_unblock() {  # $1 = state of the park record's blocker #558
        DEP_558="$1"; STUB_COMMENTS='[]'
        STUB_PARK_OUT='example-org/tool-repo#558'; STUB_PARK_RC=0
        : > "$CALLS"
        check_and_unblock >/dev/null 2>&1
    }

    run_unblock OPEN   # prose #333 closed, operator blocker #558 open
    if grep -q -- '--remove-label loom:blocked' "$CALLS"; then
        fail "#14683 regression: label lifted while example-org/tool-repo#558 is open: $(cat "$CALLS")"
    else
        pass "#558 open: loom:blocked kept (park record gates, closed prose #333 ignored)"
    fi
    if grep -q 'Unblocked' "$CALLS"; then
        fail "#14683 regression: Unblocked comment posted while #558 is open"
    else
        pass "#558 open: no Unblocked comment"
    fi

    run_unblock CLOSED   # control: the park record's own blocker closed
    if grep -q -- '--remove-label loom:blocked' "$CALLS"; then
        pass "control: #558 closed lifts loom:blocked"
    else
        fail "control: label not lifted with #558 closed: $(cat "$CALLS")"
    fi
    if grep -q 'Unblocked' "$CALLS"; then
        pass "control: Unblocked comment posted"
    else
        fail "control: no Unblocked comment: $(cat "$CALLS")"
    fi
    STUB_PARK_OUT=''; STUB_PARK_RC=2
done
echo "Tests run: $TESTS_RUN"
[[ $TESTS_FAILED -eq 0 ]] && echo "All tests passed" || { echo "Tests failed: $TESTS_FAILED"; exit 1; }
