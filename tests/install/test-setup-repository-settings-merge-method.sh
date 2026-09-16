#!/usr/bin/env bash
# Test suite for scripts/install/setup-repository-settings.sh's merge-method
# handling (#7754).
#
# Root cause: the installer used to unconditionally force
# allow_merge_commit=false / allow_squash_merge=true / allow_rebase_merge=false
# on every target repo. That broke any repo an operator had deliberately
# configured for merge-commit-only or rebase-only -- their configuration was
# silently reverted to squash-only on the next install/re-run, and
# merge-pr.sh's downstream hardcoded merge_method=squash then failed
# outright with GitHub/Gitea's "Squash merges are not allowed on this
# repository" (the original #1258 bug).
#
# This suite exercises the installer script itself as a subprocess (it is
# not designed to be sourced -- it runs its main dispatch unconditionally),
# with `gh`/`curl` stubs on PATH so no real network access or forge
# credentials are required.
#
# Usage: ./tests/install/test-setup-repository-settings-merge-method.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SETUP_SCRIPT="$REPO_ROOT/scripts/install/setup-repository-settings.sh"

PASS=0
FAIL=0
TOTAL=0

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

assert_contains() {
  local desc="$1" haystack="$2" needle="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" == *"$needle"* ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc"
    echo "    expected output to contain: '$needle'"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local desc="$1" haystack="$2" needle="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" != *"$needle"* ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc"
    echo "    expected output to NOT contain: '$needle'"
    FAIL=$((FAIL + 1))
  fi
}

# Build a throwaway git repo with a GitHub-style origin so
# detect_forge_and_repo resolves FORGE_TYPE=github without any network call.
make_fake_github_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "$dir" init -q
  git -C "$dir" remote add origin "https://github.com/owner/repo.git"
  printf '%s\n' "$dir"
}

# ============================================================================
# GitHub: repo already has a NON-squash merge strategy enabled (merge-commit
# only) -- the installer must respect it, not force squash-only.
# ============================================================================
echo ""
echo "=== GitHub: merge-commit-only repo is respected (dry-run) ==="

GH_REPO_DIR="$(make_fake_github_repo)"
GH_STUB_DIR="$(mktemp -d)"
cat > "$GH_STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$*" == *"--jq"*".permissions.admin"* ]]; then
  echo "true"
  exit 0
fi
if [[ "$1" == "api" && "$*" != *"-X PATCH"* ]]; then
  printf '{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":false}\n'
  exit 0
fi
echo '{}'
exit 0
STUB
chmod +x "$GH_STUB_DIR/gh"

gh_dry_output="$(PATH="$GH_STUB_DIR:$PATH" bash "$SETUP_SCRIPT" "$GH_REPO_DIR" --dry-run 2>&1)"

assert_contains "GitHub dry-run reports respecting the existing merge-commit-only config" \
  "$gh_dry_output" "respecting existing configuration"
assert_not_contains "GitHub dry-run does NOT propose forcing allow_squash_merge on a merge-commit-only repo" \
  "$gh_dry_output" "fallback default"

rm -rf "$GH_REPO_DIR" "$GH_STUB_DIR"

# ============================================================================
# GitHub: repo has EVERY merge strategy disabled (degenerate) -- the
# installer falls back to enabling squash so the repo can merge at all.
# ============================================================================
echo ""
echo "=== GitHub: no merge strategy enabled falls back to squash (dry-run) ==="

GH_REPO_DIR2="$(make_fake_github_repo)"
GH_STUB_DIR2="$(mktemp -d)"
cat > "$GH_STUB_DIR2/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$*" == *"--jq"*".permissions.admin"* ]]; then
  echo "true"
  exit 0
fi
if [[ "$1" == "api" && "$*" != *"-X PATCH"* ]]; then
  printf '{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":false}\n'
  exit 0
fi
echo '{}'
exit 0
STUB
chmod +x "$GH_STUB_DIR2/gh"

gh_dry_output2="$(PATH="$GH_STUB_DIR2:$PATH" bash "$SETUP_SCRIPT" "$GH_REPO_DIR2" --dry-run 2>&1)"

assert_contains "GitHub dry-run proposes the squash fallback when nothing is currently enabled" \
  "$gh_dry_output2" "fallback default"

rm -rf "$GH_REPO_DIR2" "$GH_STUB_DIR2"

# ============================================================================
# GitHub: non-dry-run PATCH payload omits the merge-strategy trio entirely
# when the repo already allows a strategy -- this is the actual behavioral
# fix, not just the dry-run preview text.
# ============================================================================
echo ""
echo "=== GitHub: live PATCH payload excludes the merge-strategy trio when respecting existing config ==="

GH_REPO_DIR3="$(make_fake_github_repo)"
GH_STUB_DIR3="$(mktemp -d)"
GH_PATCH_BODY_FILE="$(mktemp)"
export GH_PATCH_BODY_FILE
cat > "$GH_STUB_DIR3/gh" <<'STUB'
#!/usr/bin/env bash
if [[ "$*" == *"--jq"*".permissions.admin"* ]]; then
  echo "true"
  exit 0
fi
if [[ "$1" == "api" && "$*" == *"-X PATCH"* ]]; then
  cat - > "$GH_PATCH_BODY_FILE"
  echo '{}'
  exit 0
fi
if [[ "$1" == "api" ]]; then
  printf '{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":true}\n'
  exit 0
fi
echo '{}'
exit 0
STUB
chmod +x "$GH_STUB_DIR3/gh"

: > "$GH_PATCH_BODY_FILE"
PATH="$GH_STUB_DIR3:$PATH" GH_PATCH_BODY_FILE="$GH_PATCH_BODY_FILE" \
  bash "$SETUP_SCRIPT" "$GH_REPO_DIR3" >/dev/null 2>&1 || true

patch_body="$(cat "$GH_PATCH_BODY_FILE")"
assert_not_contains "live PATCH body does not force allow_squash_merge on a rebase-only repo" \
  "$patch_body" '"allow_squash_merge"'
assert_not_contains "live PATCH body does not touch allow_rebase_merge on a rebase-only repo" \
  "$patch_body" '"allow_rebase_merge"'
assert_contains "live PATCH body still applies the non-merge-strategy settings (delete_branch_on_merge)" \
  "$patch_body" '"delete_branch_on_merge"'

rm -rf "$GH_REPO_DIR3" "$GH_STUB_DIR3"; rm -f "$GH_PATCH_BODY_FILE"

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
