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

# Portable mtime (epoch secs). GNU `stat -c` first (it is an illegal option on
# BSD/macOS, so it fails cleanly there), then BSD `stat -f %m`. The reverse
# order MISFIRES on GNU: `stat -f %m <path>` there means --file-system (a bare
# mode flag), which prints a multi-line filesystem report to STDOUT while
# exiting non-zero — `2>/dev/null || fallback` only silences stderr, so the
# report leaks into the captured value and corrupts the comparison below.
file_mtime() {
  local v
  v="$(stat -c %Y "$1" 2>/dev/null || true)"
  [[ "$v" =~ ^[0-9]+$ ]] || v="$(stat -f %m "$1" 2>/dev/null || true)"
  [[ "$v" =~ ^[0-9]+$ ]] || v=""
  printf '%s\n' "$v"
}

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
before_mtime=$(file_mtime "$DEST1/loom-daemon")
sleep 1
out2=$(LOOM_DAEMON_BIN_DIR="$DEST1" provision_machine_daemon "$SRC1" 2>&1)
rc2=$?
after_mtime=$(file_mtime "$DEST1/loom-daemon")
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

# ---------- test 16: certificate-signed binaries are never force-resigned
# (#5020, epic #4990 Phase 3) — a FETCHED release artifact carries a real
# Developer ID signature (Phase 2, #5011/#5018), and `codesign -f` would
# unconditionally REPLACE it with an ad-hoc signature that has no certificate
# chain. sign_daemon_binary must detect the existing Authority and skip.
# ---------------------------------------------------------------------------
FAKE_SIGNED_DIR="$WORKDIR/fake-signed-bin"
mkdir -p "$FAKE_SIGNED_DIR"
cat > "$FAKE_SIGNED_DIR/uname" <<'EOF'
#!/usr/bin/env bash
echo "Darwin"
EOF
chmod +x "$FAKE_SIGNED_DIR/uname"
CODESIGN_SIGNED_ARGS_FILE="$WORKDIR/codesign-args-signed.txt"
# `-dvvv` reports a certificate-backed signature (Authority on stderr, exactly
# where real codesign prints it); ANY other invocation records its argv so the
# assertion below can prove no re-signing was attempted.
cat > "$FAKE_SIGNED_DIR/codesign" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "-dvvv" ]]; then
  echo "Identifier=com.rjwalters.loom-daemon" >&2
  echo "Authority=Developer ID Application: Test Authority (TESTTEAM)" >&2
  exit 0
fi
echo "\$@" >> "$CODESIGN_SIGNED_ARGS_FILE"
exit 0
EOF
chmod +x "$FAKE_SIGNED_DIR/codesign"

SRC16="$WORKDIR/src16/loom-daemon"
mkdir -p "$WORKDIR/src16"
make_fake_bin "$SRC16" "0.17.0"
DEST16="$WORKDIR/dest16"
out16=$(PATH="$FAKE_SIGNED_DIR:$PATH" LOOM_DAEMON_BIN_DIR="$DEST16" provision_machine_daemon "$SRC16" 2>&1)
rc16=$?
assert_eq "already-signed: provision returns 0" "0" "$rc16"
assert_eq "already-signed: binary is still provisioned" "1" "$( [[ -x "$DEST16/loom-daemon" ]] && echo 1 || echo 0 )"
assert_contains "already-signed: says it is not re-signing" "$out16" "already signed with a real certificate"
assert_eq "already-signed: codesign -f is NEVER invoked (no ad-hoc downgrade)" "0" \
  "$( [[ -s "$CODESIGN_SIGNED_ARGS_FILE" ]] && echo 1 || echo 0 )"

# ---------- test 17: shims (#4272/#4275) — fresh install writes all three
# working, executable PATH shims alongside the binary.
# ---------------------------------------------------------------------------
SRC17="$WORKDIR/src17/loom-daemon"
mkdir -p "$WORKDIR/src17"
make_fake_bin "$SRC17" "0.18.0"
DEST17="$WORKDIR/dest17"
provision_machine_daemon "$SRC17" "$DEST17" >/dev/null 2>&1
for shim in loom-clean loom-recover-orphans loom-claim; do
  assert_eq "fresh install: $shim shim is executable" "1" \
    "$( [[ -x "$DEST17/$shim" ]] && echo 1 || echo 0 )"
done
assert_contains "fresh install: loom-clean shim execs the clean subcommand" \
  "$(cat "$DEST17/loom-clean")" 'loom-daemon" clean "$@"'
assert_contains "fresh install: loom-recover-orphans shim execs the recover-orphans subcommand" \
  "$(cat "$DEST17/loom-recover-orphans")" 'loom-daemon" recover-orphans "$@"'
assert_contains "fresh install: loom-claim shim execs the claim subcommand" \
  "$(cat "$DEST17/loom-claim")" 'loom-daemon" claim "$@"'

# ---------- test 18: shims — the version-match short-circuit path (the
# "already current at ..." branch every #5386 repro hit) also (re)installs
# all three shims, not just the fresh-install path.
# ---------------------------------------------------------------------------
rm -f "$DEST17/loom-clean" "$DEST17/loom-recover-orphans" "$DEST17/loom-claim"
out18=$(provision_machine_daemon "$SRC17" "$DEST17" 2>&1)
assert_contains "short-circuit run reports already current" "$out18" "already current"
for shim in loom-clean loom-recover-orphans loom-claim; do
  assert_eq "short-circuit run: $shim shim is (re)installed" "1" \
    "$( [[ -x "$DEST17/$shim" ]] && echo 1 || echo 0 )"
done

# ---------- test 19: shims (#5386 root cause) — _pmd_install_shim self-heals
# a dest_dir that does not exist yet, instead of failing with a bare
# "No such file or directory" the way the version-match short-circuit branch
# used to (it never called `mkdir -p` before writing a shim, unlike the
# fresh-install branch). This directly regression-tests the fix.
# ---------------------------------------------------------------------------
DEST19="$WORKDIR/dest19-not-yet-created"
assert_eq "pre-condition: dest19 does not exist yet" "0" \
  "$( [[ -d "$DEST19" ]] && echo 1 || echo 0 )"
out19=$(_pmd_install_shim "loom-clean" "clean" "$DEST19" 2>&1)
assert_eq "missing dest_dir: shim install still returns 0 (self-heals)" "0" "$?"
assert_eq "missing dest_dir: shim is installed after self-heal" "1" \
  "$( [[ -x "$DEST19/loom-clean" ]] && echo 1 || echo 0 )"
TOTAL=$((TOTAL + 1))
if [[ -z "$out19" ]]; then
  echo -e "${GREEN}PASS${NC}: missing dest_dir: self-heal emits no warning (fully repaired)"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: missing dest_dir: self-heal emits no warning (fully repaired)"
  echo "  unexpected output: '$out19'"
  FAIL=$((FAIL + 1))
fi

# ---------- test 20: shims — a dest_dir that exists but is NOT writable
# gets an ACTIONABLE diagnostic naming the real reason, not a bare
# "No such file or directory" bash redirection error.
# ---------------------------------------------------------------------------
DEST20="$WORKDIR/dest20-readonly"
mkdir -p "$DEST20"
chmod 555 "$DEST20"
out20=$(_pmd_install_shim "loom-clean" "clean" "$DEST20" 2>&1)
rc20=$?
chmod 755 "$DEST20"  # restore so the trap's `rm -rf "$WORKDIR"` can clean up
assert_eq "unwritable dest_dir: shim install is still non-fatal (returns 0)" "0" "$rc20"
assert_contains "unwritable dest_dir: warning names the real reason" "$out20" "not writable"
assert_eq "unwritable dest_dir: no shim file left behind" "0" \
  "$( [[ -e "$DEST20/loom-clean" ]] && echo 1 || echo 0 )"

# ---------- test 21: shims — re-running install repairs a host where the
# shims are missing (acceptance criterion from #5386): delete the shims
# (simulating a host that never got them), leave the daemon binary in
# place, and confirm the NEXT install run restores all three.
# ---------------------------------------------------------------------------
SRC21="$WORKDIR/src21/loom-daemon"
mkdir -p "$WORKDIR/src21"
make_fake_bin "$SRC21" "0.18.1"
DEST21="$WORKDIR/dest21"
provision_machine_daemon "$SRC21" "$DEST21" >/dev/null 2>&1
rm -f "$DEST21/loom-clean" "$DEST21/loom-recover-orphans" "$DEST21/loom-claim"
for shim in loom-clean loom-recover-orphans loom-claim; do
  assert_eq "repair pre-condition: $shim is missing" "0" \
    "$( [[ -e "$DEST21/$shim" ]] && echo 1 || echo 0 )"
done
provision_machine_daemon "$SRC21" "$DEST21" >/dev/null 2>&1
for shim in loom-clean loom-recover-orphans loom-claim; do
  assert_eq "repair: re-running install restores the missing $shim shim" "1" \
    "$( [[ -x "$DEST21/$shim" ]] && echo 1 || echo 0 )"
done

# ---------- test 22: defaults payload (#5389) — omitted defaults_src_dir is
# a silent no-op (e.g. loom-daemon-update.sh's LOOM_DAEMON_BIN override path,
# or any caller with no source tree to mirror from). Provisioning must still
# succeed and must not create anything at the machine-level defaults dest.
# ---------------------------------------------------------------------------
SRC22="$WORKDIR/src22/loom-daemon"
mkdir -p "$WORKDIR/src22"
make_fake_bin "$SRC22" "0.19.0"
DEST22="$WORKDIR/dest22"
DEFAULTS_DEST22="$WORKDIR/machine-defaults-22/defaults"
out22=$(LOOM_DAEMON_DEFAULTS_DIR="$DEFAULTS_DEST22" provision_machine_daemon "$SRC22" "$DEST22" 2>&1)
rc22=$?
assert_eq "no defaults_src_dir: provision still returns 0" "0" "$rc22"
assert_eq "no defaults_src_dir: no machine-level defaults dir created" "0" \
  "$( [[ -e "$DEFAULTS_DEST22" ]] && echo 1 || echo 0 )"

# ---------- test 23: defaults payload (#5389) — a real defaults_src_dir is
# mirrored to LOOM_DAEMON_DEFAULTS_DIR, giving a standalone install a working
# `loom-daemon init` recovery path.
# ---------------------------------------------------------------------------
SRC23="$WORKDIR/src23/loom-daemon"
mkdir -p "$WORKDIR/src23"
make_fake_bin "$SRC23" "0.19.1"
DEST23="$WORKDIR/dest23"
DEFAULTS_SRC23="$WORKDIR/defaults-src-23"
mkdir -p "$DEFAULTS_SRC23/roles"
echo '{}' > "$DEFAULTS_SRC23/config.json"
echo 'builder role' > "$DEFAULTS_SRC23/roles/builder.md"
DEFAULTS_DEST23="$WORKDIR/machine-defaults-23/defaults"
out23=$(LOOM_DAEMON_DEFAULTS_DIR="$DEFAULTS_DEST23" \
  provision_machine_daemon "$SRC23" "$DEST23" "$DEFAULTS_SRC23" 2>&1)
rc23=$?
assert_eq "defaults_src_dir given: provision returns 0" "0" "$rc23"
assert_eq "defaults_src_dir given: config.json mirrored" "1" \
  "$( [[ -f "$DEFAULTS_DEST23/config.json" ]] && echo 1 || echo 0 )"
assert_eq "defaults_src_dir given: roles/builder.md mirrored" "1" \
  "$( [[ -f "$DEFAULTS_DEST23/roles/builder.md" ]] && echo 1 || echo 0 )"
assert_contains "defaults_src_dir given: output confirms the mirror" "$out23" "mirrored defaults payload"

# ---------- test 24: defaults payload (#5389) — re-provisioning removes
# stale files from a previous mirror (mirror, not additive union).
# ---------------------------------------------------------------------------
rm -f "$DEFAULTS_SRC23/roles/builder.md"
echo 'judge role' > "$DEFAULTS_SRC23/roles/judge.md"
LOOM_DAEMON_DEFAULTS_DIR="$DEFAULTS_DEST23" \
  provision_machine_daemon "$SRC23" "$DEST23" "$DEFAULTS_SRC23" >/dev/null 2>&1
assert_eq "re-mirror: stale roles/builder.md removed" "0" \
  "$( [[ -e "$DEFAULTS_DEST23/roles/builder.md" ]] && echo 1 || echo 0 )"
assert_eq "re-mirror: new roles/judge.md present" "1" \
  "$( [[ -f "$DEFAULTS_DEST23/roles/judge.md" ]] && echo 1 || echo 0 )"

# ---------- test 25: defaults payload (#5389) — a nonexistent
# defaults_src_dir is a silent no-op (never fatal to the caller).
# ---------------------------------------------------------------------------
SRC25="$WORKDIR/src25/loom-daemon"
mkdir -p "$WORKDIR/src25"
make_fake_bin "$SRC25" "0.19.2"
DEST25="$WORKDIR/dest25"
DEFAULTS_DEST25="$WORKDIR/machine-defaults-25/defaults"
out25=$(LOOM_DAEMON_DEFAULTS_DIR="$DEFAULTS_DEST25" \
  provision_machine_daemon "$SRC25" "$DEST25" "$WORKDIR/does-not-exist-defaults" 2>&1)
rc25=$?
assert_eq "missing defaults_src_dir: provision still returns 0" "0" "$rc25"
assert_eq "missing defaults_src_dir: no machine-level defaults dir created" "0" \
  "$( [[ -e "$DEFAULTS_DEST25" ]] && echo 1 || echo 0 )"

# ---------- test 26: defaults payload (#5389) — a misconfigured
# LOOM_DAEMON_DEFAULTS_DIR that does not end in a 'defaults' leaf is refused
# rather than silently mirroring into an unintended wide target.
# ---------------------------------------------------------------------------
SRC26="$WORKDIR/src26/loom-daemon"
mkdir -p "$WORKDIR/src26"
make_fake_bin "$SRC26" "0.19.3"
DEST26="$WORKDIR/dest26"
DEFAULTS_SRC26="$WORKDIR/defaults-src-26"
mkdir -p "$DEFAULTS_SRC26"
echo '{}' > "$DEFAULTS_SRC26/config.json"
out26=$(LOOM_DAEMON_DEFAULTS_DIR="$WORKDIR/not-a-defaults-dir" \
  provision_machine_daemon "$SRC26" "$DEST26" "$DEFAULTS_SRC26" 2>&1)
assert_contains "misconfigured LOOM_DAEMON_DEFAULTS_DIR: refuses with a clear warning" \
  "$out26" "unexpected destination"
assert_eq "misconfigured LOOM_DAEMON_DEFAULTS_DIR: binary is still provisioned (soft failure only)" "1" \
  "$( [[ -x "$DEST26/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 27: shims (#5706) — a DANGLING symlink at shim_path (the
# population left behind by the loom-tools/ Python retirement, #4971) is
# unlinked before the write, so the shim self-heals to a regular executable
# file instead of failing when `>` tries to follow the symlink to its
# missing target.
# ---------------------------------------------------------------------------
DEST27="$WORKDIR/dest27"
mkdir -p "$DEST27"
ln -s "$WORKDIR/dest27/nonexistent-target/loom-clean" "$DEST27/loom-clean"
assert_eq "pre-condition: loom-clean is a dangling symlink" "1" \
  "$( [[ -L "$DEST27/loom-clean" && ! -e "$DEST27/loom-clean" ]] && echo 1 || echo 0 )"
out27=$(_pmd_install_shim "loom-clean" "clean" "$DEST27" 2>&1)
rc27=$?
assert_eq "dangling symlink: shim install returns 0" "0" "$rc27"
assert_eq "dangling symlink: shim is replaced by a regular executable file" "1" \
  "$( [[ -f "$DEST27/loom-clean" && ! -L "$DEST27/loom-clean" && -x "$DEST27/loom-clean" ]] && echo 1 || echo 0 )"
assert_contains "dangling symlink: repaired shim execs the clean subcommand" \
  "$(cat "$DEST27/loom-clean")" 'loom-daemon" clean "$@"'
TOTAL=$((TOTAL + 1))
if [[ -z "$out27" ]]; then
  echo -e "${GREEN}PASS${NC}: dangling symlink: repair emits no warning"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: dangling symlink: repair emits no warning"
  echo "  unexpected output: '$out27'"
  FAIL=$((FAIL + 1))
fi

# ---------- test 28: shims (#5706) — a pre-existing REGULAR file at
# shim_path is still overwritten idempotently (the rm -f fix must not
# regress the normal reinstall-over-a-real-shim case).
# ---------------------------------------------------------------------------
DEST28="$WORKDIR/dest28"
mkdir -p "$DEST28"
echo "stale shim content" > "$DEST28/loom-clean"
chmod 755 "$DEST28/loom-clean"
out28=$(_pmd_install_shim "loom-clean" "clean" "$DEST28" 2>&1)
rc28=$?
assert_eq "pre-existing regular file: shim install returns 0" "0" "$rc28"
assert_eq "pre-existing regular file: shim is a regular executable file" "1" \
  "$( [[ -f "$DEST28/loom-clean" && ! -L "$DEST28/loom-clean" && -x "$DEST28/loom-clean" ]] && echo 1 || echo 0 )"
assert_contains "pre-existing regular file: overwritten shim execs the clean subcommand" \
  "$(cat "$DEST28/loom-clean")" 'loom-daemon" clean "$@"'
TOTAL=$((TOTAL + 1))
if [[ -z "$out28" ]]; then
  echo -e "${GREEN}PASS${NC}: pre-existing regular file: idempotent overwrite emits no warning"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: pre-existing regular file: idempotent overwrite emits no warning"
  echo "  unexpected output: '$out28'"
  FAIL=$((FAIL + 1))
fi

# ---------- test 29: shims (#5706 acceptance criterion) — re-running full
# install on a host with dangling loom-* symlinks (the loom-tools retirement
# population) leaves `command -v loom-clean` resolving to a regular file
# that execs `loom-daemon clean`.
# ---------------------------------------------------------------------------
SRC29="$WORKDIR/src29/loom-daemon"
mkdir -p "$WORKDIR/src29"
make_fake_bin "$SRC29" "0.19.4"
DEST29="$WORKDIR/dest29"
mkdir -p "$DEST29"
for shim in loom-clean loom-recover-orphans loom-claim; do
  ln -s "$DEST29/dangling-target-does-not-exist/$shim" "$DEST29/$shim"
done
provision_machine_daemon "$SRC29" "$DEST29" >/dev/null 2>&1
for shim in loom-clean loom-recover-orphans loom-claim; do
  assert_eq "reinstall over dangling symlinks: $shim resolves to a regular executable" "1" \
    "$( [[ -f "$DEST29/$shim" && ! -L "$DEST29/$shim" && -x "$DEST29/$shim" ]] && echo 1 || echo 0 )"
done

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
