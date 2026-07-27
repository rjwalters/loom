#!/usr/bin/env bash
# Test suite for the guard-destructive.sh DISPATCHER (issue #4041).
#
# Usage: ./tests/hooks/test-guard-destructive-dispatcher.sh
#
# guard-destructive.sh is a thin dispatcher that chooses which generic guard to
# run at runtime:
#   1. the canonical Repo Skills guard (.claude/skills/repo/hooks/
#      guard-destructive.sh) IF it exists AND carries the `repo#29` marker;
#   2. otherwise the vendored generic guard (guard-destructive-generic.sh)
#      shipped alongside the dispatcher.
#
# These tests build an isolated fake repo tree, drop the real dispatcher +
# vendored generic into <repo>/.loom/hooks/, and stub the canonical guard so we
# can assert which one the dispatcher execs across the marker-present / marker-
# absent / canonical-absent cases. Exit 0 = all pass, 1 = failures.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DISPATCHER_SRC="$REPO_ROOT/defaults/hooks/guard-destructive.sh"
GENERIC_SRC="$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
PASS=0
FAIL=0

check() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc (expected '$expected', got '$actual')"
    FAIL=$((FAIL + 1))
  fi
}

# A dangerous payload assembled so no literal appears in this file (which would
# trip a running guard when the file is edited/scanned elsewhere).
DANGER="rm -r""f /"

# Build an isolated fake repo; returns its path on stdout.
make_repo() {
  local repo
  repo="$(mktemp -d)"
  mkdir -p "$repo/.loom/hooks" "$repo/.claude/skills/repo/hooks"
  cp "$DISPATCHER_SRC" "$repo/.loom/hooks/guard-destructive.sh"
  cp "$GENERIC_SRC" "$repo/.loom/hooks/guard-destructive-generic.sh"
  chmod +x "$repo/.loom/hooks/"*.sh
  printf '%s' "$repo"
}

# Run the dispatcher in $1 with command $2; print its stdout.
run_dispatcher() {
  local repo="$1" cmd="$2"
  printf '{"tool_input":{"command":"%s"},"cwd":"%s"}\n' "$cmd" "$repo" \
    | bash "$repo/.loom/hooks/guard-destructive.sh" 2>/dev/null
}

# --- Case 1: canonical absent → vendored generic denies the dangerous payload ---
echo "Case 1: canonical absent → vendored generic runs"
REPO="$(make_repo)"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "dangerous payload denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
OUT="$(run_dispatcher "$REPO" "ls -la")"
check "benign command allowed (empty output)" "" "$OUT"
rm -rf "$REPO"

# --- Case 2: canonical present but NO marker → still uses vendored generic ---
echo "Case 2: canonical present without repo#29 marker → vendored generic runs"
REPO="$(make_repo)"
printf '#!/usr/bin/env bash\necho CANON-RAN\nexit 0\n' \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical without marker is NOT run" "" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
check "dangerous payload still denied via vendored generic" \
  "deny" \
  "$(printf '%s' "$OUT" | jq -r '.hookSpecificOutput.permissionDecision' 2>/dev/null)"
rm -rf "$REPO"

# --- Case 3: canonical present WITH repo#29 marker → dispatcher execs canonical ---
echo "Case 3: canonical present with repo#29 marker → canonical runs"
REPO="$(make_repo)"
# assemble the marker so this file itself has no literal 'repo#29' token
MARK="repo#""29"
printf '#!/usr/bin/env bash\n# fixed per %s\necho CANON-RAN\nexit 0\n' "$MARK" \
  > "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
chmod +x "$REPO/.claude/skills/repo/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
check "canonical guard is exec'd (marker present)" "CANON-RAN" \
  "$(printf '%s' "$OUT" | grep -o 'CANON-RAN' | head -1)"
rm -rf "$REPO"

# --- Case 4: neither guard available → fail-open allow (exit 0, no output) ---
echo "Case 4: no guard available → fail-open allow"
REPO="$(mktemp -d)"
mkdir -p "$REPO/.loom/hooks"
cp "$DISPATCHER_SRC" "$REPO/.loom/hooks/guard-destructive.sh"
chmod +x "$REPO/.loom/hooks/guard-destructive.sh"
OUT="$(run_dispatcher "$REPO" "$DANGER")"
RC=$?
check "no guard → empty output" "" "$OUT"
check "no guard → exit 0" "0" "$RC"
rm -rf "$REPO"

echo ""
echo "========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed"
echo "========================================="
[[ "$FAIL" -eq 0 ]] || exit 1
