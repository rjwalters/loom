#!/usr/bin/env bash
# test-sweep-lease-renew-fd-hygiene.sh - the detached renewal loop that
# `sweep-lease-renew.sh start` forks must not hold the caller's fds (#10203).
#
# `worktree.sh N | tail` hung for hours: worktree.sh keeps its caller's stdout
# on fd 3, and the loop's `< /dev/null > /dev/null 2>&1` covered fds 0-2 only,
# so the loop and its `sleep` children kept the pipe open until the 4h cap.
#
# Covers:
#   (1) a pipe handed to `start` on fds 3 and 7 closes as soon as `start`
#       returns -- pre-fix, `cat` waits out the loop's first 8s sleep
#   (2) the loop is still running once the pipe has closed
#   (3) none of fds 3-8 is open in the loop (fd 9 is its own log)
#
# Bounded: a regression fails in ~8s instead of hanging. The watched PID dies
# (4s) before the loop's first wake-up (8s), so the loop exits without ever
# renewing: no `gh` call is made and no stub is needed. Kept apart from
# test-sweep-lease-renew.sh, which sits at the file-size threshold
# (.loom/docs/file-size-policy.md).
#
# Usage:
#   ./.loom/scripts/tests/test-sweep-lease-renew-fd-hygiene.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$(cd "$TEST_DIR/.." && pwd)/sweep-lease-renew.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
TESTS_RUN=0
TESTS_FAILED=0
check() {
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$1" == "true" ]]; then
        echo -e "  ${GREEN}PASS${NC}: $2"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $2"
    fi
}

LOG="$(mktemp)"
sleep 4 &
WATCH_PID=$!
cleanup() {
    kill "$WATCH_PID" 2> /dev/null || true
    # The loop's `sleep` child outlives a killed loop; reap it too.
    [[ "${LOOP_PID:-}" =~ ^[0-9]+$ ]] && { pkill -P "$LOOP_PID" 2> /dev/null || true; }
    [[ "${LOOP_PID:-}" =~ ^[0-9]+$ ]] && { kill "$LOOP_PID" 2> /dev/null || true; }
    rm -f "$LOG"
}
trap cleanup EXIT

T0="$(date +%s)"
LOOP_PID="$({ LOOM_TERMINAL_ID='' "$SCRIPT" start 10203 --interval 8 --watch-pid "$WATCH_PID" 2> "$LOG" 3>&1 7>&1; } | cat)"
ELAPSED=$(($(date +%s) - T0))

check "$([[ "$ELAPSED" -lt 4 ]] && echo true || echo false)" \
    "(1) a pipe handed to start on fds 3/7 closes when start returns (took ${ELAPSED}s)"
LOOP_ALIVE=false
[[ "$LOOP_PID" =~ ^[0-9]+$ ]] && kill -0 "$LOOP_PID" 2> /dev/null && LOOP_ALIVE=true
check "$LOOP_ALIVE" "(2) the renewal loop is still running after the pipe closed (pid '${LOOP_PID}')"

HELD=""
if [[ "$LOOP_ALIVE" == true && -d "/proc/$LOOP_PID/fd" ]]; then
    for fd in 3 4 5 6 7 8; do [[ -e "/proc/$LOOP_PID/fd/$fd" ]] && HELD+="$fd "; done
elif [[ "$LOOP_ALIVE" == true ]] && command -v lsof > /dev/null 2>&1; then
    HELD="$(lsof -a -p "$LOOP_PID" -d 3-8 -F f 2> /dev/null | sed -n 's/^f\([0-9][0-9]*\)$/\1/p' | tr '\n' ' ')"
fi
check "$([[ "$LOOP_ALIVE" == true && -z "$HELD" ]] && echo true || echo false)" \
    "(3) the loop holds none of fds 3-8 (held: '${HELD}')"

echo ""
echo "Results: $((TESTS_RUN - TESTS_FAILED))/$TESTS_RUN passed"
if ((TESTS_FAILED > 0)); then
    echo -e "${RED}FAILED${NC}: $TESTS_FAILED test(s) failed"
    exit 1
fi
echo -e "${GREEN}ALL PASSED${NC}"
