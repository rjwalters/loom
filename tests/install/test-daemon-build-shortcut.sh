#!/usr/bin/env bash
# Test suite for install.sh::loom_daemon_dest_binary_current() (issue #4897).
#
# Usage: ./tests/install/test-daemon-build-shortcut.sh
#
# install.sh runs top-level installer logic when sourced, so we extract just
# the loom_daemon_dest_binary_current() function definition (the pre-build
# short-circuit that lets a `--quick`/Quick Install skip `pnpm daemon:build`
# -- and the `target/.cargo-lock` wait it can trigger -- when a machine-level
# installed binary is already built from source HEAD) and eval it in
# isolation, mirroring the extraction pattern in test-hooks-preserve.sh.
#
# Uses a FAKE loom-daemon binary that prints a settable `--version` string;
# no real cargo build or network access needed.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

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

# Extract the loom_daemon_dest_binary_current() function body from install.sh
# and define it here. awk grabs from the function header to the first
# closing brace at column 0 (same technique as test-hooks-preserve.sh).
extract_fn() {
  local fn="$1" src
  src="$(awk -v fn="^$1\\\\(\\\\) \\\\{" '$0 ~ fn {f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
  if [[ -z "$src" ]]; then
    echo -e "${RED}FATAL${NC}: could not extract $fn() from $INSTALL_SH"
    exit 1
  fi
  eval "$src"
}

extract_fn loom_daemon_dest_binary_current
# #5922: these two also key off Cargo's resolved target dir, so they are
# exercised below against a redirected LOOM_CARGO_TARGET_DIR.
extract_fn loom_daemon_binary_stale
extract_fn resolve_cargo_target_dir

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Build a fake loom-daemon binary that prints $2 as its --version output.
make_fake_bin() {
  local path="$1" version_output="$2"
  cat > "$path" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-}" == "--version" ]]; then echo "$version_output"; fi
EOF
  chmod +x "$path"
}

# Build a minimal git repo standing in for LOOM_ROOT, and echo its HEAD short
# commit so tests can construct a matching / mismatching fake --version.
make_loom_root() {
  local root="$1"
  mkdir -p "$root"
  git -C "$root" init -q
  git -C "$root" config user.email "test@example.com"
  git -C "$root" config user.name "Test"
  echo "placeholder" > "$root/placeholder.txt"
  git -C "$root" add placeholder.txt
  git -C "$root" commit -q -m "initial commit"
  git -C "$root" rev-parse --short HEAD
}

# ---------- test 1: no installed binary at the destination -> build still needed ----------
ROOT1="$WORKDIR/root1"
make_loom_root "$ROOT1" >/dev/null
DEST1="$WORKDIR/dest1"
mkdir -p "$DEST1"
rc1=0
LOOM_DAEMON_BIN_DIR="$DEST1" loom_daemon_dest_binary_current "$ROOT1" || rc1=$?
assert_eq "missing dest binary returns 1 (build needed)" "1" "$rc1"
assert_eq "missing dest binary: no target/release copy made" "0" \
  "$( [[ -e "$ROOT1/target/release/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 2: dest binary present but commit MISMATCHES source HEAD ----------
ROOT2="$WORKDIR/root2"
make_loom_root "$ROOT2" >/dev/null
DEST2="$WORKDIR/dest2"
mkdir -p "$DEST2"
make_fake_bin "$DEST2/loom-daemon" "0.17.0 (commit deadbee, built 2026-01-01T00:00:00Z)"
rc2=0
LOOM_DAEMON_BIN_DIR="$DEST2" loom_daemon_dest_binary_current "$ROOT2" || rc2=$?
assert_eq "mismatched commit returns 1 (build needed)" "1" "$rc2"
assert_eq "mismatched commit: no target/release copy made" "0" \
  "$( [[ -e "$ROOT2/target/release/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 3: dest binary present with commit "unknown" (no git at build time) ----------
ROOT3="$WORKDIR/root3"
make_loom_root "$ROOT3" >/dev/null
DEST3="$WORKDIR/dest3"
mkdir -p "$DEST3"
make_fake_bin "$DEST3/loom-daemon" "0.17.0 (commit unknown, built 2026-01-01T00:00:00Z)"
rc3=0
LOOM_DAEMON_BIN_DIR="$DEST3" loom_daemon_dest_binary_current "$ROOT3" || rc3=$?
assert_eq "unknown commit returns 1 (build needed)" "1" "$rc3"

# ---------- test 4: dest binary present but --version output unparsable ----------
ROOT4="$WORKDIR/root4"
make_loom_root "$ROOT4" >/dev/null
DEST4="$WORKDIR/dest4"
mkdir -p "$DEST4"
make_fake_bin "$DEST4/loom-daemon" "0.17.0"
rc4=0
LOOM_DAEMON_BIN_DIR="$DEST4" loom_daemon_dest_binary_current "$ROOT4" || rc4=$?
assert_eq "unparsable --version returns 1 (build needed)" "1" "$rc4"

# ---------- test 5: dest binary present and commit MATCHES source HEAD -> skip the build ----------
ROOT5="$WORKDIR/root5"
HEAD5="$(make_loom_root "$ROOT5")"
DEST5="$WORKDIR/dest5"
mkdir -p "$DEST5"
make_fake_bin "$DEST5/loom-daemon" "0.17.0 (commit $HEAD5, built 2026-01-01T00:00:00Z)"
rc5=0
LOOM_DAEMON_BIN_DIR="$DEST5" loom_daemon_dest_binary_current "$ROOT5" || rc5=$?
assert_eq "matching commit returns 0 (build skipped)" "0" "$rc5"
assert_eq "matching commit: copies dest binary into target/release/loom-daemon" "1" \
  "$( [[ -x "$ROOT5/target/release/loom-daemon" ]] && echo 1 || echo 0 )"
assert_eq "matching commit: copied binary reports the same --version" \
  "0.17.0 (commit $HEAD5, built 2026-01-01T00:00:00Z)" \
  "$("$ROOT5/target/release/loom-daemon" --version)"

# ---------- test 6: LOOM_DAEMON_BIN_DIR unset falls back to $HOME/.local/bin ----------
# Point HOME at a throwaway dir with no installed binary; must still return 1
# (no crash resolving the default dest dir).
ROOT6="$WORKDIR/root6"
make_loom_root "$ROOT6" >/dev/null
FAKE_HOME6="$WORKDIR/home6"
mkdir -p "$FAKE_HOME6"
rc6=0
( unset LOOM_DAEMON_BIN_DIR; HOME="$FAKE_HOME6" loom_daemon_dest_binary_current "$ROOT6" ) || rc6=$?
assert_eq "default dest dir (no LOOM_DAEMON_BIN_DIR) with nothing installed returns 1" "1" "$rc6"

# ==========================================================================
# Issue #5922: a redirected Cargo target directory
# ==========================================================================
# Cargo's build output is NOT necessarily <repo>/target -- `build.target-dir`
# in ~/.cargo/config.toml or CARGO_TARGET_DIR relocates it wholesale. Every
# daemon-binary path in install.sh must follow it, or the installer looks for
# (and copies to) a directory the build never wrote to, aborting with a
# misleading "Failed to build loom-daemon" after a build that actually
# succeeded.

# ---------- test 7: dest-binary copy follows LOOM_CARGO_TARGET_DIR ----------
ROOT7="$WORKDIR/root7"
HEAD7="$(make_loom_root "$ROOT7")"
DEST7="$WORKDIR/dest7"
mkdir -p "$DEST7"
make_fake_bin "$DEST7/loom-daemon" "0.17.0 (commit $HEAD7, built 2026-01-01T00:00:00Z)"
REDIRECTED7="$WORKDIR/redirected-target-7"
rc7=0
LOOM_CARGO_TARGET_DIR="$REDIRECTED7" LOOM_DAEMON_BIN_DIR="$DEST7" \
  loom_daemon_dest_binary_current "$ROOT7" || rc7=$?
assert_eq "redirected target dir: matching commit still returns 0 (build skipped)" "0" "$rc7"
assert_eq "redirected target dir: copy lands under LOOM_CARGO_TARGET_DIR/release" "1" \
  "$( [[ -x "$REDIRECTED7/release/loom-daemon" ]] && echo 1 || echo 0 )"
assert_eq "redirected target dir: nothing written to the default \$ROOT/target" "0" \
  "$( [[ -e "$ROOT7/target/release/loom-daemon" ]] && echo 1 || echo 0 )"

# ---------- test 8: staleness check reads the redirected target dir ----------
ROOT8="$WORKDIR/root8"
make_loom_root "$ROOT8" >/dev/null
REDIRECTED8="$WORKDIR/redirected-target-8"
mkdir -p "$REDIRECTED8/release"
# A binary NEWER than every source file -> not stale. Without the #5922 fix
# this reports "stale" (really: "missing"), because it looks under
# $ROOT8/target where a redirected build never writes.
touch "$REDIRECTED8/release/loom-daemon"
rc8=0
LOOM_CARGO_TARGET_DIR="$REDIRECTED8" loom_daemon_binary_stale "$ROOT8" || rc8=$?
assert_eq "redirected target dir: an up-to-date binary there is NOT reported stale" "1" "$rc8"
# Same tree with no binary in the redirected dir -> stale (build needed).
REDIRECTED8B="$WORKDIR/redirected-target-8b"
rc8b=0
LOOM_CARGO_TARGET_DIR="$REDIRECTED8B" loom_daemon_binary_stale "$ROOT8" || rc8b=$?
assert_eq "redirected target dir: a missing binary there IS reported stale" "0" "$rc8b"

# ---------- test 9: resolve_cargo_target_dir honors CARGO_TARGET_DIR ----------
# Each case runs inside a command-substitution subshell so the function's
# LOOM_CARGO_TARGET_DIR cache cannot leak between cases.
# scripts/cargo-target-dir.sh short-circuits on CARGO_TARGET_DIR, so this
# needs neither a Rust toolchain nor jq.
RESOLVED9="$(
  LOOM_CARGO_TARGET_DIR=""
  export CARGO_TARGET_DIR="$WORKDIR/env-redirected-9"
  resolve_cargo_target_dir "$REPO_ROOT"
  printf '%s\n' "$LOOM_CARGO_TARGET_DIR"
)"
assert_eq "resolve_cargo_target_dir follows CARGO_TARGET_DIR" \
  "$WORKDIR/env-redirected-9" "$RESOLVED9"

# Default configuration (nothing to redirect to) must still resolve to the
# root's own target/ -- the exact pre-#5922 behavior. Uses a synthetic root
# carrying a copy of the helper but NO cargo manifest, so the assertion holds
# regardless of whether the HOST running these tests has its own
# build.target-dir redirect configured (asserting against $REPO_ROOT/target
# directly would fail on precisely the host that reported #5922).
FAKEROOT9B="$WORKDIR/fakeroot9b"
mkdir -p "$FAKEROOT9B/scripts"
cp "$REPO_ROOT/scripts/cargo-target-dir.sh" "$FAKEROOT9B/scripts/cargo-target-dir.sh"
RESOLVED9B="$(
  LOOM_CARGO_TARGET_DIR=""
  unset CARGO_TARGET_DIR
  resolve_cargo_target_dir "$FAKEROOT9B"
  printf '%s\n' "$LOOM_CARGO_TARGET_DIR"
)"
assert_eq "resolve_cargo_target_dir default (no redirect) is <root>/target" \
  "$FAKEROOT9B/target" "$RESOLVED9B"

# A LOOM_ROOT with no scripts/cargo-target-dir.sh (partial checkout) must fall
# back to the pre-#5922 assumption rather than resolving to an empty path.
RESOLVED9C="$(
  LOOM_CARGO_TARGET_DIR=""
  unset CARGO_TARGET_DIR
  resolve_cargo_target_dir "$WORKDIR/no-such-loom-root"
  printf '%s\n' "$LOOM_CARGO_TARGET_DIR"
)"
assert_eq "resolve_cargo_target_dir falls back to <root>/target without the helper script" \
  "$WORKDIR/no-such-loom-root/target" "$RESOLVED9C"

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
