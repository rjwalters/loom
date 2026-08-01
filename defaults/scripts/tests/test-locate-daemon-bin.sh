#!/usr/bin/env bash
# test-locate-daemon-bin.sh — Tests for lib/locate-daemon-bin.sh's shared
# loom_locate_daemon_bin() / loom_daemon_bin_search_paths() (Issue #4875).
#
# Focus: a non-interactive `ssh host 'cmd'` invocation does not source the
# login profile, so `~/.local/bin` (the epic #3835 Phase 3a machine-level
# install location) is NOT on $PATH. Before this fix, `loom-daemon-start.sh`
# and its sibling scripts (loom-daemon-watchdog.sh, loom-daemon-update.sh,
# loom-status.sh, `.loom/bin/loom health`) gave up in that exact scenario even
# though a current binary sat at `~/.local/bin/loom-daemon`. This suite drives
# the shared resolver directly (all five call sites now delegate to it) with
# $PATH reduced to a minimal, non-interactive default and no $LOOM_DAEMON_BIN,
# asserting the binary is still found.
#
# Style matches the other lib-focused suites — plain bash, hand-rolled
# assertions, no Bats.
#
# Usage:
#   ./defaults/scripts/tests/test-locate-daemon-bin.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB="$SCRIPT_DIR/../lib/locate-daemon-bin.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "${GREEN}✓${NC} $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "${RED}✗${NC} $1"; }

assert_eq() { # <expected> <actual> <msg>
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$1', got '$2')"; fi
}

assert_contains() { # <needle> <haystack> <msg>
    if [[ "$2" == *"$1"* ]]; then pass "$3"; else fail "$3 (expected to find '$1' in [$2])"; fi
}

if [[ ! -r "$LIB" ]]; then
    echo -e "${RED}FATAL${NC}: $LIB not found" >&2
    exit 1
fi

# A minimal, non-interactive-shell-like PATH: no ~/.local/bin, no repo-local
# toolchain dirs, just the base system directories a non-login `ssh host
# 'cmd'` session would inherit.
MINIMAL_PATH="/usr/bin:/bin"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-locate-daemon-bin.XXXXXX")"
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

make_fake_bin() { # <path>
    mkdir -p "$(dirname "$1")"
    cat > "$1" <<'EOF'
#!/usr/bin/env bash
echo "fake-loom-daemon"
EOF
    chmod +x "$1"
}

# ---------- 1. $LOOM_DAEMON_BIN wins over everything, even a bogus PATH ----------
BIN1="$WORKDIR/t1/explicit-loom-daemon"
make_fake_bin "$BIN1"
out=$( env -i PATH="$MINIMAL_PATH" HOME="$WORKDIR/t1-nohome" LOOM_DAEMON_BIN="$BIN1" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t1-root'" )
assert_eq "$BIN1" "$out" "LOOM_DAEMON_BIN override wins"

# ---------- 2. LOOM_DAEMON_BIN set but NOT executable falls through (does not
#               hard-fail) to the remaining candidates ----------
BOGUS_BIN="$WORKDIR/t2/does-not-exist"
BIN2_HOME="$WORKDIR/t2-home"
BIN2="$BIN2_HOME/.local/bin/loom-daemon"
make_fake_bin "$BIN2"
out=$( env -i PATH="$MINIMAL_PATH" HOME="$BIN2_HOME" LOOM_DAEMON_BIN="$BOGUS_BIN" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t2-root'" )
assert_eq "$BIN2" "$out" "non-executable LOOM_DAEMON_BIN falls through to machine-level install, not a hard failure"

# ---------- 3. `loom-daemon` on PATH is found (no LOOM_DAEMON_BIN needed) ----------
PATH_BIN_DIR="$WORKDIR/t3/on-path"
make_fake_bin "$PATH_BIN_DIR/loom-daemon"
out=$( env -i PATH="$PATH_BIN_DIR:$MINIMAL_PATH" HOME="$WORKDIR/t3-nohome" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t3-root'" )
assert_eq "$PATH_BIN_DIR/loom-daemon" "$out" "loom-daemon on \$PATH is found"

# ---------- 4. THE CORE FIX (#4875): PATH reduced to a minimal, non-login
#               default (no ~/.local/bin, no LOOM_DAEMON_BIN) still finds the
#               machine-level install under $HOME/.local/bin -- exactly the
#               `ssh host 'loom-daemon-start.sh --from-config'` scenario. ----------
SSH_HOME="$WORKDIR/t4-ssh-home"
SSH_BIN="$SSH_HOME/.local/bin/loom-daemon"
make_fake_bin "$SSH_BIN"
out=$( env -i PATH="$MINIMAL_PATH" HOME="$SSH_HOME" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t4-root'" )
assert_eq "$SSH_BIN" "$out" "non-interactive-SSH-like minimal \$PATH + no LOOM_DAEMON_BIN still finds \$HOME/.local/bin/loom-daemon (#4875)"

# ---------- 5. $LOOM_DAEMON_BIN_DIR overrides the default ~/.local/bin dir ----------
CUSTOM_DIR="$WORKDIR/t5-custom-install-dir"
CUSTOM_BIN="$CUSTOM_DIR/loom-daemon"
make_fake_bin "$CUSTOM_BIN"
DECOY_HOME="$WORKDIR/t5-decoy-home"
mkdir -p "$DECOY_HOME"
out=$( env -i PATH="$MINIMAL_PATH" HOME="$DECOY_HOME" LOOM_DAEMON_BIN_DIR="$CUSTOM_DIR" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t5-root'" )
assert_eq "$CUSTOM_BIN" "$out" "\$LOOM_DAEMON_BIN_DIR overrides the default ~/.local/bin install dir"

# ---------- 6. in-repo build-output candidates are still the last resort ----------
REPO6="$WORKDIR/t6-repo"
make_fake_bin "$REPO6/target/release/loom-daemon"
out=$( env -i PATH="$MINIMAL_PATH" HOME="$WORKDIR/t6-nohome" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$REPO6'" )
assert_eq "$REPO6/target/release/loom-daemon" "$out" "in-repo target/release/loom-daemon is still found when nothing else resolves"

# ---------- 7. nothing resolvable -> empty output, no error ----------
out=$( env -i PATH="$MINIMAL_PATH" HOME="$WORKDIR/t7-nohome" \
    bash -c "source '$LIB'; loom_locate_daemon_bin '$WORKDIR/t7-root'" )
assert_eq "" "$out" "nothing resolvable -> empty output (caller reports not-found itself)"

# ---------- 8. loom_daemon_bin_search_paths() names the machine-level
#               install location so a "not found" error can list it ----------
paths_out=$( env -i PATH="$MINIMAL_PATH" HOME="$WORKDIR/t8-home" \
    bash -c "source '$LIB'; loom_daemon_bin_search_paths '$WORKDIR/t8-root'" )
assert_contains "$WORKDIR/t8-home/.local/bin/loom-daemon" "$paths_out" "search-paths summary names the machine-level ~/.local/bin install location"
assert_contains "$WORKDIR/t8-root/target/release/loom-daemon" "$paths_out" "search-paths summary still names the in-repo build-output candidates"

# ---------- summary ----------
echo
echo "Ran $TESTS_RUN tests: $TESTS_PASSED passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]]
