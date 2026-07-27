#!/usr/bin/env bash
# test-loom-daemon-start.sh — Tests for loom-daemon-start.sh autonomous-mode
# env resolution.
#
# Focus (#3911): a bare `loom-daemon-start.sh` must default FLAGS-OFF — it must
# NOT enable the autonomous work finder. Opt-in (--work-finder / --health-gate /
# --from-config) and explicit-off (--no-work-finder) must still behave.
#
# Style matches test-spawn-claude.sh — plain bash, hand-rolled assertions.
# Bats is NOT used in this repository.
#
# Strategy: drive the script in --foreground mode against a FAKE daemon binary
# that prints the LOOM_WORK_FINDER / LOOM_MAIN_HEALTH_GATE it inherited, then
# assert on that marker line. --foreground `exec`s the binary, so the exported
# env is exactly what a real daemon would see.
#
# Usage:
#   ./defaults/scripts/tests/test-loom-daemon-start.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
START_SCRIPT="$(cd "$SCRIPT_DIR/../cli" && pwd)/loom-daemon-start.sh"

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
        echo -e "  expected: [$expected]"
        echo -e "  actual:   [$actual]"
    fi
}

# ---------- fixture ----------
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/.loom/logs"

FAKE_BIN="$WORKDIR/fake-loom-daemon"
cat > "$FAKE_BIN" <<'EOF'
#!/usr/bin/env bash
# Prints the autonomous-mode env it inherited, then exits cleanly.
echo "FAKE_DAEMON WF=[${LOOM_WORK_FINDER:-}] HG=[${LOOM_MAIN_HEALTH_GATE:-}]"
EOF
chmod +x "$FAKE_BIN"

# ---------- tests ----------

# 1. Plain start = FLAGS-OFF: work finder off, health gate off.
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[0] HG=[0]" "$out" "plain start defaults both loops OFF (#3911)"

# 2. --work-finder opts the finder on (gate stays off).
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --work-finder --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[1] HG=[0]" "$out" "--work-finder enables finder only"

# 3. --health-gate opts the gate on (finder stays off).
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --health-gate --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[0] HG=[1]" "$out" "--health-gate enables gate only"

# 4. Both flags → both on.
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --work-finder --health-gate --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[1] HG=[1]" "$out" "--work-finder --health-gate enables both"

# 5. Already-exported env wins over the flags-off default.
out=$( ( cd "$WORKDIR" && env -u LOOM_MAIN_HEALTH_GATE LOOM_WORK_FINDER=1 LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[1] HG=[0]" "$out" "exported LOOM_WORK_FINDER=1 wins on plain start"

# 6. --from-config forces neither var (leaves both unset for config to drive).
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --from-config --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[] HG=[]" "$out" "--from-config leaves both env vars unset"

# 7. --no-work-finder forces finder off (explicit; matches default).
out=$( ( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --no-work-finder --foreground 2>/dev/null ) | grep '^FAKE_DAEMON' )
assert_eq "FAKE_DAEMON WF=[0] HG=[0]" "$out" "--no-work-finder forces finder off"

# 8. --help mentions the FLAGS-OFF default and the opt-in flags.
help_out=$(bash "$START_SCRIPT" --help 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$help_out" | grep -qi 'FLAGS-OFF' && echo "$help_out" | grep -q -- '--work-finder'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --help documents the FLAGS-OFF default and --work-finder"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --help documents the FLAGS-OFF default and --work-finder"
fi

# 9. Regression (#3968 flag persistence): a BARE invocation with ZERO args in
#    background mode (the common real-world case — --foreground is not used
#    here on purpose) must not crash. Bash < 4.4 (still /bin/bash on stock
#    macOS) raises "unbound variable" on `"${arr[@]}"` for a zero-element
#    array under `set -u`; the persist-flags loop must guard against that.
#    Also asserts the persisted flags file exists and is empty for a bare start.
#    Needs a fake binary that stays alive (the shared $FAKE_BIN above prints
#    one line and exits immediately, which the start script's own liveness
#    check would correctly treat as a startup failure — not what this test
#    is exercising).
#    --no-launchd forces the legacy nohup path even on a Darwin test runner —
#    this test exercises the flags-persistence array guard, not launchd
#    (#3972), and must never mutate the real machine's LaunchAgents.
BG_FAKE_BIN="$WORKDIR/fake-loom-daemon-bg"
cat > "$BG_FAKE_BIN" <<'EOF'
#!/usr/bin/env bash
sleep 5
EOF
chmod +x "$BG_FAKE_BIN"
( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$BG_FAKE_BIN" bash "$START_SCRIPT" --no-launchd >/dev/null 2>&1 )
bg_rc=$?
assert_eq "0" "$bg_rc" "bare (zero-arg) background start exits 0 (no unbound-variable crash, #3968)"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -f "$WORKDIR/.loom/.daemon.flags" && ! -s "$WORKDIR/.loom/.daemon.flags" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} bare start persists an EMPTY .loom/.daemon.flags (no autonomy flags to record)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} bare start persists an EMPTY .loom/.daemon.flags (no autonomy flags to record)"
fi
# Clean up the background daemon this test started.
if [[ -f "$WORKDIR/.loom/.daemon.pid" ]]; then
    kill "$(cat "$WORKDIR/.loom/.daemon.pid" 2>/dev/null)" 2>/dev/null || true
fi

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
