#!/usr/bin/env bash
# Integration test for daemon management scripts

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

DAEMON_PID_FILE=".loom/.daemon.pid"
DAEMON_LOG_FILE=".loom/.daemon.log"
DAEMON_SOCKET="$HOME/.loom/daemon.sock"

passed=0
failed=0

# Helper functions
pass() {
  echo -e "${GREEN}✓${NC} $1"
  ((passed++))
}

fail() {
  echo -e "${RED}✗${NC} $1"
  ((failed++))
}

warn() {
  echo -e "${YELLOW}!${NC} $1"
}

cleanup() {
  echo ""
  echo "Cleaning up..."
  ./scripts/stop-daemon.sh > /dev/null 2>&1 || true
  rm -f "$DAEMON_PID_FILE" "$DAEMON_LOG_FILE"
  echo ""
}

# Ensure clean state
cleanup

echo "======================================"
echo "Daemon Management Scripts Test Suite"
echo "======================================"
echo ""

# Test 1: Start daemon
echo "Test 1: Start daemon"
./scripts/start-daemon.sh > /dev/null 2>&1
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    pass "Daemon started successfully (PID: $PID)"
  else
    fail "PID file exists but process not running"
  fi
else
  fail "PID file not created"
fi
echo ""

# Test 2: Verify log file created
echo "Test 2: Verify log file"
if [ -f "$DAEMON_LOG_FILE" ]; then
  if grep -q "Loom daemon starting" "$DAEMON_LOG_FILE"; then
    pass "Log file created with startup message"
  else
    fail "Log file exists but missing startup message"
  fi
else
  fail "Log file not created"
fi
echo ""

# Test 3: Verify socket exists
echo "Test 3: Verify Unix socket"
sleep 1  # Give daemon time to create socket
if [ -S "$DAEMON_SOCKET" ]; then
  pass "Unix socket created at $DAEMON_SOCKET"
else
  fail "Unix socket not found"
fi
echo ""

# Test 4: Idempotent start (should not start twice)
echo "Test 4: Idempotent start"
OUTPUT=$(./scripts/start-daemon.sh 2>&1)
if echo "$OUTPUT" | grep -q "already running"; then
  pass "Script correctly detected daemon already running"
else
  fail "Script did not detect running daemon"
fi
echo ""

# Test 5: Ping daemon (test IPC communication)
echo "Test 5: Ping daemon via IPC"
# Use Tauri command to ping (requires Tauri to be built)
# For now, just check process responds to signal 0
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    pass "Daemon responds to signal check"
  else
    fail "Daemon not responding"
  fi
else
  fail "Cannot test ping - PID file missing"
fi
echo ""

# Test 6: Stop daemon
echo "Test 6: Stop daemon"
./scripts/stop-daemon.sh > /dev/null 2>&1
if [ ! -f "$DAEMON_PID_FILE" ]; then
  pass "PID file removed"
else
  fail "PID file still exists"
fi

# Check process actually stopped
if ! pgrep -f "target/debug/loom-daemon" > /dev/null; then
  pass "Daemon process terminated"
else
  warn "Daemon process still running (may be from other test)"
fi
echo ""

# Test 7: Stop when not running
echo "Test 7: Stop when not running (should not error)"
OUTPUT=$(./scripts/stop-daemon.sh 2>&1)
if [ $? -eq 0 ]; then
  pass "Stop script handled 'not running' gracefully"
else
  fail "Stop script errored when daemon not running"
fi
echo ""

# Test 8: Restart daemon
echo "Test 8: Restart daemon"
./scripts/restart-daemon.sh > /dev/null 2>&1
sleep 1
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    pass "Daemon restarted successfully (PID: $PID)"
  else
    fail "Restart created PID file but process not running"
  fi
else
  fail "Restart did not create PID file"
fi
echo ""

# Test 9: Stop after restart
echo "Test 9: Stop after restart"
./scripts/stop-daemon.sh > /dev/null 2>&1
if [ ! -f "$DAEMON_PID_FILE" ]; then
  pass "Daemon stopped successfully after restart"
else
  fail "PID file still exists after stop"
fi
echo ""

# Test 10: Stale PID file handling
echo "Test 10: Stale PID file handling"
echo "99999" > "$DAEMON_PID_FILE"  # Fake PID unlikely to exist
OUTPUT=$(./scripts/stop-daemon.sh 2>&1)
if echo "$OUTPUT" | grep -q "not running"; then
  pass "Stop script handled stale PID file"
else
  fail "Stop script did not detect stale PID"
fi
if [ ! -f "$DAEMON_PID_FILE" ]; then
  pass "Stale PID file removed"
else
  fail "Stale PID file not cleaned up"
fi
echo ""

# Test 11: Start after stale PID cleanup
echo "Test 11: Start after stale PID cleanup"
./scripts/start-daemon.sh > /dev/null 2>&1
if [ -f "$DAEMON_PID_FILE" ]; then
  PID=$(cat "$DAEMON_PID_FILE")
  if kill -0 "$PID" 2>/dev/null; then
    pass "Daemon started successfully after stale PID cleanup"
  else
    fail "Start failed after stale PID cleanup"
  fi
else
  fail "PID file not created after stale PID cleanup"
fi
echo ""

# Final cleanup
cleanup

# Summary
echo "======================================"
echo "Test Summary"
echo "======================================"
echo -e "${GREEN}Passed: $passed${NC}"
echo -e "${RED}Failed: $failed${NC}"
echo ""

if [ $failed -eq 0 ]; then
  echo -e "${GREEN}All tests passed!${NC}"
  exit 0
else
  echo -e "${RED}Some tests failed.${NC}"
  exit 1
fi
