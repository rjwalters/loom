#!/usr/bin/env bash
# test-loom-daemon-update.sh — Tests for loom-daemon-update.sh (Issue #3968).
#
# Focus: (1) staleness detection (--check, built-commit vs. source-HEAD
# comparison), (2) flag preservation across a rebuild+restart (the persisted
# .loom/.daemon.flags file must be replayed verbatim, never widened), and
# (3) the "was not running -> do not start" / --no-restart / --dry-run
# guardrails that keep the FLAGS-OFF/opt-in contract intact.
#
# Style matches test-loom-daemon-start.sh — plain bash, hand-rolled
# assertions, no bats.
#
# Strategy: build a throwaway repo root (temp dir) with a real `.git`, a
# stub `loom-daemon/Cargo.toml`, real copies of loom-daemon-start.sh /
# loom-daemon-stop.sh (so the restart path exercises the actual scripts, not
# a mock), a FAKE `cargo` on PATH that "builds" by copying a prepared fake
# daemon binary into target/release/ instead of actually compiling Rust, and
# FAKE daemon binaries whose `--version` output embeds a controllable commit
# and whose normal-run mode records the autonomy env vars it inherited to a
# marker file before looping (staying "alive" for PID-liveness checks).
#
# Usage:
#   ./defaults/scripts/tests/test-loom-daemon-update.sh

set -uo pipefail

# Force the legacy nohup path everywhere in this suite (#3972): the restart
# flow below exercises the REAL loom-daemon-start.sh / loom-daemon-stop.sh
# (not a mock), and this test must NEVER touch the real machine's
# ~/Library/LaunchAgents/ -- which may hold an actual production
# com.rjwalters.loom-daemon LaunchAgent under the exact same default label.
export LOOM_DAEMON_LAUNCHD=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$SCRIPT_DIR/../cli" && pwd)"
UPDATE_SCRIPT="$CLI_DIR/loom-daemon-update.sh"

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

assert_true() {
    local cond="$1" msg="$2"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$cond" == "true" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "${GREEN}✓${NC} $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "${RED}✗${NC} $msg"
    fi
}

# ---------- fixture builder ----------
# Sets up a fresh throwaway repo root at $1 with a real git HEAD, a stub
# loom-daemon crate, real start/stop scripts, and a minimal, machine-agnostic
# PATH (excludes ~/.local/bin and similar, so a real loom-daemon possibly
# installed on the dev machine can never leak into a test).
new_fixture() {
    local root="$1"
    mkdir -p "$root/.loom/logs" "$root/.loom/scripts/cli" "$root/loom-daemon"
    cp "$CLI_DIR/loom-daemon-start.sh" "$root/.loom/scripts/cli/loom-daemon-start.sh"
    cp "$CLI_DIR/loom-daemon-stop.sh" "$root/.loom/scripts/cli/loom-daemon-stop.sh"
    chmod +x "$root/.loom/scripts/cli/"*.sh
    cat > "$root/loom-daemon/Cargo.toml" <<'EOF'
[package]
name = "loom-daemon"
version = "0.0.0"
EOF
    ( cd "$root" && git init -q && git -c user.email=test@test -c user.name=test commit -q --allow-empty -m init )
}

# Writes a fake daemon binary at $1 that reports commit $2 on --version and,
# on a normal run, appends its inherited LOOM_WORK_FINDER / LOOM_MAIN_HEALTH_GATE
# to marker file $3 before looping forever (so it stays alive for kill -0).
write_fake_daemon() {
    local path="$1" commit="$2" marker="$3"
    cat > "$path" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then
    echo "loom-daemon 0.15.0 (commit ${commit}, built 2026-07-26T00:00:00Z)"
    exit 0
fi
echo "FAKE_DAEMON WF=[\${LOOM_WORK_FINDER:-}] HG=[\${LOOM_MAIN_HEALTH_GATE:-}]" > "${marker}"
while true; do sleep 1; done
EOF
    chmod +x "$path"
}

# Writes a fake `cargo` that, on `cargo build --release` (cwd = loom-daemon/),
# copies $NEW_FAKE_BIN_SRC into target/release/loom-daemon instead of
# compiling. Tests export NEW_FAKE_BIN_SRC before invoking loom-daemon-update.sh.
write_fake_cargo() {
    local path="$1"
    cat > "$path" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "build" ]]; then
    mkdir -p target/release
    cp "$NEW_FAKE_BIN_SRC" target/release/loom-daemon
    chmod +x target/release/loom-daemon
    echo "[fake cargo] build ok"
    exit 0
fi
echo "[fake cargo] unsupported subcommand: $*" >&2
exit 1
EOF
    chmod +x "$path"
}

MINIMAL_PATH="/usr/bin:/bin:/usr/sbin:/sbin"

BASE_WORKDIR="$(mktemp -d)"
# Best-effort cleanup of any fake-daemon processes left running (matched by
# their script path under $BASE_WORKDIR, which appears in `ps`'s command
# line) — individual tests also kill their own PIDs explicitly, this is a
# backstop for anything a failed assertion left behind.
trap 'pkill -f "$BASE_WORKDIR" >/dev/null 2>&1; rm -rf "$BASE_WORKDIR"' EXIT

FAKE_BIN_DIR="$BASE_WORKDIR/fakebin"
mkdir -p "$FAKE_BIN_DIR"
write_fake_cargo "$FAKE_BIN_DIR/cargo"
TEST_PATH="$FAKE_BIN_DIR:$MINIMAL_PATH"

# ============================================================
# 1. --check reports "up to date" (exit 0) when the installed
#    binary's baked-in commit matches the source tree's HEAD.
# ============================================================
W1="$BASE_WORKDIR/w1"
new_fixture "$W1"
HEAD1="$(cd "$W1" && git rev-parse --short HEAD)"
write_fake_daemon "$W1/installed-loom-daemon" "$HEAD1" "$W1/marker"
out=$( cd "$W1" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W1/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check; echo "EXIT=$?" )
rc=$(echo "$out" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)
assert_eq "0" "$rc" "--check exits 0 when installed commit matches source HEAD"

# ============================================================
# 2. --check reports "update available" (exit 3) when the
#    installed binary's baked-in commit differs from source HEAD.
# ============================================================
W2="$BASE_WORKDIR/w2"
new_fixture "$W2"
write_fake_daemon "$W2/installed-loom-daemon" "deadbee" "$W2/marker"
out=$( cd "$W2" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W2/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check; echo "EXIT=$?" )
rc=$(echo "$out" | grep -o 'EXIT=[0-9]*' | cut -d= -f2)
assert_eq "3" "$rc" "--check exits 3 when installed commit differs from source HEAD"

# ============================================================
# 3. --check makes NO writes (no rebuild, no provisioning).
# ============================================================
W3="$BASE_WORKDIR/w3"
new_fixture "$W3"
write_fake_daemon "$W3/installed-loom-daemon" "deadbee" "$W3/marker"
( cd "$W3" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W3/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --check >/dev/null 2>&1 )
if [[ -e "$W3/loom-daemon/target/release/loom-daemon" ]]; then
    assert_eq "no-build-dir" "build-dir-present" "--check performs no rebuild"
else
    assert_eq "true" "true" "--check performs no rebuild"
fi

# ============================================================
# 4. --dry-run reports the plan but makes no writes either.
# ============================================================
W4="$BASE_WORKDIR/w4"
new_fixture "$W4"
write_fake_daemon "$W4/installed-loom-daemon" "deadbee" "$W4/marker"
dryrun_out=$( cd "$W4" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$W4/installed-loom-daemon" \
    bash "$UPDATE_SCRIPT" --dry-run 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -e "$W4/loom-daemon/target/release/loom-daemon" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run performs no rebuild"
elif echo "$dryrun_out" | grep -q 'dry-run.*cargo build'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --dry-run performs no rebuild and prints the plan"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --dry-run performs no rebuild and prints the plan"
    echo "  output: $dryrun_out"
fi

# ============================================================
# 5. Full flow: stale binary + a daemon that WAS running with
#    persisted flags (--work-finder) -> rebuild, provision,
#    restart with EXACTLY the persisted flags (never widened).
# ============================================================
W5="$BASE_WORKDIR/w5"
new_fixture "$W5"
HEAD5="$(cd "$W5" && git rev-parse --short HEAD)"
INSTALLED5="$W5/installed/loom-daemon"
mkdir -p "$W5/installed"
write_fake_daemon "$INSTALLED5" "deadbee" "$W5/old-marker"
NEW_FAKE5="$W5/new-fake-daemon"
write_fake_daemon "$NEW_FAKE5" "$HEAD5" "$W5/new-marker"

# Persisted flags from a prior `loom-daemon-start.sh --work-finder` (Issue #3968).
echo "--work-finder" > "$W5/.loom/.daemon.flags"

# Simulate an already-running old daemon: background the OLD fake binary and
# record its PID, exactly like loom-daemon-start.sh would have.
"$INSTALLED5" >/dev/null 2>&1 &
old_pid=$!
sleep 0.3
echo "$old_pid" > "$W5/.loom/.daemon.pid"
TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} fixture: old daemon process is alive before update"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} fixture: old daemon process is alive before update"
fi

( cd "$W5" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED5" NEW_FAKE_BIN_SRC="$NEW_FAKE5" \
    env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE \
    bash "$UPDATE_SCRIPT" >"$W5/update.log" 2>&1 )
update_rc=$?
assert_eq "0" "$update_rc" "full update run (rebuild+provision+restart) exits 0"

TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid" 2>/dev/null; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} old daemon process was stopped by the restart"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} old daemon process was stopped by the restart"
fi

# Wait briefly for the new (restarted) daemon to write its marker.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [[ -s "$W5/new-marker" ]] && break
    sleep 0.3
done
new_marker_content="$(cat "$W5/new-marker" 2>/dev/null || echo "<missing>")"
assert_eq "FAKE_DAEMON WF=[1] HG=[0]" "$new_marker_content" \
    "restarted daemon inherited EXACTLY the persisted --work-finder flag (never widened to health-gate too)"

TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$INSTALLED5" ]] && "$INSTALLED5" --version 2>/dev/null | grep -q "$HEAD5"; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} LOOM_DAEMON_BIN path was provisioned with the freshly-built binary"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} LOOM_DAEMON_BIN path was provisioned with the freshly-built binary"
fi

# Clean up the restarted daemon (find it via the PID file this run wrote).
if [[ -f "$W5/.loom/.daemon.pid" ]]; then
    kill "$(cat "$W5/.loom/.daemon.pid" 2>/dev/null)" 2>/dev/null || true
fi

# ============================================================
# 5b. Regression: an EMPTY persisted-flags file (a prior bare/FLAGS-OFF
#     start) must restart cleanly with zero flags — no unbound-variable
#     crash from `"${RESTART_ARGS[@]}"` on a zero-element array under
#     `set -u` (bash < 4.4, still /bin/bash on stock macOS).
# ============================================================
W5B="$BASE_WORKDIR/w5b"
new_fixture "$W5B"
HEAD5B="$(cd "$W5B" && git rev-parse --short HEAD)"
INSTALLED5B="$W5B/installed/loom-daemon"
mkdir -p "$W5B/installed"
write_fake_daemon "$INSTALLED5B" "deadbee" "$W5B/old-marker"
NEW_FAKE5B="$W5B/new-fake-daemon"
write_fake_daemon "$NEW_FAKE5B" "$HEAD5B" "$W5B/new-marker"
: > "$W5B/.loom/.daemon.flags" # empty, mirroring a bare prior start

"$INSTALLED5B" >/dev/null 2>&1 &
old_pid5b=$!
sleep 0.3
echo "$old_pid5b" > "$W5B/.loom/.daemon.pid"

( cd "$W5B" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED5B" NEW_FAKE_BIN_SRC="$NEW_FAKE5B" \
    env -u LOOM_WORK_FINDER -u LOOM_MAIN_HEALTH_GATE \
    bash "$UPDATE_SCRIPT" >/dev/null 2>&1 )
update5b_rc=$?
assert_eq "0" "$update5b_rc" "restart with an EMPTY persisted-flags file exits 0 (#3968 regression)"

for _ in 1 2 3 4 5 6 7 8 9 10; do
    [[ -s "$W5B/new-marker" ]] && break
    sleep 0.3
done
new5b_marker_content="$(cat "$W5B/new-marker" 2>/dev/null || echo "<missing>")"
# A plain (zero-flag) restart forwards no CLI args, so loom-daemon-start.sh's
# own FLAGS-OFF default applies: LOOM_WORK_FINDER=0, LOOM_MAIN_HEALTH_GATE=0.
assert_eq "FAKE_DAEMON WF=[0] HG=[0]" "$new5b_marker_content" \
    "restarted daemon with empty persisted flags runs plain FLAGS-OFF (no args forwarded)"

if [[ -f "$W5B/.loom/.daemon.pid" ]]; then
    kill "$(cat "$W5B/.loom/.daemon.pid" 2>/dev/null)" 2>/dev/null || true
fi

# ============================================================
# 6. Stale binary but daemon was NOT running -> rebuild+provision
#    happen, but the daemon is NOT started (never widen FLAGS-OFF
#    by starting autonomy — or anything — that wasn't already up).
# ============================================================
W6="$BASE_WORKDIR/w6"
new_fixture "$W6"
HEAD6="$(cd "$W6" && git rev-parse --short HEAD)"
INSTALLED6="$W6/installed/loom-daemon"
mkdir -p "$W6/installed"
write_fake_daemon "$INSTALLED6" "deadbee" "$W6/old-marker"
NEW_FAKE6="$W6/new-fake-daemon"
write_fake_daemon "$NEW_FAKE6" "$HEAD6" "$W6/new-marker"
# No .loom/.daemon.pid -> WAS_RUNNING resolves false.

out6=$( cd "$W6" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED6" NEW_FAKE_BIN_SRC="$NEW_FAKE6" \
    bash "$UPDATE_SCRIPT" 2>&1 )
rc6=$?
assert_eq "0" "$rc6" "update with no prior PID file exits 0 (rebuild+provision only)"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$out6" | grep -qi 'not running'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} update reports the daemon was not running (nothing started)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} update reports the daemon was not running (nothing started)"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -f "$W6/.loom/.daemon.pid" ]]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} update never starts a daemon that wasn't already running"
else
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} update never starts a daemon that wasn't already running"
fi

# ============================================================
# 7. --no-restart rebuilds + provisions but leaves a running
#    daemon untouched (old process stays alive on the old binary).
# ============================================================
W7="$BASE_WORKDIR/w7"
new_fixture "$W7"
HEAD7="$(cd "$W7" && git rev-parse --short HEAD)"
INSTALLED7="$W7/installed/loom-daemon"
mkdir -p "$W7/installed"
write_fake_daemon "$INSTALLED7" "deadbee" "$W7/old-marker"
NEW_FAKE7="$W7/new-fake-daemon"
write_fake_daemon "$NEW_FAKE7" "$HEAD7" "$W7/new-marker"

"$INSTALLED7" >/dev/null 2>&1 &
old_pid7=$!
sleep 0.3
echo "$old_pid7" > "$W7/.loom/.daemon.pid"

( cd "$W7" && PATH="$TEST_PATH" LOOM_DAEMON_BIN="$INSTALLED7" NEW_FAKE_BIN_SRC="$NEW_FAKE7" \
    bash "$UPDATE_SCRIPT" --no-restart >/dev/null 2>&1 )

TESTS_RUN=$((TESTS_RUN + 1))
if kill -0 "$old_pid7" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --no-restart leaves the running daemon process untouched"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --no-restart leaves the running daemon process untouched"
fi
kill "$old_pid7" 2>/dev/null || true

# ============================================================
# 8. --help documents --check / --dry-run / --force / --no-restart.
# ============================================================
help_out=$(bash "$UPDATE_SCRIPT" --help 2>/dev/null)
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$help_out" | grep -q -- '--check' && echo "$help_out" | grep -q -- '--dry-run' \
    && echo "$help_out" | grep -q -- '--no-restart'; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "${GREEN}✓${NC} --help documents --check / --dry-run / --no-restart"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "${RED}✗${NC} --help documents --check / --dry-run / --no-restart"
fi

# ============================================================
# 9. Refuses to run outside a loom-daemon source checkout.
# ============================================================
W9="$BASE_WORKDIR/w9"
mkdir -p "$W9/.loom"
( cd "$W9" && git init -q )
( cd "$W9" && PATH="$TEST_PATH" bash "$UPDATE_SCRIPT" --check >/dev/null 2>&1 )
rc9=$?
assert_eq "1" "$rc9" "refuses to run when loom-daemon/Cargo.toml is absent"

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
