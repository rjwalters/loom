#!/usr/bin/env bash
# test-daemon-liveness.sh — regression test for #5548 ("pgrep -f loom-daemon
# is not a liveness check — leaked test fixtures named loom-daemon kept a
# dead daemon looking healthy for 66 minutes").
#
# Reproduces the incident's exact shape: a bash script literally named
# `loom-daemon`, running as `bash /path/to/loom-daemon`. `pgrep -f` matches
# it forever (a full-command-line substring match); `pgrep -x` never does
# (its kernel-reported process *name*/comm is "bash", not "loom-daemon" —
# only the real, compiled binary's comm is "loom-daemon"). This asserts
# scripts/stop-daemon.sh and scripts/start-daemon.sh use the narrower `-x`
# matcher and that a leaked decoy fixture cannot be mistaken for the real
# daemon.
#
# SAFETY: this host may be running a REAL loom-daemon (e.g. the operator's
# production daemon, comm exactly "loom-daemon" — verified present on the
# box this suite was authored on). Test 3 below therefore NEVER lets
# scripts/stop-daemon.sh's own `pgrep` call reach the live process table —
# it shadows `pgrep` on PATH with a stub that only knows about this test's
# scratch decoy, so the exercised kill-or-not decision is real but can never
# touch a process this test does not own. Tests 1/2 use the real `pgrep`
# read-only (never `kill`), and assert only on the presence/absence of this
# suite's own DECOY_PID in the output — never "no match at all" — so they
# stay correct even on a host with its own real loom-daemon already running.
#
# Usage: bash scripts/test-daemon-liveness.sh
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STOP_SCRIPT="$REPO_ROOT/scripts/stop-daemon.sh"
START_SCRIPT="$REPO_ROOT/scripts/start-daemon.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

passed=0
failed=0
pass() { echo -e "${GREEN}✓${NC} $1"; passed=$((passed + 1)); }
fail() { echo -e "${RED}✗${NC} $1"; failed=$((failed + 1)); }

if ! command -v pgrep >/dev/null 2>&1; then
    echo "pgrep not found on PATH -- skipping (this suite tests pgrep-based liveness checks)"
    exit 0
fi

WORKDIR="$(mktemp -d)"
DECOY_PID=""
cleanup() {
    [[ -n "$DECOY_PID" ]] && kill "$DECOY_PID" 2>/dev/null
    rm -rf "$WORKDIR" 2>/dev/null || true
}
trap cleanup EXIT
trap 'cleanup; exit 1' INT TERM

# The exact incident shape (#5548): a bash script literally named
# `loom-daemon`, backgrounded. We do not need to orphan it (re-parent to
# PID 1) to reproduce the bug -- pgrep matches on process attributes
# (name/cmdline), not parentage; the incident's "orphaned 2 days" detail
# just explains why it was still around to be matched, not why it matched.
DECOY_DIR="$WORKDIR/decoy"
mkdir -p "$DECOY_DIR"
cat > "$DECOY_DIR/loom-daemon" <<'EOF'
#!/usr/bin/env bash
while true; do sleep 1; done
EOF
chmod +x "$DECOY_DIR/loom-daemon"
"$DECOY_DIR/loom-daemon" >/dev/null 2>&1 &
DECOY_PID=$!
# Give the decoy a moment to actually be running before probing for it.
for _ in $(seq 1 20); do
    kill -0 "$DECOY_PID" 2>/dev/null && break
    sleep 0.1
done

echo "Test 1: a leaked fixture literally named 'loom-daemon' does not satisfy pgrep -x"
match_pids="$(pgrep -x "loom-daemon" 2>/dev/null || true)"
if printf '%s\n' "$match_pids" | grep -qx "$DECOY_PID"; then
    fail "pgrep -x 'loom-daemon' matched the bash-script decoy PID $DECOY_PID (its comm should be 'bash', not 'loom-daemon')"
else
    pass "pgrep -x 'loom-daemon' does not match the decoy PID $DECOY_PID"
fi

echo "Test 2: the OLD (broken) pgrep -f WOULD have matched it (proves the decoy reproduces the incident)"
match_pids_f="$(pgrep -f "loom-daemon" 2>/dev/null || true)"
if printf '%s\n' "$match_pids_f" | grep -qx "$DECOY_PID"; then
    pass "pgrep -f 'loom-daemon' (the pre-fix matcher) matches the decoy PID $DECOY_PID, confirming the bug shape is real"
else
    fail "pgrep -f 'loom-daemon' unexpectedly did NOT match the decoy -- test setup may be broken"
fi

echo "Test 3: scripts/stop-daemon.sh's no-PID-file fallback leaves the decoy alone (pgrep stubbed for safety -- see file header)"
STUB_BIN_DIR="$WORKDIR/stubbin"
mkdir -p "$STUB_BIN_DIR"
# A pgrep stub scoped ONLY to this test's own decoy -- never touches the
# live process table. Mirrors the real pgrep contract just enough for
# stop-daemon.sh's call shape (`pgrep -x "loom-daemon" | head -1` and, for
# comparison, `pgrep -f "loom-daemon"`): `-x` never matches (the decoy's
# real comm is "bash"); `-f` matches the decoy's PID (proving the pre-fix
# shape), consistent with Tests 1/2 above.
cat > "$STUB_BIN_DIR/pgrep" <<STUBEOF
#!/usr/bin/env bash
if [[ "\$1" == "-x" && "\$2" == "loom-daemon" ]]; then
    exit 1
fi
if [[ "\$1" == "-f" && "\$2" == "loom-daemon" ]]; then
    echo "$DECOY_PID"
    exit 0
fi
exit 1
STUBEOF
chmod +x "$STUB_BIN_DIR/pgrep"

STOP_WORKDIR="$WORKDIR/stopcwd"
mkdir -p "$STOP_WORKDIR/.loom"
out="$(cd "$STOP_WORKDIR" && PATH="$STUB_BIN_DIR:$PATH" bash "$STOP_SCRIPT" 2>&1)"
if printf '%s\n' "$out" | grep -q "Daemon not running"; then
    pass "stop-daemon.sh (no PID file, decoy present) reports 'Daemon not running'"
else
    fail "stop-daemon.sh did not report 'Daemon not running' (got: $out)"
fi
if kill -0 "$DECOY_PID" 2>/dev/null; then
    pass "stop-daemon.sh left the decoy process alone"
else
    fail "stop-daemon.sh killed the decoy process -- the #5548 regression is back"
fi

echo "Test 4: scripts/start-daemon.sh's bare pgrep fallback resolves via -x (exact process-name match), not -f"
if grep -q 'pgrep -x "loom-daemon" | head -1' "$START_SCRIPT"; then
    pass "start-daemon.sh's bare fallback uses 'pgrep -x' (exact process-name match)"
else
    fail "start-daemon.sh's bare fallback no longer matches the expected 'pgrep -x' pattern -- check for regressions"
fi

echo ""
echo "Results: $passed passed, $failed failed"
[[ "$failed" -eq 0 ]]
