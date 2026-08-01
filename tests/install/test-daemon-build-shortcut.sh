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
_FN_SRC="$(awk '/^loom_daemon_dest_binary_current\(\) \{/{f=1} f{print} f&&/^}$/{exit}' "$INSTALL_SH")"
if [[ -z "$_FN_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract loom_daemon_dest_binary_current() from $INSTALL_SH"
  exit 1
fi
eval "$_FN_SRC"

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

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
