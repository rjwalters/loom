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

# 5. The bootout path is unchanged (#4054 must not remove it — it stays as
#    belt-and-braces alongside the new exit-code-carries-intent contract).
TESTS_RUN=$((TESTS_RUN + 1))
if grep -q 'launchctl bootout' "$STOP_SCRIPT" && grep -q 'launchd_bootout_if_loaded' "$STOP_SCRIPT"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} bootout path unchanged (launchd_bootout_if_loaded + launchctl bootout still present)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} bootout path unchanged (launchd_bootout_if_loaded + launchctl bootout still present)"
fi

# 6. Stop must NOT report success while a daemon is still alive (#4054): if the
#    launchd job for the label remains loaded with a live pid after the stop (a
#    bootout that did not stick — the inverted-#4011 silent-success hole), the
#    script exits non-zero. Darwin-only: `launchd_job_loaded` short-circuits on
#    non-Darwin, so the relaunch-detection branch cannot run there.
if [[ "$(uname -s)" == "Darwin" ]]; then
    FAKE_BIN_DIR="$WORKDIR/fakebin"
    mkdir -p "$FAKE_BIN_DIR"
    # Fake launchctl: reports the job as loaded and names the "relaunched"
    # daemon's pid, and treats bootout as a no-op (simulating a bootout that
    # failed to stop the relaunched instance).
    cat > "$FAKE_BIN_DIR/launchctl" <<'FAKE'
#!/usr/bin/env bash
case "$1" in
  print)   printf '\tpid = %s\n' "${RELAUNCH_PID:-0}"; exit 0 ;;
  bootout) exit 0 ;;
  *)       exit 0 ;;
esac
FAKE
    chmod +x "$FAKE_BIN_DIR/launchctl"

    # Original daemon (killed by the stop) + a separate live "relaunched" daemon.
    ( sleep 30 & echo $! > "$SLEEP_PID_FILE" )
    orig_pid=$(cat "$SLEEP_PID_FILE")
    sleep 30 &
    relaunch_pid=$!

    stuck_out=$( cd "$WORKDIR" && PATH="$FAKE_BIN_DIR:$PATH" RELAUNCH_PID="$relaunch_pid" \
        LOOM_LAUNCHD_LABEL="$FAKE_LABEL" LOOM_DAEMON_STOP_GRACE_SECS=2 bash "$STOP_SCRIPT" 2>&1 )
    stuck_rc=$?

    assert_eq "1" "$stuck_rc" "relaunched-daemon-still-alive: stop exits non-zero (does not report success)"
    TESTS_RUN=$((TESTS_RUN + 1))
    if echo "$stuck_out" | grep -qi 'still alive'; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} relaunched-daemon-still-alive: reports the live daemon instead of success"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} relaunched-daemon-still-alive: reports the live daemon instead of success"
    fi

    kill -9 "$orig_pid" "$relaunch_pid" 2>/dev/null || true
else
    echo "  (skipping relaunch-detection test — not Darwin)"
fi

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
