#!/usr/bin/env bash
# Test suite for uninstall-loom.sh Step 5b "Clean Up Orphaned Machine-Level
# Shims" (#5738).
#
# Usage: ./tests/install/test-uninstall-shim-cleanup.sh
#
# uninstall-loom.sh's Step 5b is a thin wiring layer: it sources
# scripts/install/provision-daemon.sh and calls its
# `_pmd_cleanup_retired_shims` helper against `${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}`.
# The safety-gated removal logic itself (loom-tools ownership + dangling
# check + the explicit eleven-name allowlist) is exercised exhaustively in
# tests/install/test-provision-daemon.sh — this suite pins ONLY the wiring:
#
#   1. Source anchors: uninstall-loom.sh actually sources provision-daemon.sh
#      and actually calls `_pmd_cleanup_retired_shims` in Step 5b, so the
#      cleanup is not silently dead code.
#   2. Functional: the exact dest-dir expression Step 5b uses
#      (`${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}`) resolves to, and cleans up,
#      the right directory — both the env-override path and the $HOME
#      fallback path.
#   3. Safety: replaying Step 5b's call against a dir holding BOTH an orphaned
#      retired-name shim and a live, repairable shim (`loom-clean`) removes
#      only the orphan — Step 5b must never touch the three repairable shims
#      (they are live, shared, working resources; see the comment above
#      Step 5b in uninstall-loom.sh).
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"

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

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

echo ""
echo "=== Source anchors: uninstall-loom.sh wires Step 5b correctly ==="

assert_eq "sources provision-daemon.sh" "yes" \
  "$(grep -q 'source "\$LOOM_ROOT/scripts/install/provision-daemon.sh"' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "Step 5b heading is present" "yes" \
  "$(grep -q 'STEP 5b: Clean Up Orphaned Machine-Level Shims' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "Step 5b calls _pmd_cleanup_retired_shims" "yes" \
  "$(grep -q '_pmd_cleanup_retired_shims "\${LOOM_DAEMON_BIN_DIR:-\$HOME/.local/bin}"' "$UNINSTALL_SH" && echo yes || echo no)"
assert_eq "Step 5b runs after Step 5 (file removal) and before Step 6 (smart remove)" "yes" \
  "$(awk '/STEP 5: Remove Loom Files/{s=NR} /STEP 5b: Clean Up Orphaned/{b=NR} /STEP 6: Smart Remove CLAUDE.md/{six=NR} END{print (s>0 && b>s && six>b) ? "yes" : "no"}' "$UNINSTALL_SH")"

echo ""
echo "=== Functional: Step 5b's exact dest-dir expression ==="

# Case 1: LOOM_DAEMON_BIN_DIR override — replay the literal expression.
DEST_OVERRIDE="$WORKDIR/custom-bin"
mkdir -p "$DEST_OVERRIDE"
ln -s "$WORKDIR/nonexistent-venv-root/loom-tools/.venv/bin/loom-status" "$DEST_OVERRIDE/loom-status"
(
  export LOOM_DAEMON_BIN_DIR="$DEST_OVERRIDE"
  _pmd_cleanup_retired_shims "${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}" >/dev/null 2>&1
)
assert_eq "LOOM_DAEMON_BIN_DIR override: dangling orphan in the overridden dir is removed" "0" \
  "$( [[ -e "$DEST_OVERRIDE/loom-status" || -L "$DEST_OVERRIDE/loom-status" ]] && echo 1 || echo 0 )"

# Case 2: $HOME fallback — replay with HOME pointed at a scratch dir and no
# LOOM_DAEMON_BIN_DIR set, matching Step 5b's real-world default.
FAKE_HOME="$WORKDIR/fake-home"
mkdir -p "$FAKE_HOME/.local/bin"
ln -s "$WORKDIR/nonexistent-venv-root/loom-tools/.venv/bin/loom-worktree" \
  "$FAKE_HOME/.local/bin/loom-worktree"
(
  unset LOOM_DAEMON_BIN_DIR
  export HOME="$FAKE_HOME"
  _pmd_cleanup_retired_shims "${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}" >/dev/null 2>&1
)
assert_eq "\$HOME fallback: dangling orphan in \$HOME/.local/bin is removed" "0" \
  "$( [[ -e "$FAKE_HOME/.local/bin/loom-worktree" || -L "$FAKE_HOME/.local/bin/loom-worktree" ]] && echo 1 || echo 0 )"

echo ""
echo "=== Safety: Step 5b never touches the three repairable shims ==="

DEST_MIXED="$WORKDIR/mixed-bin"
mkdir -p "$DEST_MIXED"
# A live, working repairable shim (as _pmd_install_shim would leave it).
cat > "$DEST_MIXED/loom-clean" <<'EOF'
#!/usr/bin/env bash
exec "$(dirname "$0")/loom-daemon" clean "$@"
EOF
chmod 755 "$DEST_MIXED/loom-clean"
# An orphaned, unrepairable shim alongside it.
ln -s "$WORKDIR/nonexistent-venv-root/loom-tools/.venv/bin/loom-health-monitor" \
  "$DEST_MIXED/loom-health-monitor"
_pmd_cleanup_retired_shims "$DEST_MIXED" >/dev/null 2>&1
assert_eq "mixed dir: orphaned loom-health-monitor is removed" "0" \
  "$( [[ -e "$DEST_MIXED/loom-health-monitor" || -L "$DEST_MIXED/loom-health-monitor" ]] && echo 1 || echo 0 )"
assert_eq "mixed dir: live loom-clean shim is left untouched" "1" \
  "$( [[ -f "$DEST_MIXED/loom-clean" && ! -L "$DEST_MIXED/loom-clean" && -x "$DEST_MIXED/loom-clean" ]] && echo 1 || echo 0 )"

echo ""
echo "==================================================="
echo "Results: $PASS/$TOTAL passed, $FAIL failed"
echo "==================================================="
[[ "$FAIL" -eq 0 ]]
