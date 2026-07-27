#!/usr/bin/env bash
# test-loom-daemon-stop.sh — Tests for loom-daemon-stop.sh, including the
# macOS launchd bootout counterpart added by #3972.
#
# Every invocation below pins LOOM_LAUNCHD_LABEL to a random, guaranteed-
# nonexistent label. This is deliberate and load-bearing: it ensures
# `launchd_job_loaded` always resolves false (no loaded job with that label)
# so these tests can NEVER observe or mutate the real machine's
# ~/Library/LaunchAgents/com.rjwalters.loom-daemon.plist, on this dev box or
# any CI runner.
#
# Style matches test-loom-daemon-start.sh — plain bash, hand-rolled
# assertions. Bats is NOT used in this repository.
#
# Usage:
#   ./defaults/scripts/tests/test-loom-daemon-stop.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STOP_SCRIPT="$(cd "$SCRIPT_DIR/../cli" && pwd)/loom-daemon-stop.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
        echo "  expected: [$expected]"
        echo "  actual:   [$actual]"
    fi
}

# A guaranteed-nonexistent label so `launchctl print` never matches a real
# loaded job on the host machine, regardless of platform.
FAKE_LABEL="com.example.loom-daemon-stop-test-$$-$RANDOM"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/.loom"

# ---------- tests ----------

# 1. No PID file, no running process -> "nothing to stop", exits 0.
out=$( cd "$WORKDIR" && LOOM_LAUNCHD_LABEL="$FAKE_LABEL" bash "$STOP_SCRIPT" 2>&1 )
rc=$?
assert_eq "0" "$rc" "no daemon running: exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out" | grep -qi "nothing to stop"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} no daemon running: reports 'nothing to stop'"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} no daemon running: reports 'nothing to stop'"
fi

# 2. A live PID-file-tracked process is stopped (SIGTERM path).
SLEEP_PID_FILE="$WORKDIR/.loom/.daemon.pid"
( sleep 30 & echo $! > "$SLEEP_PID_FILE" )
sleep_pid=$(cat "$SLEEP_PID_FILE")
( cd "$WORKDIR" && LOOM_LAUNCHD_LABEL="$FAKE_LABEL" LOOM_DAEMON_STOP_GRACE_SECS=2 bash "$STOP_SCRIPT" >/dev/null 2>&1 )
rc2=$?
assert_eq "0" "$rc2" "live pid: stop exits 0"
TESTS_RUN=$((TESTS_RUN + 1))
if ! kill -0 "$sleep_pid" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} live pid: process is actually killed"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} live pid: process is actually killed"
    kill -9 "$sleep_pid" 2>/dev/null || true
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ ! -f "$SLEEP_PID_FILE" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} live pid: PID file removed after stop"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} live pid: PID file removed after stop"
fi

# 3. --force skips the grace window (SIGKILL immediately).
( sleep 30 & echo $! > "$SLEEP_PID_FILE" )
sleep_pid2=$(cat "$SLEEP_PID_FILE")
start_ts=$(date +%s)
( cd "$WORKDIR" && LOOM_LAUNCHD_LABEL="$FAKE_LABEL" bash "$STOP_SCRIPT" --force >/dev/null 2>&1 )
end_ts=$(date +%s)
elapsed=$((end_ts - start_ts))
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$elapsed" -le 5 ]] && ! kill -0 "$sleep_pid2" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --force kills immediately without waiting the grace window"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --force kills immediately without waiting the grace window (elapsed=${elapsed}s)"
    kill -9 "$sleep_pid2" 2>/dev/null || true
fi

# 4. --help documents the launchd bootout counterpart and LOOM_LAUNCHD_LABEL.
help_out=$(bash "$STOP_SCRIPT" --help 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$help_out" | grep -qi 'launchd' && echo "$help_out" | grep -q 'LOOM_LAUNCHD_LABEL'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --help documents the launchd bootout counterpart"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --help documents the launchd bootout counterpart"
fi

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
