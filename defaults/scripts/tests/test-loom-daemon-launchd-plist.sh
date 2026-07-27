#!/usr/bin/env bash
# test-loom-daemon-launchd-plist.sh — Tests for the launchd LaunchAgent plist
# rendering added by loom-daemon-start.sh / loom-daemon-stop.sh (#3972).
#
# Root cause under test: a plain `nohup "$DAEMON_BIN" &` leaves the daemon
# wired into the LAUNCHING SESSION's Mach bootstrap namespace, so when that
# session dies, gh/git start failing XPC lookups (trustd/opendirectoryd) for
# the daemon and every child it spawns. The fix loads the daemon as a
# `gui/<uid>` LaunchAgent instead.
#
# These tests exercise ONLY the plist-rendering path (`--print-plist`), which
# is pure string generation with NO side effects — it never calls `launchctl`
# and never touches ~/Library/LaunchAgents. This is deliberate: a test suite
# must never mutate the real machine's launchd state, on this dev box or any
# CI runner. Real `launchctl bootstrap`/`kickstart`/`bootout` behavior is not
# covered here (requires an actual GUI login session) and is validated
# manually per the daemon-reference.md Operability writeup.
#
# Style matches test-loom-daemon-start.sh — plain bash, hand-rolled
# assertions. Bats is NOT used in this repository.
#
# Usage:
#   ./defaults/scripts/tests/test-loom-daemon-launchd-plist.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
START_SCRIPT="$(cd "$SCRIPT_DIR/../cli" && pwd)/loom-daemon-start.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" == *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
        echo "  expected to find: [$needle]"
    fi
}

assert_not_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" != *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
        echo "  expected NOT to find: [$needle]"
    fi
}

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

# ---------- fixture ----------
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
mkdir -p "$WORKDIR/.loom/logs"

FAKE_BIN="$WORKDIR/fake-loom-daemon"
cat > "$FAKE_BIN" <<'EOF'
#!/usr/bin/env bash
echo "FAKE_DAEMON"
EOF
chmod +x "$FAKE_BIN"

# ---------- tests ----------

# 1. --print-plist is pure inspection: never writes to ~/Library/LaunchAgents
#    or invokes launchctl, regardless of host platform.
before_count=0
if [[ -d "$HOME/Library/LaunchAgents" ]]; then
    before_count=$(find "$HOME/Library/LaunchAgents" -maxdepth 1 -name 'com.rjwalters.loom-daemon*.plist' 2>/dev/null | wc -l | tr -d ' ')
fi
plist_out=$( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --print-plist 2>&1 )
plist_rc=$?
after_count=0
if [[ -d "$HOME/Library/LaunchAgents" ]]; then
    after_count=$(find "$HOME/Library/LaunchAgents" -maxdepth 1 -name 'com.rjwalters.loom-daemon*.plist' 2>/dev/null | wc -l | tr -d ' ')
fi
assert_eq "0" "$plist_rc" "--print-plist exits 0"
assert_eq "$before_count" "$after_count" "--print-plist never writes to ~/Library/LaunchAgents"

# 2. Plain start: RunAtLoad/KeepAlive false, both autonomy loops OFF present
#    and equal to 0 (FLAGS-OFF default, #3911 semantics preserved), PATH
#    covers gh/git/cargo/python3 fallback dirs.
assert_contains "$plist_out" "<key>RunAtLoad</key>" "plist declares RunAtLoad"
assert_contains "$plist_out" $'<key>RunAtLoad</key>\n    <true/>' "RunAtLoad is true (mirrors the validated incident-fix plist; survives reboot/re-login)"
assert_contains "$plist_out" $'<key>KeepAlive</key>\n    <false/>' "KeepAlive is false (no launchd auto-restart on crash)"
assert_contains "$plist_out" "<key>LOOM_WORK_FINDER</key>" "plain start forwards LOOM_WORK_FINDER"
assert_contains "$plist_out" $'<key>LOOM_WORK_FINDER</key>\n        <string>0</string>' "plain start: LOOM_WORK_FINDER=0 (FLAGS-OFF default)"
assert_contains "$plist_out" $'<key>LOOM_MAIN_HEALTH_GATE</key>\n        <string>0</string>' "plain start: LOOM_MAIN_HEALTH_GATE=0 (FLAGS-OFF default)"
assert_contains "$plist_out" "/.local/bin" "PATH includes ~/.local/bin fallback (loom-daemon's own provisioning dir, #3922)"
assert_contains "$plist_out" "/.cargo/bin" "PATH includes ~/.cargo/bin fallback (cargo)"
assert_contains "$plist_out" "/usr/bin" "PATH includes /usr/bin fallback (python3, git)"
assert_contains "$plist_out" "/opt/homebrew/bin" "PATH includes Homebrew fallback (gh, git)"
assert_contains "$plist_out" "$FAKE_BIN" "ProgramArguments points at the resolved daemon binary"
assert_contains "$plist_out" "com.rjwalters.loom-daemon" "default Label is com.rjwalters.loom-daemon"

# 3. --work-finder --print-plist -> LOOM_WORK_FINDER=1, health gate stays 0.
wf_out=$( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --work-finder --print-plist 2>&1 )
assert_contains "$wf_out" $'<key>LOOM_WORK_FINDER</key>\n        <string>1</string>' "--work-finder: plist forwards LOOM_WORK_FINDER=1"
assert_contains "$wf_out" $'<key>LOOM_MAIN_HEALTH_GATE</key>\n        <string>0</string>' "--work-finder: health gate stays 0"

# 4. --from-config --print-plist -> neither autonomy var is forced into the
#    plist at all (env not forced -- config drives inside the daemon process).
fc_out=$( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --from-config --print-plist 2>&1 )
assert_not_contains "$fc_out" "<key>LOOM_WORK_FINDER</key>" "--from-config: LOOM_WORK_FINDER NOT forced into the plist"
assert_not_contains "$fc_out" "<key>LOOM_MAIN_HEALTH_GATE</key>" "--from-config: LOOM_MAIN_HEALTH_GATE NOT forced into the plist"

# 5. An already-exported LOOM_WORK_FINDER=1 (operator override) is forwarded
#    verbatim even on a plain (non-flag) invocation.
exported_out=$( cd "$WORKDIR" && env -u LOOM_MAIN_HEALTH_GATE LOOM_WORK_FINDER=1 LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --print-plist 2>&1 )
assert_contains "$exported_out" $'<key>LOOM_WORK_FINDER</key>\n        <string>1</string>' "already-exported LOOM_WORK_FINDER=1 wins and is forwarded"

# 6. LOOM_LAUNCHD_LABEL overrides the default label.
label_out=$( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_LAUNCHD_LABEL="com.example.custom" LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --print-plist 2>&1 )
assert_contains "$label_out" "com.example.custom" "LOOM_LAUNCHD_LABEL overrides the default Label"
assert_not_contains "$label_out" "<string>com.rjwalters.loom-daemon</string>" "custom label replaces (not appends to) the default"

# 7. --print-plist never persists to .loom/.daemon.flags (it isn't a daemon
#    autonomy flag; loom-daemon-update.sh must never replay it).
rm -f "$WORKDIR/.loom/.daemon.flags"
( cd "$WORKDIR" && env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE LOOM_DAEMON_BIN="$FAKE_BIN" bash "$START_SCRIPT" --work-finder --print-plist >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if [[ ! -f "$WORKDIR/.loom/.daemon.flags" ]] || ! grep -q -- '--print-plist' "$WORKDIR/.loom/.daemon.flags" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --print-plist is excluded from the persisted .daemon.flags file"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --print-plist is excluded from the persisted .daemon.flags file"
fi

# 8. --help documents --no-launchd and --print-plist.
help_out=$(bash "$START_SCRIPT" --help 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$help_out" | grep -q -- '--no-launchd' && echo "$help_out" | grep -q -- '--print-plist'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --help documents --no-launchd and --print-plist"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --help documents --no-launchd and --print-plist"
fi

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
