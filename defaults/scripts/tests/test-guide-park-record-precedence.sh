#!/usr/bin/env bash
# test-guide-park-record-precedence.sh - Regression test for issue #13414
#
# Guide's unblock sweep lifted operator-ruled parks because unchecked
# acceptance-criterion `- [ ] #N` lines counted as gates. A park record now wins.
# parse_dependencies / has_restore_note are extracted VERBATIM from each shipped
# copy and run against fixtures (and a stubbed loom-daemon).
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo "  PASS: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo "  FAIL: $1"; }
assert_eq() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (got '$1', expected '$2')"; fi; }

# The stub dispatches on its arguments (#13895): one catch-all answer made the
# `park-record parse` call in parse_dependencies read has_restore_note's `[]`.
#   forge trusted-comments  -> $STUB_COMMENTS
#   park-record parse       -> $STUB_PARK_OUT, exit $STUB_PARK_RC, following the
#                              real contract: 0 = a live record exists (stdout =
#                              its blockers, one per line, possibly none),
#                              1 = no live record, anything else = daemon failure.
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
        *) echo "stub loom-daemon: unexpected call: $*" >&2; return 2 ;;
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
    STUB_PARK_OUT=$'200\nexample-org/tool-repo#7'
    assert_eq "$(parse_dependencies "$AC$PARK")" $'200\nexample-org/tool-repo#7' "daemon blocker list passed through, cross-repo ref intact"
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
done
echo "Tests run: $TESTS_RUN"
[[ $TESTS_FAILED -eq 0 ]] && echo "All tests passed" || { echo "Tests failed: $TESTS_FAILED"; exit 1; }
