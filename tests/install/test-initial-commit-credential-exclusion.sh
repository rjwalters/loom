#!/usr/bin/env bash
# Test suite for the #8734 audit of install-loom.sh / install.sh's
# "Create initial commit" `git add -A` (#7818/#8005 credential-bearing class).
#
# Usage: ./tests/install/test-initial-commit-credential-exclusion.sh
#
# scripts/install-loom.sh and install.sh both offer to `git init` a target
# directory that is not yet a git repository, write a starter .gitignore, and
# then run `git add -A` to create the first commit. At that point the
# .gitignore just written does NOT yet carry loom-daemon's managed
# CREDENTIAL_PATTERNS block (that lands later, when .loom/ itself is
# installed) -- so if the target directory already held a live
# credential-bearing path on disk (e.g. an operator ran `loom-daemon tokens
# bootstrap` in this same directory before ever running `git init`), a bare
# `git add -A` would sweep it into the very first commit.
#
# Both scripts now run `git add -A -- . ':!...'` with the credential class
# excluded, matching land-resync-commit.sh / resync-installed.sh (machine-
# checked against loom-daemon/src/init/post_init.rs CREDENTIAL_PATTERNS by
# credential_class_tests.rs). This suite extracts the EXACT command each
# script runs (rather than reimplementing it) and exercises it against a
# fixture directory holding one file per CREDENTIAL_PATTERNS entry plus an
# ordinary file, asserting the credential paths are excluded from the index
# while the ordinary file is staged.
#
# Self-contained, no network. Exit code 0 = all tests pass, 1 = failures.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0
TOTAL=0
RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'

assert_true() {
  local desc="$1" cond="$2"
  TOTAL=$((TOTAL + 1))
  if [[ "$cond" == "true" ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"; PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"; FAIL=$((FAIL + 1))
  fi
}

# Extract the "git add -A -- . ':!...'" line for the initial-commit block from
# a given installer script. Fails loudly (not a silent no-op) if the pattern
# is missing, so a future edit that drops the exclusion trips this test
# instead of silently regressing.
extract_git_add_line() {
  local file="$1"
  grep -m1 "git add -A -- \." "$file"
}

INSTALL_LOOM_SH="$REPO_ROOT/scripts/install-loom.sh"
INSTALL_SH="$REPO_ROOT/install.sh"

INSTALL_LOOM_GIT_ADD_LINE="$(extract_git_add_line "$INSTALL_LOOM_SH")"
INSTALL_SH_GIT_ADD_LINE="$(extract_git_add_line "$INSTALL_SH")"

# ----------------------------------------------------------------------------
# Fixture: a fresh (not-yet-committed) git repo holding one path per
# CREDENTIAL_PATTERNS entry plus an ordinary file.
# ----------------------------------------------------------------------------
make_fixture_repo() {
  local dir="$1"
  mkdir -p "$dir"
  git -C "$dir" init -q
  mkdir -p "$dir/.loom/claude-config" "$dir/.loom/tokens" "$dir/.loom/api-keys" \
    "$dir/.loom/gh-config" "$dir/.loom/gh-config-by-owner/some-owner"
  echo "fake-oauth-creds" > "$dir/.loom/claude-config/creds.json"
  echo "fake-token" > "$dir/.loom/tokens/acct-1.json"
  echo "fake-key" > "$dir/.loom/api-keys/key.txt"
  echo "fake-gh-config" > "$dir/.loom/gh-config/hosts.yml"
  echo "fake-gh-config-by-owner" > "$dir/.loom/gh-config-by-owner/some-owner/hosts.yml"
  echo "FAKE_TOKEN=abc123" > "$dir/.loom/accounts.env"
  # Prefix-lookalikes: must NOT be excluded (same contract as
  # is_credential_path()'s matching rules).
  mkdir -p "$dir/.loom/tokens-archive"
  echo "not a live credential" > "$dir/.loom/tokens-archive/x.json"
  echo "not a live credential" > "$dir/.loom/accounts.env.example"
  # An ordinary project file that must still get staged.
  echo "# hello" > "$dir/README.md"
}

WORK_DIR="$(mktemp -d)"
cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT

export GIT_CONFIG_GLOBAL="$WORK_DIR/gitconfig"
export GIT_CONFIG_SYSTEM=/dev/null
export HOME="$WORK_DIR"
git config --global user.email "test@example.com"
git config --global user.name "Test"
git config --global init.defaultBranch main

run_case() {
  local label="$1" git_add_line="$2"
  local fixture="$WORK_DIR/$label"
  make_fixture_repo "$fixture"

  # Run the EXACT `git add -A -- . ':!...'` command extracted from the real
  # script, inside the fixture directory.
  ( cd "$fixture" && eval "$git_add_line" )

  local staged
  staged="$(git -C "$fixture" diff --cached --name-only)"

  for credential_path in \
    ".loom/claude-config/creds.json" \
    ".loom/tokens/acct-1.json" \
    ".loom/api-keys/key.txt" \
    ".loom/gh-config/hosts.yml" \
    ".loom/gh-config-by-owner/some-owner/hosts.yml" \
    ".loom/accounts.env"
  do
    if grep -qxF "$credential_path" <<<"$staged"; then
      assert_true "$label: $credential_path is NOT staged" "false"
    else
      assert_true "$label: $credential_path is NOT staged" "true"
    fi
  done

  # Prefix-lookalikes must still be staged (excluded ONLY the exact class).
  if grep -qxF ".loom/tokens-archive/x.json" <<<"$staged"; then
    assert_true "$label: prefix-lookalike .loom/tokens-archive/x.json IS staged" "true"
  else
    assert_true "$label: prefix-lookalike .loom/tokens-archive/x.json IS staged" "false"
  fi
  if grep -qxF ".loom/accounts.env.example" <<<"$staged"; then
    assert_true "$label: prefix-lookalike .loom/accounts.env.example IS staged" "true"
  else
    assert_true "$label: prefix-lookalike .loom/accounts.env.example IS staged" "false"
  fi

  # The ordinary file must still be staged -- the exclusion must not swallow
  # unrelated content.
  if grep -qxF "README.md" <<<"$staged"; then
    assert_true "$label: README.md IS staged" "true"
  else
    assert_true "$label: README.md IS staged" "false"
  fi
}

run_case "install-loom.sh" "$INSTALL_LOOM_GIT_ADD_LINE"
run_case "install.sh" "$INSTALL_SH_GIT_ADD_LINE"

echo ""
echo "----------------------------------------"
echo "Results: $PASS/$TOTAL passed, $FAIL failed"
echo "----------------------------------------"

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
