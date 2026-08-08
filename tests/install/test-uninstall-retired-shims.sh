#!/usr/bin/env bash
# Test suite for the retired `loom-*` PATH-shim cleanup in
# scripts/uninstall-loom.sh (issue #5738).
#
# Usage: ./tests/install/test-uninstall-retired-shims.sh
#
# Background: a pre-#4971 install symlinked fourteen `loom-*` names in the
# machine bin dir into `<repo>/loom-tools/.venv/bin/`. #4971 deleted that venv,
# so eleven of them became permanently dangling PATH entries that nothing
# regenerated and nothing removed — "no supported way to remove them short of
# manual deletion" (#5738). Uninstall now unlinks them.
#
# This drives the REAL uninstall end-to-end in `--local --yes` mode against a
# throwaway git repo, with LOOM_DAEMON_BIN_DIR pointed at a throwaway bin dir,
# so nothing outside $WORKDIR is ever touched. It pins both halves of the
# acceptance criteria:
#
#   1. dangling loom-tools/.venv shims ARE removed by an uninstall run;
#   2. a user-authored `loom-*` script, a LIVE venv symlink, and a dangling
#      symlink pointing somewhere other than a loom-tools/.venv are all
#      PRESERVED — the removal can never take out something that still works
#      or something the operator wrote.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"

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
    FAIL=$((FAIL + 1))
  fi
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# ---------------------------------------------------------------------------
# Fixture: a minimal consumer repo with Loom "installed", plus a throwaway
# machine bin dir standing in for ~/.local/bin.
# ---------------------------------------------------------------------------
make_target_repo() {
  local target="$1"
  mkdir -p "$target/.loom/roles"
  echo '{}' > "$target/.loom/config.json"
  cp "$REPO_ROOT/defaults/roles/builder.md" "$target/.loom/roles/builder.md"
  git -C "$target" init -q .
  git -C "$target" config user.email "test@example.com"
  git -C "$target" config user.name "Test"
  git -C "$target" add -A
  git -C "$target" commit -qm "init"
}

# Plants one fixture of every shape the predicate must distinguish.
make_bin_dir() {
  local bin="$1" live_venv="$2"

  # (1) DEAD: dangling symlinks into a loom-tools/.venv that no longer exists.
  #     These are the eleven-name population — must be removed.
  ln -s "$WORKDIR/retired-checkout/loom-tools/.venv/bin/loom-status" "$bin/loom-status"
  ln -s "$WORKDIR/retired-checkout/loom-tools/.venv/bin/loom-cleanup" "$bin/loom-cleanup"
  ln -s "$WORKDIR/retired-checkout/loom-tools/.venv/bin/loom-worktree" "$bin/loom-worktree"

  # (2) SAFE: a regular file the operator wrote that happens to be named
  #     `loom-*` (one colliding with a retired name, one not).
  printf '#!/bin/sh\necho user-authored\n' > "$bin/loom-forge"
  chmod 755 "$bin/loom-forge"
  printf '#!/bin/sh\necho user-authored\n' > "$bin/loom-my-helper"
  chmod 755 "$bin/loom-my-helper"

  # (3) SAFE: a LIVE symlink into a loom-tools/.venv that still exists.
  mkdir -p "$live_venv"
  printf '#!/bin/sh\necho live\n' > "$live_venv/loom-health-monitor"
  chmod 755 "$live_venv/loom-health-monitor"
  ln -s "$live_venv/loom-health-monitor" "$bin/loom-health-monitor"

  # (4) SAFE: a dangling symlink whose target is NOT inside a loom-tools/.venv.
  ln -s "$WORKDIR/elsewhere/bin/loom-auto-merge" "$bin/loom-auto-merge"

  # (5) SAFE: the live managed shims — owned by _pmd_install_shim, never in
  #     scope for the retired-shim cleanup.
  printf '#!/usr/bin/env bash\nexec loom-daemon clean "$@"\n' > "$bin/loom-clean"
  chmod 755 "$bin/loom-clean"
}

# ---------- test 1: --dry-run reports the dead shims and removes NOTHING ----
TARGET1="$WORKDIR/target1"
BIN1="$WORKDIR/bin1"
mkdir -p "$BIN1"
make_target_repo "$TARGET1"
make_bin_dir "$BIN1" "$WORKDIR/live-checkout-1/loom-tools/.venv/bin"

out1="$(LOOM_DAEMON_BIN_DIR="$BIN1" bash "$UNINSTALL_SH" --dry-run --local "$TARGET1" 2>&1)"
assert_contains "dry-run counts the dead shims" "$out1" "Dead machine-level loom-* PATH shims: 3"
assert_contains "dry-run names the unlink plan" "$out1" "Would also unlink 3 broken machine-level loom-* PATH shim(s)"
assert_eq "dry-run removes nothing" "8" "$(ls -1 "$BIN1" | wc -l | tr -d ' ')"
for name in loom-status loom-cleanup loom-worktree; do
  assert_eq "dry-run leaves $name in place" "1" \
    "$( [[ -L "$BIN1/$name" ]] && echo 1 || echo 0 )"
done

# ---------- test 2: a real uninstall run removes exactly the dead shims -----
TARGET2="$WORKDIR/target2"
BIN2="$WORKDIR/bin2"
LIVE_VENV2="$WORKDIR/live-checkout-2/loom-tools/.venv/bin"
mkdir -p "$BIN2"
make_target_repo "$TARGET2"
make_bin_dir "$BIN2" "$LIVE_VENV2"

out2="$(LOOM_DAEMON_BIN_DIR="$BIN2" bash "$UNINSTALL_SH" --yes --local "$TARGET2" 2>&1)"
rc2=$?
assert_eq "uninstall exits 0" "0" "$rc2"

for name in loom-status loom-cleanup loom-worktree; do
  assert_eq "uninstall removes dangling loom-tools/.venv shim $name" "0" \
    "$( [[ -e "$BIN2/$name" || -L "$BIN2/$name" ]] && echo 1 || echo 0 )"
done
assert_contains "uninstall reports each removal" "$out2" "removed retired shim loom-status"

assert_eq "guardrail: user-authored regular file at a retired name preserved" "1" \
  "$( [[ -f "$BIN2/loom-forge" && ! -L "$BIN2/loom-forge" ]] && echo 1 || echo 0 )"
assert_eq "guardrail: user-authored loom-my-helper preserved" "1" \
  "$( [[ -f "$BIN2/loom-my-helper" ]] && echo 1 || echo 0 )"
assert_eq "guardrail: LIVE loom-tools/.venv symlink preserved" "1" \
  "$( [[ -L "$BIN2/loom-health-monitor" && -e "$BIN2/loom-health-monitor" ]] && echo 1 || echo 0 )"
assert_eq "guardrail: dangling symlink outside a loom-tools/.venv preserved" "1" \
  "$( [[ -L "$BIN2/loom-auto-merge" ]] && echo 1 || echo 0 )"
assert_eq "guardrail: managed loom-clean shim preserved" "1" \
  "$( [[ -f "$BIN2/loom-clean" && -x "$BIN2/loom-clean" ]] && echo 1 || echo 0 )"
assert_eq "exactly the 3 dead shims were removed" "5" \
  "$(ls -1 "$BIN2" | wc -l | tr -d ' ')"

# ---------- test 3: a clean bin dir makes the cleanup a silent no-op --------
TARGET3="$WORKDIR/target3"
BIN3="$WORKDIR/bin3"
mkdir -p "$BIN3"
make_target_repo "$TARGET3"
printf '#!/bin/sh\n' > "$BIN3/loom-my-helper"
chmod 755 "$BIN3/loom-my-helper"

out3="$(LOOM_DAEMON_BIN_DIR="$BIN3" bash "$UNINSTALL_SH" --yes --local "$TARGET3" 2>&1)"
assert_contains "no dead shims: count reported as 0" "$out3" "Dead machine-level loom-* PATH shims: 0"
TOTAL=$((TOTAL + 1))
if [[ "$out3" != *"Unlinking"* ]]; then
  echo -e "${GREEN}PASS${NC}: no dead shims: no unlink step runs"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: no dead shims: no unlink step runs"
  FAIL=$((FAIL + 1))
fi
assert_eq "no dead shims: unrelated loom-my-helper untouched" "1" \
  "$( [[ -f "$BIN3/loom-my-helper" ]] && echo 1 || echo 0 )"

# ---------- test 4: a missing bin dir never fails the uninstall -------------
TARGET4="$WORKDIR/target4"
make_target_repo "$TARGET4"
out4="$(LOOM_DAEMON_BIN_DIR="$WORKDIR/bin4-does-not-exist" bash "$UNINSTALL_SH" --yes --local "$TARGET4" 2>&1)"
rc4=$?
assert_eq "missing bin dir: uninstall still exits 0" "0" "$rc4"
assert_contains "missing bin dir: count reported as 0" "$out4" "Dead machine-level loom-* PATH shims: 0"

# ---------- summary ----------
echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
