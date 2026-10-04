#!/usr/bin/env bash
# test-sweep-lease-renew-lifecycle.sh - Lifecycle tests for sweep-lease-renew.sh
# (#10229), split from test-sweep-lease-renew.sh to keep that suite under the
# file-size ratchet.
#
#   (y1) issue-state prints the explicit issue state; read failures and
#        malformed bodies are exit 1 (transient, never "completion")
#   (y2) a loop stops for good once the issue closes, with a LIVE parent, and
#        never PATCHes again
#   (y3) a transient state failure never PATCHes an unverified target, keeps the
#        loop alive, and renewal resumes when the state is readable again
#   (y4) concurrent identical starts leave ONE renewer; a different issue /
#        sweep / repo each get their own
#   (y5) release stops exactly that key's owner; a later start is a fresh
#        owner; a killed owner's record is recovered
#   (y6) per-cycle call budget: one state read + one window read + one PATCH
#
# `gh` is stubbed on PATH; no real credentials or live forge calls.

set -uo pipefail
# shellcheck source=lib/write-scope-fixture.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/write-scope-fixture.sh"

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$(cd "$TEST_DIR/.." && pwd)/sweep-lease-renew.sh"
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$1" == "$2" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $3"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $3"
        echo "    Expected: '$1'"
        echo "    Actual:   '$2'"
    fi
}

STUB_DIR="$(mktemp -d)"
trap 'jobs -p | xargs kill 2> /dev/null; rm -rf "$STUB_DIR" 2> /dev/null || true' EXIT

cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
D="${LOOM_TEST_STUB_DIR:?}"
[[ "$1" == "api" ]] || exit 3
shift
method="GET"; path=""; field_kv=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --method) method="$2"; shift 2 ;;
    --paginate) shift ;;
    -f|-F|--field|--raw-field) field_kv="$2"; shift 2 ;;
    *) [[ -n "$path" ]] || path="$1"; shift ;;
  esac
done
if [[ "$method" == "GET" && "$path" == repos/*/issues/*/comments* ]]; then
  echo "$path" >> "$D/list-calls.log"; cat "$D/comments.json"; exit 0
fi
if [[ "$method" == "GET" && "$path" == repos/*/issues/[0-9]* ]]; then
  echo "$path" >> "$D/state-calls.log"
  [[ ! -f "$D/issue-state-fail" ]] || { echo "stub gh: state read failed" >&2; exit 1; }
  [[ ! -f "$D/issue-state-malformed" ]] || { echo "not json"; exit 0; }
  echo "{\"state\":\"$(cat "$D/issue-state" 2> /dev/null || echo open)\"}"; exit 0
fi
if [[ "$method" == "PATCH" && "$path" == repos/*/issues/comments/* ]]; then
  echo "${path##*/}" >> "$D/patch-calls.log"; echo '{}'; exit 0
fi
echo "stub gh: unhandled: $method $path" >&2; exit 3
STUB
chmod +x "$STUB_DIR/gh"
cat > "$STUB_DIR/github-app-token.sh" <<'MINT'
#!/usr/bin/env bash
echo '{"status":"not_configured","message":"github app not configured"}'
MINT
chmod +x "$STUB_DIR/github-app-token.sh"

export LOOM_TEST_STUB_DIR="$STUB_DIR"
export LOOM_LEASE_RENEW_STATE_DIR="$STUB_DIR/renew-state"
export PATH="$STUB_DIR:$PATH"
# shellcheck source=lib/trust-stub.sh
source "$TEST_DIR/lib/trust-stub.sh"
loom_trust_stub "$STUB_DIR"
write_scope_register "$STUB_DIR/checkout" acme/widget
cd "$STUB_DIR/checkout" || exit 1
export LOOM_GITHUB_APP_SCRIPT="$STUB_DIR/github-app-token.sh"
unset LOOM_PERSONAL_GH_TOKEN LOOM_TERMINAL_ID LOOM_HOST_ID LOOM_LEASE_PUBLISH_HOSTNAME HOSTNAME LOOM_SWEEP_ID LOOM_ROLE 2> /dev/null || true

reset_state() {
    rm -f "$STUB_DIR"/comments.json "$STUB_DIR"/issue-state* "$STUB_DIR"/state-calls.log \
        "$STUB_DIR"/list-calls.log "$STUB_DIR"/patch-calls.log "$STUB_DIR"/y4-out.*
    rm -rf "$LOOM_LEASE_RENEW_STATE_DIR" 2> /dev/null || true
    mkdir -p "$LOOM_LEASE_RENEW_STATE_DIR"
}
run_script() {
    OUT="$("$SCRIPT" "$@" 2> "$STUB_DIR/stderr.log")"
    RC=$?
}
patch_n() { cat "$STUB_DIR/patch-calls.log" 2> /dev/null | wc -l | tr -d ' '; }
wait_patches() {
    local waited=0
    while ((waited < 20)) && [[ "$(patch_n)" -lt "$1" ]]; do
        sleep 0.5
        waited=$((waited + 1))
    done
}

echo "Testing sweep-lease-renew.sh lifecycle (#10229)..."

Y_LEASE='{"id": 42, "created_at": "2026-10-01T00:00:00Z", "body": "<!-- loom:lease host=y-host sweep=y-sweep -->\nprose"}'
alive() { kill -0 "$1" 2> /dev/null; }
yb() { if "$@"; then echo true; else echo false; fi; }

# (y1) issue-state prints the explicit state; failures / malformed are exit 1.
reset_state
echo closed > "$STUB_DIR/issue-state"
run_script issue-state 10229
assert_eq "closed" "$OUT" "(y1) issue-state prints closed"
touch "$STUB_DIR/issue-state-fail"
run_script issue-state 10229
assert_eq "1" "$RC" "(y1) an unreadable state is exit 1 (transient, not completion)"
rm -f "$STUB_DIR/issue-state-fail"
touch "$STUB_DIR/issue-state-malformed"
run_script issue-state 10229
assert_eq "1" "$RC" "(y1) a malformed state is exit 1"

# (y2) open -> closed with a LIVE parent: renews while open, then stops for good.
reset_state
echo "[$Y_LEASE]" > "$STUB_DIR/comments.json"
sleep 30 &
WATCH_Y2=$!
LOOP_Y2="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y2" --host y-host --sweep-id y-sweep 2> /dev/null)"
wait_patches 1
echo closed > "$STUB_DIR/issue-state"
sleep 3
assert_eq "false" "$(yb alive "$LOOP_Y2")" "(y2) the loop exited after the issue closed, parent still alive"
Y2_N="$(patch_n)"
sleep 2.5
assert_eq "$Y2_N" "$(patch_n)" "(y2) no PATCH after the close"
assert_eq "true" "$(yb alive "$WATCH_Y2")" "(y2) the interactive parent was untouched"
kill "$WATCH_Y2" 2> /dev/null || true
wait "$WATCH_Y2" 2> /dev/null || true

# (y3) transient state failure: no PATCH of an unverified target; the loop
# survives and resumes once the state is readable again.
reset_state
echo "[$Y_LEASE]" > "$STUB_DIR/comments.json"
touch "$STUB_DIR/issue-state-fail"
sleep 30 &
WATCH_Y3=$!
LOOP_Y3="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y3" --host y-host --sweep-id y-sweep 2> /dev/null)"
sleep 3.5
assert_eq "0" "$(patch_n)" "(y3) no PATCH while the issue state is unverifiable"
assert_eq "true" "$(yb alive "$LOOP_Y3")" "(y3) the loop survives a transient state failure"
rm -f "$STUB_DIR/issue-state-fail"
wait_patches 1
assert_eq "true" "$([[ "$(patch_n)" -ge 1 ]] && echo true || echo false)" "(y3) renewal resumes once the state is readable"
kill "$LOOP_Y3" "$WATCH_Y3" 2> /dev/null || true
wait "$WATCH_Y3" 2> /dev/null || true

# (y4) concurrent identical starts leave ONE renewer; other issue/sweep/repo
# keys are independent.
reset_state
echo "[$Y_LEASE]" > "$STUB_DIR/comments.json"
sleep 30 &
WATCH_Y4=$!
Y4_JOBS=()
for i in 1 2 3 4; do
    "$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-sweep > "$STUB_DIR/y4-out.$i" 2> /dev/null &
    Y4_JOBS+=($!)
done
wait "${Y4_JOBS[@]}" 2> /dev/null || true
Y4_LOOP="$(cat "$STUB_DIR"/y4-out.* | sort -u)"
assert_eq "1" "$(printf '%s\n' "$Y4_LOOP" | grep -c .)" "(y4) four concurrent identical starts report ONE loop pid"
OTHER_ISSUE="$("$SCRIPT" start 10230 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-sweep 2> /dev/null)"
OTHER_SWEEP="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-other 2> /dev/null)"
OTHER_REPO="$(LOOM_REPO=acme/other "$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-sweep 2> /dev/null)"
assert_eq "true" "$([[ -n "$OTHER_ISSUE" && "$OTHER_ISSUE" != "$Y4_LOOP" && -n "$OTHER_SWEEP" && "$OTHER_SWEEP" != "$Y4_LOOP" && -n "$OTHER_REPO" && "$OTHER_REPO" != "$Y4_LOOP" ]] && echo true || echo false)" "(y4) a different issue, sweep and repo each get their own loop"

# (y5) release ends exactly that key's owner, leaves peers alone; a later start
# is a fresh owner; a dead owner's record is recovered.
"$SCRIPT" release 10229 --host y-host --sweep-id y-sweep 2> /dev/null
sleep 0.5
assert_eq "false" "$(yb alive "$Y4_LOOP")" "(y5) release stopped the owner loop"
assert_eq "true" "$(if alive "$OTHER_ISSUE" && alive "$OTHER_SWEEP" && alive "$OTHER_REPO"; then echo true; else echo false; fi)" "(y5) release left every other key's loop running"
Y5_NEW="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-sweep 2> /dev/null)"
assert_eq "true" "$([[ -n "$Y5_NEW" && "$Y5_NEW" != "$Y4_LOOP" ]] && yb alive "$Y5_NEW" || echo false)" "(y5) a start after release becomes a fresh owner"
kill -9 "$Y5_NEW" 2> /dev/null || true
sleep 0.3
Y5_REC="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y4" --host y-host --sweep-id y-sweep 2> /dev/null)"
assert_eq "true" "$([[ -n "$Y5_REC" && "$Y5_REC" != "$Y5_NEW" ]] && yb alive "$Y5_REC" || echo false)" "(y5) a killed owner's record is recovered by a new start"
kill "$Y5_REC" "$OTHER_ISSUE" "$OTHER_SWEEP" "$OTHER_REPO" "$WATCH_Y4" 2> /dev/null || true
wait "$WATCH_Y4" 2> /dev/null || true

# (y6) per-cycle budget: steady state is one state read + one list/window read
# + one PATCH per interval (3 calls; 36/h at the default 300 s).
reset_state
echo "[$Y_LEASE]" > "$STUB_DIR/comments.json"
sleep 30 &
WATCH_Y6=$!
LOOP_Y6="$("$SCRIPT" start 10229 --interval 1 --watch-pid "$WATCH_Y6" --host y-host --sweep-id y-sweep 2> /dev/null)"
wait_patches 3
kill "$LOOP_Y6" "$WATCH_Y6" 2> /dev/null || true
wait "$WATCH_Y6" 2> /dev/null || true
Y6_P="$(patch_n)"
Y6_S="$(cat "$STUB_DIR/state-calls.log" 2> /dev/null | wc -l | tr -d ' ')"
Y6_L="$(cat "$STUB_DIR/list-calls.log" 2> /dev/null | wc -l | tr -d ' ')"
assert_eq "true" "$([[ "$Y6_P" -ge 3 && "$Y6_S" -ge "$Y6_P" && "$Y6_S" -le $((Y6_P + 1)) && "$Y6_L" -ge "$Y6_P" && "$Y6_L" -le $((Y6_P + 1)) ]] && echo true || echo false)" "(y6) one state read, one list read, one PATCH per cycle (p=$Y6_P s=$Y6_S l=$Y6_L)"

echo ""
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
if ((TESTS_FAILED > 0)); then
    echo -e "${RED}FAILED${NC}: $TESTS_FAILED test(s) failed"
    exit 1
fi
echo -e "${GREEN}ALL PASSED${NC}"
exit 0
