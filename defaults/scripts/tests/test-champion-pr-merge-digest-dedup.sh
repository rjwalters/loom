#!/usr/bin/env bash
# test-champion-pr-merge-digest-dedup.sh - #13876: concurrent Champion passes
# can both find the merge-risk-hold digest issue absent and both create one;
# Step 0's selection is deterministic, so the higher-numbered duplicate is a
# write-frozen loser that no pass ever reads again.
#
# Extract-and-execute (#7979), not a mirror: the SHIPPED bash blocks are lifted
# out of champion-pr-merge.md and run against canned forge JSON with `gh`,
# `$GH_READ`, `post-comment.sh` and `create-issue.sh` stubbed. Blocks run:
#   1. the Per-PR Digest Step 0 block (locate + duplicate reconcile) -> the
#      loser is pointed at the winner once, unpinned and closed; marker-less
#      matches are never touched; the #7338 marker-less fallback still selects;
#   2. the Per-PR Digest Step 2 block (write/create) -> the pre-create
#      UNCACHED recheck adopts a concurrently-created digest instead of
#      creating a second one.
#
# Hermetic: no network, no forge, no tokens.
#
# Usage:
#   ./.loom/scripts/tests/test-champion-pr-merge-digest-dedup.sh

# SC2034: the HELD_*/DIGEST_* fixtures are read by the eval'd shipped blocks,
# which shellcheck cannot see. SC2317: run_step0/run_step2 are invoked via
# command substitution the checker cannot trace.
# shellcheck disable=SC2034,SC2317

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
# Installed (.loom/scripts -> ../../.claude) vs. source (defaults/scripts ->
# ../.claude) layouts differ in depth; probe both (#6725).
if [[ -d "$SCRIPTS_DIR/../../.claude/commands/loom" ]]; then
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../../.claude/commands/loom" && pwd)"
else
    PROMPT_DIR="$(cd "$SCRIPTS_DIR/../.claude/commands/loom" && pwd)"
fi
CHAMPION_MD="$PROMPT_DIR/champion-pr-merge.md"

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Local assert_contains kept: uses grep -qF matching; shared helper uses a bash substring match.
assert_contains() {
    if grep -qF -- "$2" <<<"$1"; then pass "$3"; else fail "$3 (missing '$2' in: $1)"; fi
}
assert_lacks() {
    if grep -qF -- "$2" <<<"$1"; then fail "$3 (unexpected '$2' in: $1)"; else pass "$3"; fi
}
# Local assert_doc_contains kept: fixed $CHAMPION_MD target, so (needle, msg) not (file, needle, msg).
assert_doc_contains() {
    if grep -qF -- "$1" "$CHAMPION_MD"; then pass "$2"; else fail "$2 (missing literal: $1)"; fi
}

# Print the first ```bash block that follows the first line containing $1.
extract_block() {
    awk -v m="$1" '
        f == 0 && index($0, m) { f = 1; next }
        f == 1 && /^```bash$/  { f = 2; next }
        f == 2 && /^```$/      { exit }
        f == 2                 { print }
    ' "$CHAMPION_MD"
}

STEP0="$(extract_block '**Step 0 — locate the existing digest issue')"
STEP2="$(extract_block '**Step 2 — write the digest to a durable tracking issue')"

WORK="$(mktemp -d)" && trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/.loom/scripts"
printf '%s\n' '#!/usr/bin/env bash' 'echo "POST $*" >>"$STUB_LOG"' >"$WORK/.loom/scripts/post-comment.sh"
printf '%s\n' '#!/usr/bin/env bash' 'echo "CREATE $*" >>"$STUB_LOG"' \
    'echo "https://github.com/rjwalters/loom/issues/9999"' >"$WORK/.loom/scripts/create-issue.sh"
chmod +x "$WORK/.loom/scripts/post-comment.sh" "$WORK/.loom/scripts/create-issue.sh"

# ---- forge stubs ------------------------------------------------------------
# FIX_LIST    the $GH_READ `issue list` payload Step 0 reads (cached side)
# FIX_VIEW    per-issue `issue view` payloads, keyed by number
# GH_LIST     the plain-gh `issue list` payload Step 2's recheck reads
#             (uncached side — the concurrent pass's create must appear HERE)
# GH_UNPIN_FAIL=1 makes `gh issue unpin` fail once (best-effort edge).
declare -A FIX_VIEW
gh() {
    local filter="" a
    local -a args=("$@")
    for ((a = 0; a < ${#args[@]}; a++)); do
        [[ "${args[a]}" == "--jq" ]] && filter="${args[a + 1]}"
    done
    case "$1 $2" in
        "issue list")
            # Step 2's recheck filters server-side like the real gh.
            if [[ -n "$filter" ]]; then jq -r "$filter" <<<"$GH_LIST"; else printf '%s\n' "$GH_LIST"; fi
            ;;
        "issue edit") echo "EDIT $*" >>"$STUB_LOG" ;;
        "issue pin") echo "PIN $*" >>"$STUB_LOG" ;;
        "issue unpin")
            if [[ "${GH_UNPIN_FAIL:-0}" == 1 ]]; then GH_UNPIN_FAIL=0; return 1; fi
            echo "UNPIN $*" >>"$STUB_LOG"
            ;;
        "issue close") echo "CLOSE $*" >>"$STUB_LOG" ;;
        "repo view") echo "rjwalters/loom" ;;
        *) echo "unexpected gh call: $*" >&2; return 99 ;;
    esac
}
gh_read() {
    [[ "$1" == "--clear-cache" ]] && return 0
    local filter="" payload
    local -a args=("$@")
    for ((a = 0; a < ${#args[@]}; a++)); do
        [[ "${args[a]}" == "--jq" ]] && filter="${args[a + 1]}"
    done
    case "$1 $2" in
        "issue list")
            if [[ -n "$filter" ]]; then jq -r "$filter" <<<"$FIX_LIST"; else printf '%s\n' "$FIX_LIST"; fi
            ;;
        "issue view")
            # NB: no brace-bearing :- default here — inside ${var:-word} the
            # default's own '}' would terminate the expansion and append a
            # stray '}' to the payload.
            payload="${FIX_VIEW[$3]-}"
            [[ -z "$payload" ]] && payload='{"comments": []}'
            jq -r "$filter" <<<"$payload"
            ;;
        *) echo "unexpected GH_READ call: $*" >&2; return 99 ;;
    esac
}
export -f gh gh_read

MARK='<!-- champion:merge-risk-hold-digest -->'
tagged() { # <number>
    jq -n --argjson n "$1" --arg m "$MARK" '{number: $n, body: ($m + "\n# Merge-Risk Hold Digest\n\nold body")}';
}
markerless() { # <number>
    jq -n --argjson n "$1" '{number: $n, body: "# Merge-Risk Hold Digest\n\npredates the marker convention"}';
}

# Run the extracted Step 0 block once; print the outcome variables and the stub log.
run_step0() {
    (
        cd "$WORK" || exit 1
        export STUB_LOG="$WORK/log"; : >"$STUB_LOG"
        GH_READ=gh_read
        eval "$STEP0" >/dev/null 2>&1
        echo "DIGEST_ISSUE=$DIGEST_ISSUE WINNER=${DIGEST_WINNER:-}"
        cat "$STUB_LOG"
    )
}

# Run the extracted Step 2 block once with $1 as the Step 0 DIGEST_ISSUE value.
run_step2() {
    (
        cd "$WORK" || exit 1
        export STUB_LOG="$WORK/log"; : >"$STUB_LOG"
        GH_READ=gh_read
        # Step 1/1b products the block renders into the digest body.
        export HELD_STRUCTURAL_UNAVAILABLE=0 HELD_COUNT=0 HELD_CONFLICTING=0 HELD_ROTTING=0 \
            HELD_CONFLICTING_CLEAN=0 HELD_AT_DOCTOR=0 OLDEST_DAYS=0 HELD_NOTE="" RELEASED="" \
            ROT_THRESHOLD_DAYS=3 CONFLICT_SINCE_MARKERS="" DIGEST_ROWS="" \
            DIGEST_TITLE="Champion: Merge-Risk Hold Digest" DIGEST_MARKER="$MARK"
        DIGEST_ISSUE="$1"
        eval "$STEP2" >/dev/null 2>&1
        echo "DIGEST_ISSUE=$DIGEST_ISSUE"
        cat "$STUB_LOG"
    )
}

echo "=== test-champion-pr-merge-digest-dedup.sh ==="
echo

echo "Test 0: the shipped blocks are extractable (this suite RUNS them, it does not mirror them)"
assert_contains "$STEP0" 'DIGEST_LOSER' "Step 0 reconcile loop extracted"
assert_contains "$STEP0" 'champion:digest-duplicate-of' "pointer marker extracted"
assert_contains "$STEP2" 'gh issue list' "Step 2 pre-create recheck extracted"
assert_doc_contains 'UNCACHED (gh, not $GH_READ — Step 0'"'"'s read may be a' \
    "the recheck is documented as uncached, not a cache hit"
assert_doc_contains '**singleton state** — exactly one open digest per install' "the singleton-state rule is stated"
assert_doc_contains 'bookkeeping-only exception to Champion'"'"'s close authority' \
    "the reconcile is scoped as Champion's only close-authority exception"
echo

echo "Test 1: two tagged matches -> loser pointed at the winner once, unpinned, closed; winner untouched"
FIX_LIST="[$(tagged 100), $(tagged 107)]"
FIX_VIEW[100]=$(jq -n --arg m "$MARK" '{"body": ($m + "\nold body")}')
FIX_VIEW[107]='{"comments": []}'
OUT=$(run_step0)
assert_contains "$OUT" "DIGEST_ISSUE=100 WINNER=100" "min_by(.number) selects the winner"
assert_contains "$OUT" "POST 107 --body <!-- champion:digest-duplicate-of:100 -->" "loser gets the one-time pointer at the winner"
assert_contains "$OUT" "UNPIN issue unpin 107" "loser unpinned"
assert_contains "$OUT" "CLOSE issue close 107 --reason not planned" "loser closed not planned"
assert_lacks "$OUT" "POST 100 " "winner: no comment"
assert_lacks "$OUT" "UNPIN issue unpin 100" "winner: not unpinned"
assert_lacks "$OUT" "CLOSE issue close 100" "winner: not closed"
echo

echo "Test 2: one tagged + one marker-less (lower number) -> nothing closed, marker still wins (#7338)"
FIX_LIST="[$(markerless 95), $(tagged 100)]"
OUT=$(run_step0)
assert_contains "$OUT" "DIGEST_ISSUE=100 WINNER=100" "marker-tagged match beats the lower-numbered marker-less one"
assert_lacks "$OUT" "CLOSE" "marker-less match never closed"
assert_lacks "$OUT" "UNPIN" "marker-less match never unpinned"
assert_lacks "$OUT" "POST" "marker-less match never commented"
echo

echo "Test 2b: marker-less only -> the #7338 fallback still selects it, no reconcile runs"
FIX_LIST="[$(markerless 95)]"
OUT=$(run_step0)
assert_contains "$OUT" "DIGEST_ISSUE=95 WINNER=" "fallback to oldest title match unchanged"
assert_lacks "$OUT" "CLOSE" "nothing to reconcile"
echo

echo "Test 3: loser already carries the pointer -> not re-commented, still unpinned and closed"
FIX_LIST="[$(tagged 100), $(tagged 107)]"
FIX_VIEW[100]=$(jq -n --arg m "$MARK" '{"body": ($m + "\nold body")}')
FIX_VIEW[107]='{"comments": [{"body": "<!-- champion:digest-duplicate-of:100 -->\n dup"}]}'
OUT=$(run_step0)
assert_lacks "$OUT" "POST" "marker guard suppresses the repeat pointer"
assert_contains "$OUT" "UNPIN issue unpin 107" "loser still unpinned"
assert_contains "$OUT" "CLOSE issue close 107 --reason not planned" "loser still closed"
echo

echo "Test 4: unpin fails -> best-effort, the close still happens"
FIX_LIST="[$(tagged 100), $(tagged 107)]"
FIX_VIEW[100]=$(jq -n --arg m "$MARK" '{"body": ($m + "\nold body")}')
FIX_VIEW[107]='{"comments": []}'
GH_UNPIN_FAIL=1
OUT=$(run_step0)
assert_contains "$OUT" "CLOSE issue close 107 --reason not planned" "a failed unpin does not abort the reconcile"
GH_UNPIN_FAIL=0
echo

echo "Test 5: Step 2 recheck finds a concurrently-created digest -> edit branch, no create"
GH_LIST="[$(tagged 200)]"
OUT=$(run_step2 "")
assert_contains "$OUT" "DIGEST_ISSUE=200" "the recheck adopts the winner of the create race"
assert_contains "$OUT" "EDIT issue edit 200 --body" "existing digest edited in place"
assert_contains "$OUT" "PIN issue pin 200" "adopted digest pinned"
assert_lacks "$OUT" "CREATE" "no second digest created"
echo

echo "Test 6: Step 2 recheck finds nothing -> create path still works"
GH_LIST='[]'
OUT=$(run_step2 "")
assert_contains "$OUT" "DIGEST_ISSUE=9999" "create-issue.sh result adopted"
assert_contains "$OUT" "CREATE --title Champion: Merge-Risk Hold Digest" "digest created under the singleton title"
assert_contains "$OUT" "--label loom:blocked" "created out of every role's work queue"
assert_contains "$OUT" "PIN issue pin 9999" "new digest pinned"
assert_lacks "$OUT" "EDIT" "no edit attempted against a nonexistent issue"
echo

echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
