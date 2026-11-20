#!/usr/bin/env bash
# test-guide-unblock-open-crossrepo.sh - Regression test for issue #14710
#
# The Fleet Guide removed `loom:blocked` from a parked issue whose park record
# named a still-OPEN cross-repository draft PR (2AMLogic/.loom#630): the
# inline per-dependency `gh issue view ... --json state` read in
# check_and_unblock reported it resolved. Dependency resolution now goes
# through ONE daemon-side resolver — `loom-daemon dep-recheck-fingerprint
# dep-states`, binding-verified against the qualified OWNER/REPO#N and
# fail-closed — via guide.md's `resolve_dep_states`, shared by
# check_and_unblock (guide.md) and check_and_unblock_prs
# (defaults/docs/park-record.md).
#
# This suite extracts the prompt-side functions VERBATIM from each shipped
# copy (guide.md and the loom-guide SKILL.md) and drives them with stubs:
#
#   1. resolve_dep_states against a stubbed loom-daemon: a false verdict, an
#      unreadable daemon (exit 1), an old binary without the subcommand
#      (exit 2), garbage output, and the positive case whose quoted RESOLVED
#      list must survive the documented eval consumer.
#   2. the WHOLE check_and_unblock against a stubbed gh + loom-daemon with
#      the #14710 fixture (two terminal deps + one open cross-repo draft PR):
#      no `gh issue edit`, no unblock comment. The positive case (all
#      terminal) still unblocks, and a trusted hold-restore note still skips.
#   3. static pins: check_and_unblock delegates to resolve_dep_states and no
#      longer carries the inline dep-state read; park-record.md's PR-side
#      check_and_unblock_prs does the same.
#
# The daemon side of the contract (binding verification, the OPEN/UNKNOWN
# split, MERGED/CLOSED acceptance — the #10040 behavior the deleted
# test-guide-unblock-merged-blocker.sh used to pin at this boundary) is
# driven against the real binary in test-dep-recheck-fingerprint.sh's
# dep-states section.
#
# Usage:
#   ./.loom/scripts/tests/test-guide-unblock-open-crossrepo.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
TESTS_RUN=0
TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }
assert_eq() {
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (got '$1', expected '$2')"; fi
}

command -v jq >/dev/null 2>&1 || { echo "FATAL: jq required"; exit 2; }

# The #14710 incident shape, in neutral fixtures: a parked issue blocked on
# three qualified deps, one of which (the draft PR #630) is still OPEN while
# a same-numbered CLOSED issue exists in a different repo — the exact wrong
# answer the old inline read could not distinguish.
DRAFT_DEP="example-org/other#630"
PARK_DEPS="$(printf '%s\n%s\n%s' "$DRAFT_DEP" 'example-org/tool-repo#575' 'example-org/loom-mirror#13762')"

# =====================================================================
# Part A: resolve_dep_states (stubbed loom-daemon)
# =====================================================================

# The stub dispatches on its arguments (the same shape as
# test-guide-park-record-precedence.sh's stub):
#   dep-recheck-fingerprint dep-states -> $STUB_DEPSTATES, exit $STUB_DEPSTATES_RC
#   park-record parse                  -> stdin drained, $STUB_PARK_OUT, exit $STUB_PARK_RC
#   forge trusted-comments             -> $STUB_COMMENTS
#   forge parent split-state           -> exit 1 (NOT_SPLIT)
#   anything else                      -> exit 2, loudly.
STUB_DEPSTATES=''; STUB_DEPSTATES_RC=0
STUB_PARK_OUT=''; STUB_PARK_RC=0
STUB_COMMENTS='[]'
loom-daemon() {
    case "$1 ${2:-}" in
        "dep-recheck-fingerprint dep-states")
            [[ -z "$STUB_DEPSTATES" ]] || printf '%s\n' "$STUB_DEPSTATES"
            return "$STUB_DEPSTATES_RC" ;;
        "park-record parse")
            cat >/dev/null
            [[ -z "$STUB_PARK_OUT" ]] || printf '%s\n' "$STUB_PARK_OUT"
            return "$STUB_PARK_RC" ;;
        "forge trusted-comments") printf '%s\n' "$STUB_COMMENTS" ;;
        "forge parent"*) echo "NOT_SPLIT"; return 1 ;;
        *) echo "stub loom-daemon: unexpected call: $*" >&2; return 2 ;;
    esac
}

for copy in \
    "defaults/.claude/commands/loom/guide.md" \
    "defaults/.agents/skills/loom-guide/SKILL.md"; do
    f="$REPO_ROOT/$copy"
    echo "Copy: $copy"
    [[ -f "$f" ]] || { fail "$copy exists"; continue; }

    RDS="$(sed -n '/^resolve_dep_states() {$/,/^}$/p' "$f")"
    CBU="$(sed -n '/^check_and_unblock() {$/,/^}$/p' "$f")"
    [[ -n "$RDS" ]] || { fail "extract resolve_dep_states from $copy"; continue; }
    [[ -n "$CBU" ]] || { fail "extract check_and_unblock from $copy"; continue; }
    eval "$RDS"

    # Static pins: the delegation exists and the inline dep-state read is gone.
    # shellcheck disable=SC2016  # literal prompt text
    if grep -q 'resolve_dep_states "$deps"' <<<"$CBU"; then
        pass "check_and_unblock delegates to resolve_dep_states"
    else
        fail "check_and_unblock delegates to resolve_dep_states"
    fi
    # shellcheck disable=SC2016  # literal prompt text
    if grep -q 'gh issue view "$dn"' <<<"$CBU"; then
        fail "check_and_unblock no longer carries the inline gh issue view dep read (#14710)"
    else
        pass "check_and_unblock no longer carries the inline gh issue view dep read (#14710)"
    fi

    # A: not all terminal (the open draft) -> false.
    STUB_DEPSTATES="ALL_RESOLVED=false
RESOLVED=''
OPEN='$DRAFT_DEP'
UNKNOWN=''"; STUB_DEPSTATES_RC=0
    assert_eq "$(resolve_dep_states "$PARK_DEPS" | sed -n '1p')" "false" \
        "open cross-repo draft among the deps answers false (no unblock)"

    # B: unreadable daemon (exit 1, the live-read fail-safe code) -> false.
    STUB_DEPSTATES=''; STUB_DEPSTATES_RC=1
    assert_eq "$(resolve_dep_states "$PARK_DEPS" | sed -n '1p')" "false" \
        "unreadable daemon read answers false (hold preserved)"

    # C: old binary without the subcommand (clap exit 2) -> false.
    STUB_DEPSTATES=''; STUB_DEPSTATES_RC=2
    assert_eq "$(resolve_dep_states "$PARK_DEPS" | sed -n '1p')" "false" \
        "old binary without dep-states answers false (hold preserved)"

    # D: garbage output with exit 0 -> false, and nothing executed.
    STUB_DEPSTATES='something broke upstream'; STUB_DEPSTATES_RC=0
    assert_eq "$(resolve_dep_states "$PARK_DEPS" | sed -n '1p')" "false" \
        "garbage daemon output answers false"

    # E: all terminal -> true, and the eval-quoted RESOLVED list round-trips
    # (the daemon shell_quotes multi-ref values; the announcement reads the
    # value, not the quoting).
    STUB_DEPSTATES="ALL_RESOLVED=true
RESOLVED='example-org/tool-repo#575 $DRAFT_DEP example-org/loom-mirror#13762'
OPEN=''
UNKNOWN=''"; STUB_DEPSTATES_RC=0
    verdict="$(resolve_dep_states "$PARK_DEPS")"
    assert_eq "$(sed -n '1p' <<<"$verdict")" "true" \
        "all-terminal deps answer true (legitimate unblocks still proceed)"
    assert_eq "$(sed -n '2p' <<<"$verdict")" "example-org/tool-repo#575 $DRAFT_DEP example-org/loom-mirror#13762" \
        "resolved list round-trips through the eval consumer for the announcement"
    STUB_DEPSTATES=''; STUB_DEPSTATES_RC=0
done

# park-record.md's PR side: same delegation, same removed ladder.
PRF="$REPO_ROOT/defaults/docs/park-record.md"
echo "Copy: defaults/docs/park-record.md"
if [[ -f "$PRF" ]]; then
    CBUP="$(sed -n '/^check_and_unblock_prs() {$/,/^}$/p' "$PRF")"
    # shellcheck disable=SC2016  # literal prompt text
    if [[ -n "$CBUP" ]] && grep -q 'resolve_dep_states "$deps"' <<<"$CBUP"; then
        pass "check_and_unblock_prs delegates to the shared resolver"
    else
        fail "check_and_unblock_prs delegates to the shared resolver"
    fi
    # shellcheck disable=SC2016  # literal prompt text
    if grep -q 'gh issue view "$dn"' <<<"$CBUP"; then
        fail "check_and_unblock_prs no longer carries the inline gh issue view dep read (#14710)"
    else
        pass "check_and_unblock_prs no longer carries the inline gh issue view dep read (#14710)"
    fi
else
    fail "defaults/docs/park-record.md exists"
fi

# =====================================================================
# Part B: the whole check_and_unblock against stubbed gh + loom-daemon
# =====================================================================

TMP="$(mktemp -d)"
WRITES="$TMP/writes.log"
COMMENTS="$TMP/comments.log"
: >"$WRITES"; : >"$COMMENTS"

# check_and_unblock invokes post-comment.sh BY RELATIVE PATH, so run from a
# temp cwd whose ./.loom/scripts/post-comment.sh records instead of posting.
mkdir -p "$TMP/.loom/scripts"
cat >"$TMP/.loom/scripts/post-comment.sh" <<'REC'
#!/usr/bin/env bash
echo "post-comment $*" >>"$COMMENTS_FILE"
exit 0
REC
chmod +x "$TMP/.loom/scripts/post-comment.sh"
export COMMENTS_FILE="$COMMENTS"

# gh stub: the blocked listing, the linked-PR read, and every WRITE recorded.
gh() {
    case "${1:-}" in
        issue)
            case "$2" in
                list) printf '%s\n' "$STUB_BLOCKED_JSON" ;;
                view) printf '%s\n' '{"closedByPullRequestsReferences":[]}' ;;
                edit) echo "gh issue edit $*" >>"$WRITES_FILE"; return 0 ;;
                *) echo "stub gh: unhandled issue sub '$2'" >&2; return 1 ;;
            esac ;;
        api) return 0 ;;  # was_previously_approved: empty history -> false
        *) echo "stub gh: unhandled args: $*" >&2; return 1 ;;
    esac
}

# The parked issue, #14710 shape: park record naming the three qualified deps.
STUB_BLOCKED_JSON="$(jq -cn --arg park "$PARK_DEPS" \
    '[{number: 9001, title: "ratification rollout", labels: [{name: "loom:blocked"}],
       body: ("<!-- loom:park Blocked by: " + $park + " by=builder at=x reason=\"upstream\" -->")}]')"

# Tail functions check_and_unblock calls but guide.md defines elsewhere (or
# not at all): no-ops for this fixture.
check_and_unblock_prs() { :; }
convert_awaiting_revision_parks() { :; }
release_due_transient_parks() { :; }

for copy in \
    "defaults/.claude/commands/loom/guide.md" \
    "defaults/.agents/skills/loom-guide/SKILL.md"; do
    f="$REPO_ROOT/$copy"
    echo "Copy (full drive): $copy"
    [[ -f "$f" ]] || { fail "$copy exists (full drive)"; continue; }

    for fn in parse_dependencies was_previously_approved has_superseding_block \
        has_permanent_block has_restore_note resolve_dep_states check_and_unblock; do
        # `has_permanent_block() {` carries a trailing comment in the source,
        # so match the signature prefix rather than the full line.
        body="$(sed -n "/^${fn}() {/,/^\}$/p" "$f")"
        [[ -n "$body" ]] || { fail "extract $fn from $copy (full drive)"; continue 2; }
        eval "$body"
    done

    cd "$TMP" || { fail "enter temp cwd"; continue; }
    # Used by the eval'd check_and_unblock body ("$GH_READ"), which the
    # linter cannot see — hence the export.
    export GH_READ=gh
    export WRITES_FILE="$WRITES"

    # F: the #14710 fixture — two terminal deps + one open cross-repo draft
    # PR: NO label edit, NO unblock comment, NO success announcement.
    STUB_PARK_OUT="$PARK_DEPS"; STUB_PARK_RC=0
    STUB_COMMENTS='[]'
    STUB_DEPSTATES="ALL_RESOLVED=false
RESOLVED=''
OPEN='$DRAFT_DEP'
UNKNOWN=''"; STUB_DEPSTATES_RC=0
    : >"$WRITES"; : >"$COMMENTS"
    out="$(check_and_unblock 2>/dev/null)"
    assert_eq "0" "$(wc -l <"$WRITES" | tr -d ' ')" \
        "open cross-repo draft: no gh issue edit (label write) is made"
    assert_eq "0" "$(wc -l <"$COMMENTS" | tr -d ' ')" \
        "open cross-repo draft: no unblock comment is posted"
    if grep -q "Unblocked" <<<"$out"; then
        fail "open cross-repo draft: no success announcement on stdout"
    else
        pass "open cross-repo draft: no success announcement on stdout"
    fi

    # G: positive — all deps terminal: the unblock write and comment happen
    # (pre-approval path: clear loom:blocked, no loom:issue restore).
    STUB_DEPSTATES="ALL_RESOLVED=true
RESOLVED='example-org/tool-repo#575 $DRAFT_DEP example-org/loom-mirror#13762'
OPEN=''
UNKNOWN=''"; STUB_DEPSTATES_RC=0
    : >"$WRITES"; : >"$COMMENTS"
    out="$(check_and_unblock 2>/dev/null)"
    assert_eq "1" "$(wc -l <"$WRITES" | tr -d ' ')" \
        "all-terminal deps: exactly one label edit is made"
    if grep -q -- "--remove-label loom:blocked" "$WRITES" && ! grep -q -- "--add-label loom:issue" "$WRITES"; then
        pass "all-terminal deps: hold cleared without promoting a pre-approval issue"
    else
        fail "all-terminal deps: hold cleared without promoting a pre-approval issue (writes: $(cat "$WRITES"))"
    fi
    assert_eq "1" "$(wc -l <"$COMMENTS" | tr -d ' ')" \
        "all-terminal deps: the unblock announcement is posted"

    # H: a trusted hold-restore note (#13414) still skips even when every
    # dependency reads terminal — trusted operator holds are untouched.
    STUB_COMMENTS='[{"body":"Restored hold.\n<!-- loom:hold-restored -->"}]'
    : >"$WRITES"; : >"$COMMENTS"
    check_and_unblock >/dev/null 2>&1
    assert_eq "0" "$(wc -l <"$WRITES" | tr -d ' ')" \
        "trusted hold-restore note skips the unblock (operator holds preserved)"
    STUB_COMMENTS='[]'

    # I: daemon unreadable (exit 1) — the whole sweep still writes nothing.
    STUB_DEPSTATES=''; STUB_DEPSTATES_RC=1
    : >"$WRITES"; : >"$COMMENTS"
    check_and_unblock >/dev/null 2>&1
    assert_eq "0" "$(wc -l <"$WRITES" | tr -d ' ')" \
        "unreadable daemon read: no label edit anywhere in the sweep"
done

cd "$REPO_ROOT" || true
rm -rf "$TMP"

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
fi
echo "All tests passed"
