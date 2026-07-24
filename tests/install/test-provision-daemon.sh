#!/usr/bin/env bash
# Test suite for scripts/install/provision-daemon.sh (issue #3922)
#
# Usage: ./tests/install/test-provision-daemon.sh
#
# Exercises provision_machine_daemon: machine-level install of the built
# loom-daemon binary to a PATH location so a consumer repo's
# loom-daemon-start.sh resolves it via `command -v loom-daemon`. Uses a FAKE
# loom-daemon binary that prints a settable --version string; no real cargo
# build or network access needed.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=scripts/install/provision-daemon.sh
source "$REPO_ROOT/scripts/install/provision-daemon.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0
TOTAL=0

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected: '$expected'"
    echo "  actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

assert_contains() {
  local desc="$1" haystack="$2" needle="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" == *"$needle"* ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected to contain: '$needle'"
    echo "  actual: '$haystack'"
    FAIL=$((FAIL + 1))
  fi
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Build a fake loom-daemon binary that prints $1 as its --version.
make_fake_bin() {
  local path="$1" ver="$2"
  cat > "$path" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then echo "loom-daemon $ver"; fi
EOF
  chmod +x "$path"
}

# ---------- test 1: fresh install to an empty dest dir ----------
SRC1="$WORKDIR/src1/loom-daemon"
mkdir -p "$WORKDIR/src1"
make_fake_bin "$SRC1" "0.14.1"
DEST1="$WORKDIR/dest1"

out1=$(LOOM_DAEMON_BIN_DIR="$DEST1" provision_machine_daemon "$SRC1" 2>&1)
rc1=$?
assert_eq "fresh install returns 0" "0" "$rc1"
assert_eq "binary installed at dest" "1" "$( [[ -x "$DEST1/loom-daemon" ]] && echo 1 || echo 0 )"
assert_eq "installed binary reports src version" "loom-daemon 0.14.1" "$("$DEST1/loom-daemon" --version)"
assert_contains "fresh-install output names the destination" "$out1" "$DEST1/loom-daemon"

# ---------- test 2: idempotent — same version is a no-op copy ----------
# Record mtime, run again, assert it did not re-copy (mtime unchanged) and it
# reports "already current".
before_mtime=$(stat -f %m "$DEST1/loom-daemon" 2>/dev/null || stat -c %Y "$DEST1/loom-daemon")
sleep 1
out2=$(LOOM_DAEMON_BIN_DIR="$DEST1" provision_machine_daemon "$SRC1" 2>&1)
rc2=$?
after_mtime=$(stat -f %m "$DEST1/loom-daemon" 2>/dev/null || stat -c %Y "$DEST1/loom-daemon")
assert_eq "idempotent run returns 0" "0" "$rc2"
assert_eq "idempotent run does NOT re-copy (mtime unchanged)" "$before_mtime" "$after_mtime"
assert_contains "idempotent run reports already current" "$out2" "already current"

# ---------- test 3: version drift — different version overwrites ----------
SRC2="$WORKDIR/src2/loom-daemon"
mkdir -p "$WORKDIR/src2"
make_fake_bin "$SRC2" "0.15.0"
out3=$(LOOM_DAEMON_BIN_DIR="$DEST1" provision_machine_daemon "$SRC2" 2>&1)
rc3=$?
assert_eq "version-drift run returns 0" "0" "$rc3"
assert_eq "dest binary upgraded to new version" "loom-daemon 0.15.0" "$("$DEST1/loom-daemon" --version)"
assert_contains "version-drift run reports install" "$out3" "installed loom-daemon"

# ---------- test 4: missing/unset source binary is a soft failure ----------
out4=$(LOOM_DAEMON_BIN_DIR="$WORKDIR/dest4" provision_machine_daemon "$WORKDIR/does-not-exist" 2>&1)
rc4=$?
assert_eq "missing source returns 1 (soft failure)" "1" "$rc4"
assert_contains "missing source warns" "$out4" "not found"
assert_eq "missing source creates no dest" "0" "$( [[ -e "$WORKDIR/dest4/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 5: PATH warning when dest dir is not on PATH ----------
SRC5="$WORKDIR/src5/loom-daemon"
mkdir -p "$WORKDIR/src5"
make_fake_bin "$SRC5" "0.14.1"
DEST5="$WORKDIR/dest5"
# Ensure DEST5 is definitely not on PATH.
out5=$(PATH="/usr/bin:/bin" LOOM_DAEMON_BIN_DIR="$DEST5" bash -c '
  source "'"$REPO_ROOT"'/scripts/install/provision-daemon.sh"
  provision_machine_daemon "'"$SRC5"'"' 2>&1)
assert_contains "off-PATH dest emits a PATH warning" "$out5" "is not on your PATH"

# ---------- test 6: PATH present → no PATH warning ----------
SRC6="$WORKDIR/src6/loom-daemon"
mkdir -p "$WORKDIR/src6"
make_fake_bin "$SRC6" "0.14.1"
DEST6="$WORKDIR/dest6"
mkdir -p "$DEST6"
out6=$(PATH="$DEST6:/usr/bin:/bin" LOOM_DAEMON_BIN_DIR="$DEST6" bash -c '
  source "'"$REPO_ROOT"'/scripts/install/provision-daemon.sh"
  provision_machine_daemon "'"$SRC6"'"' 2>&1)
TOTAL=$((TOTAL + 1))
if [[ "$out6" != *"is not on your PATH"* ]]; then
  echo -e "${GREEN}PASS${NC}: on-PATH dest emits no PATH warning"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: on-PATH dest emits no PATH warning"
  echo "  unexpected warning in: '$out6'"
  FAIL=$((FAIL + 1))
fi

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
