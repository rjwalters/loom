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

# Binary-format sanity gate bypass (#4397, deferred from #4381's incident
# review): provision_machine_daemon now refuses to install anything that
# isn't a real compiled binary (Mach-O/ELF — see _pmd_is_real_binary in
# scripts/install/provision-daemon.sh). Every fake "daemon" binary this suite
# writes (make_fake_bin below) is a bash script standing in for the real
# compiled binary, so THIS SUITE — and only this suite — sets the explicit,
# auditable bypass suite-wide. Production callers (scripts/install-loom.sh,
# defaults/scripts/cli/loom-daemon-update.sh) never set it. Tests 14/15 below
# exercise the gate itself with the bypass explicitly unset.
export LOOM_PROVISION_ALLOW_SCRIPT=1

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

# ---------- test 7: signing helper — codesign failure is non-fatal (#4016) ----------
# Fake `uname` reports Darwin (deterministic regardless of the host running
# this suite) and a fake `codesign` always exits 1. provision_machine_daemon
# must still return 0 and still install the binary; it should surface a
# non-fatal warning, never abort.
FAKE_FAIL_DIR="$WORKDIR/fake-codesign-fail-bin"
mkdir -p "$FAKE_FAIL_DIR"
cat > "$FAKE_FAIL_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_FAIL_DIR/uname"
cat > "$FAKE_FAIL_DIR/codesign" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$FAKE_FAIL_DIR/codesign"

SRC7="$WORKDIR/src7/loom-daemon"
mkdir -p "$WORKDIR/src7"
make_fake_bin "$SRC7" "0.15.1"
DEST7="$WORKDIR/dest7"
out7=$(PATH="$FAKE_FAIL_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST7" provision_machine_daemon "$SRC7" 2>&1)
rc7=$?
assert_eq "codesign failure: provision still returns 0 (non-fatal)" "0" "$rc7"
assert_eq "codesign failure: binary is still provisioned" "1" "$( [[ -x "$DEST7/loom-daemon" ]] && echo 1 || echo 0 )"
assert_contains "codesign failure: warns non-fatally" "$out7" "codesign failed"

# ---------- test 8: signing helper — non-Darwin skips codesign entirely ----------
# Fake `uname` reports Linux; a fake `codesign` writes a marker file if ever
# invoked. provision_machine_daemon must still succeed and must NEVER invoke
# codesign on a non-Darwin host.
FAKE_LINUX_DIR="$WORKDIR/fake-linux-bin"
mkdir -p "$FAKE_LINUX_DIR"
cat > "$FAKE_LINUX_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Linux"
EOF
chmod +x "$FAKE_LINUX_DIR/uname"
CODESIGN_MARKER8="$WORKDIR/codesign-invoked-marker"
cat > "$FAKE_LINUX_DIR/codesign" <<EOF
#!/usr/bin/env bash
touch "$CODESIGN_MARKER8"
exit 0
EOF
chmod +x "$FAKE_LINUX_DIR/codesign"

SRC8="$WORKDIR/src8/loom-daemon"
mkdir -p "$WORKDIR/src8"
make_fake_bin "$SRC8" "0.15.2"
DEST8="$WORKDIR/dest8"
# shellcheck disable=SC2034  # captured for ad-hoc debugging, not asserted on
out8=$(PATH="$FAKE_LINUX_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST8" provision_machine_daemon "$SRC8" 2>&1)
rc8=$?
assert_eq "non-Darwin: provision still returns 0" "0" "$rc8"
assert_eq "non-Darwin: binary is still provisioned" "1" "$( [[ -x "$DEST8/loom-daemon" ]] && echo 1 || echo 0 )"
assert_eq "non-Darwin: codesign is never invoked" "0" "$( [[ -e "$CODESIGN_MARKER8" ]] && echo 1 || echo 0 )"

# ---------- test 9: signing helper — codesign absent from PATH ----------
# A curated PATH containing only the handful of tools provision_machine_daemon
# actually needs (uname, mkdir, install, cp, chmod, env, bash/sh), deliberately
# excluding codesign. Provisioning must still succeed with at most a warning
# and never attempt to invoke a missing codesign.
NO_CODESIGN_DIR="$WORKDIR/no-codesign-bin"
mkdir -p "$NO_CODESIGN_DIR"
for tool in uname mkdir install cp chmod env bash sh; do
  tool_path="$(command -v "$tool" 2>/dev/null || true)"
  [[ -n "$tool_path" ]] && ln -sf "$tool_path" "$NO_CODESIGN_DIR/$tool"
done

SRC9="$WORKDIR/src9/loom-daemon"
mkdir -p "$WORKDIR/src9"
make_fake_bin "$SRC9" "0.15.3"
DEST9="$WORKDIR/dest9"
# shellcheck disable=SC2034  # captured for ad-hoc debugging, not asserted on
out9=$(PATH="$NO_CODESIGN_DIR" LOOM_DAEMON_BIN_DIR="$DEST9" provision_machine_daemon "$SRC9" 2>&1)
rc9=$?
assert_eq "codesign absent: provision still returns 0" "0" "$rc9"
assert_eq "codesign absent: binary is still provisioned" "1" "$( [[ -x "$DEST9/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 10: signing helper — success path signs $dest_bin with the stable identifier ----------
# Fake `uname` reports Darwin; a fake `codesign` records its argv instead of
# actually signing (the fake binary from make_fake_bin is a plain shell
# script, not a Mach-O, so a REAL codesign invocation is exercised separately
# by test-loom-daemon-update.sh's e2e style; this test isolates the call
# contract: sign_daemon_binary is invoked with the stable --identifier and the
# DEST path, not the source path).
FAKE_OK_DIR="$WORKDIR/fake-codesign-ok-bin"
mkdir -p "$FAKE_OK_DIR"
cat > "$FAKE_OK_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_OK_DIR/uname"
CODESIGN_ARGS_FILE="$WORKDIR/codesign-args.txt"
cat > "$FAKE_OK_DIR/codesign" <<EOF
#!/usr/bin/env bash
echo "\$@" > "$CODESIGN_ARGS_FILE"
exit 0
EOF
chmod +x "$FAKE_OK_DIR/codesign"

SRC10="$WORKDIR/src10/loom-daemon"
mkdir -p "$WORKDIR/src10"
make_fake_bin "$SRC10" "0.15.4"
DEST10="$WORKDIR/dest10"
# shellcheck disable=SC2034  # captured for ad-hoc debugging, not asserted on
out10=$(PATH="$FAKE_OK_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST10" provision_machine_daemon "$SRC10" 2>&1)
rc10=$?
assert_eq "codesign success: provision returns 0" "0" "$rc10"
codesign_args="$(cat "$CODESIGN_ARGS_FILE" 2>/dev/null || echo "<missing>")"
assert_contains "codesign success: invoked with the stable identifier" "$codesign_args" "--identifier com.rjwalters.loom-daemon"
assert_contains "codesign success: signs the installed DEST binary, not the source" "$codesign_args" "$DEST10/loom-daemon"

# ---------- test 11: LOOM_CODESIGN_IDENTITY (#4244) — identity found in the
# keychain -> codesign is invoked WITH that identity (not "-s -"). Fakes
# `uname` (Darwin), `security find-identity` (reports the identity as a valid
# codesigning identity), and `codesign` (records its argv instead of actually
# signing, since the fake binary is a plain shell script, not a Mach-O).
# ---------------------------------------------------------------------------
FAKE_IDENTITY_DIR="$WORKDIR/fake-identity-bin"
mkdir -p "$FAKE_IDENTITY_DIR"
cat > "$FAKE_IDENTITY_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_IDENTITY_DIR/uname"
cat > "$FAKE_IDENTITY_DIR/security" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "find-identity" ]]; then
  echo '  1) ABCDEF1234567890ABCDEF1234567890ABCDEF12 "Loom Local Signing"'
  echo '     1 valid identities found'
  exit 0
fi
exit 1
EOF
chmod +x "$FAKE_IDENTITY_DIR/security"
CODESIGN_ARGS_ID_FILE="$WORKDIR/codesign-args-identity.txt"
cat > "$FAKE_IDENTITY_DIR/codesign" <<EOF
#!/usr/bin/env bash
echo "\$@" > "$CODESIGN_ARGS_ID_FILE"
exit 0
EOF
chmod +x "$FAKE_IDENTITY_DIR/codesign"

SRC11="$WORKDIR/src11/loom-daemon"
mkdir -p "$WORKDIR/src11"
make_fake_bin "$SRC11" "0.15.5"
DEST11="$WORKDIR/dest11"
out11=$(PATH="$FAKE_IDENTITY_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST11" \
  LOOM_CODESIGN_IDENTITY="Loom Local Signing" provision_machine_daemon "$SRC11" 2>&1)
rc11=$?
assert_eq "LOOM_CODESIGN_IDENTITY set + found: provision returns 0" "0" "$rc11"
codesign_args_id="$(cat "$CODESIGN_ARGS_ID_FILE" 2>/dev/null || echo "<missing>")"
assert_contains "LOOM_CODESIGN_IDENTITY set + found: codesign invoked WITH the identity" \
  "$codesign_args_id" "-s Loom Local Signing"
TOTAL=$((TOTAL + 1))
if [[ "$codesign_args_id" != *"-s -"* ]]; then
  echo -e "${GREEN}PASS${NC}: LOOM_CODESIGN_IDENTITY set + found: does NOT fall back to ad-hoc (-s -)"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: LOOM_CODESIGN_IDENTITY set + found: does NOT fall back to ad-hoc (-s -)"
  echo "  actual: '$codesign_args_id'"
  FAIL=$((FAIL + 1))
fi
assert_contains "LOOM_CODESIGN_IDENTITY set + found: still uses the stable --identifier" \
  "$codesign_args_id" "--identifier com.rjwalters.loom-daemon"

# ---------- test 12: LOOM_CODESIGN_IDENTITY (#4244) — identity NOT found in
# the keychain -> falls back to the ad-hoc path unchanged, with a warning.
# ---------------------------------------------------------------------------
FAKE_NO_IDENTITY_DIR="$WORKDIR/fake-no-identity-bin"
mkdir -p "$FAKE_NO_IDENTITY_DIR"
cat > "$FAKE_NO_IDENTITY_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_NO_IDENTITY_DIR/uname"
cat > "$FAKE_NO_IDENTITY_DIR/security" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "find-identity" ]]; then
  echo '     0 valid identities found'
  exit 0
fi
exit 1
EOF
chmod +x "$FAKE_NO_IDENTITY_DIR/security"
CODESIGN_ARGS_NOID_FILE="$WORKDIR/codesign-args-no-identity.txt"
cat > "$FAKE_NO_IDENTITY_DIR/codesign" <<EOF
#!/usr/bin/env bash
echo "\$@" > "$CODESIGN_ARGS_NOID_FILE"
exit 0
EOF
chmod +x "$FAKE_NO_IDENTITY_DIR/codesign"

SRC12="$WORKDIR/src12/loom-daemon"
mkdir -p "$WORKDIR/src12"
make_fake_bin "$SRC12" "0.15.6"
DEST12="$WORKDIR/dest12"
out12=$(PATH="$FAKE_NO_IDENTITY_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST12" \
  LOOM_CODESIGN_IDENTITY="Nonexistent Cert" provision_machine_daemon "$SRC12" 2>&1)
rc12=$?
assert_eq "LOOM_CODESIGN_IDENTITY set but missing: provision returns 0" "0" "$rc12"
codesign_args_noid="$(cat "$CODESIGN_ARGS_NOID_FILE" 2>/dev/null || echo "<missing>")"
assert_contains "LOOM_CODESIGN_IDENTITY missing: falls back to ad-hoc (-s -)" \
  "$codesign_args_noid" "-s -"
assert_contains "LOOM_CODESIGN_IDENTITY missing: warns non-fatally" "$out12" "not found"

# ---------- test 13: LOOM_CODESIGN_IDENTITY unset — byte-identical to the
# pre-#4244 ad-hoc path (regression guard for test 10's assertions, isolated
# from any repo-level codesign.identity config that might exist).
# ---------------------------------------------------------------------------
SRC13="$WORKDIR/src13/loom-daemon"
mkdir -p "$WORKDIR/src13"
make_fake_bin "$SRC13" "0.15.7"
DEST13="$WORKDIR/dest13"
CODESIGN_ARGS_UNSET_FILE="$WORKDIR/codesign-args-unset.txt"
FAKE_UNSET_DIR="$WORKDIR/fake-unset-bin"
mkdir -p "$FAKE_UNSET_DIR"
cat > "$FAKE_UNSET_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_UNSET_DIR/uname"
cat > "$FAKE_UNSET_DIR/codesign" <<EOF
#!/usr/bin/env bash
echo "\$@" > "$CODESIGN_ARGS_UNSET_FILE"
exit 0
EOF
chmod +x "$FAKE_UNSET_DIR/codesign"
out13=$(cd "$WORKDIR" && PATH="$FAKE_UNSET_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST13" \
  env -u LOOM_CODESIGN_IDENTITY -u LOOM_ROOT \
  bash -c 'source "'"$REPO_ROOT"'/scripts/install/provision-daemon.sh"; provision_machine_daemon "'"$SRC13"'"' 2>&1)
rc13=$?
assert_eq "LOOM_CODESIGN_IDENTITY unset: provision returns 0" "0" "$rc13"
codesign_args_unset="$(cat "$CODESIGN_ARGS_UNSET_FILE" 2>/dev/null || echo "<missing>")"
assert_contains "LOOM_CODESIGN_IDENTITY unset: ad-hoc path is unchanged (-s -)" \
  "$codesign_args_unset" "-s -"

# ---------- test 14: binary-format gate (#4397) — refuses a script-based
# "binary" when LOOM_PROVISION_ALLOW_SCRIPT is unset (production behavior).
# ---------------------------------------------------------------------------
SRC14="$WORKDIR/src14/loom-daemon"
mkdir -p "$WORKDIR/src14"
make_fake_bin "$SRC14" "0.16.0"   # a bash script, standing in for the real binary
DEST14="$WORKDIR/dest14"
out14=$( ( unset LOOM_PROVISION_ALLOW_SCRIPT
  LOOM_DAEMON_BIN_DIR="$DEST14" provision_machine_daemon "$SRC14" ) 2>&1 )
rc14=$?
assert_eq "gate rejects a script-based fake binary without the bypass" "1" "$rc14"
assert_contains "gate rejection names the reason" "$out14" "not a compiled binary"
assert_eq "gate rejection installs nothing" "0" "$( [[ -e "$DEST14/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 15: binary-format gate (#4397) — accepts a REAL compiled
# executable (a copy of /bin/cat, standing in for a real Mach-O/ELF daemon
# binary) even without the bypass.
# ---------------------------------------------------------------------------
SRC15="$WORKDIR/src15/loom-daemon"
mkdir -p "$WORKDIR/src15"
cp "$(command -v cat)" "$SRC15"
chmod +x "$SRC15"
DEST15="$WORKDIR/dest15"
out15=$( ( unset LOOM_PROVISION_ALLOW_SCRIPT
  LOOM_DAEMON_BIN_DIR="$DEST15" provision_machine_daemon "$SRC15" ) 2>&1 )
rc15=$?
assert_eq "gate accepts a real compiled binary without the bypass" "0" "$rc15"
assert_eq "real binary is installed at dest" "1" "$( [[ -x "$DEST15/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
