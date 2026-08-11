#!/usr/bin/env bash
# Test suite for the Loom-managed nested Biome configs (#6031).
#
# Usage: ./tests/install/test-lint-ignore-configs.sh
#
# Background: a consumer repo running a repo-wide `biome check .` used to fail
# on files Loom installs and the consumer never wrote:
#
#   * `.loom/scripts/experiments/judge-fanout-workflow.js` is a Claude Code
#     Workflow script that legally uses top-level `return` (its VM allows it).
#     Standard ECMAScript does not, so Biome emits a hard `parse` error that no
#     formatter/linter option can suppress.
#   * `.loom/config.json`, `.loom/install-metadata.json`, and
#     `.claude/settings.json` are machine-emitted with 2-space indentation, so
#     any consumer whose Biome config differs (the default is tabs) sees a
#     format diff on every run.
#
# The fix ships two nested Biome configuration files as ordinary Loom payload:
# `defaults/.loom/biome.jsonc` (blanket — `.loom/` is entirely Loom's) and
# `defaults/.claude/biome.jsonc` (narrow — `.claude/` is shared with the
# consumer, so only Loom-owned paths are excluded). `.biomeignore` is NOT a
# viable alternative: Biome 2.x dropped it.
#
# This suite is hermetic — it never invokes biome (no network, no npx) and
# instead asserts the shipped configs' structure plus every install-path wiring
# point that must carry them into a consumer repo.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

LOOM_BIOME="$REPO_ROOT/defaults/.loom/biome.jsonc"
CLAUDE_BIOME="$REPO_ROOT/defaults/.claude/biome.jsonc"
RESYNC_SH="$REPO_ROOT/defaults/scripts/resync-installed.sh"
INIT_RS="$REPO_ROOT/loom-daemon/src/init/mod.rs"

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

assert_true() {
  local desc="$1" cond="$2"
  assert_eq "$desc" "true" "$cond"
}

if ! command -v jq >/dev/null 2>&1; then
  echo -e "${RED}FATAL${NC}: jq is required by this suite"
  exit 1
fi

# JSONC -> JSON: drop lines whose first non-whitespace characters are `//`.
# Safe here only because both shipped configs use whole-line comments and never
# a trailing comment or a `//` inside a string — which the first test asserts.
strip_jsonc_comments() {
  sed -e 's|^[[:space:]]*//.*$||' "$1"
}

# ============================================================================
# Test 1: both configs are shipped and are parseable JSONC
# ============================================================================
echo ""
echo "=== both nested Biome configs are shipped as defaults/ payload ==="

assert_true "defaults/.loom/biome.jsonc exists" \
  "$([[ -f "$LOOM_BIOME" ]] && echo true || echo false)"
assert_true "defaults/.claude/biome.jsonc exists" \
  "$([[ -f "$CLAUDE_BIOME" ]] && echo true || echo false)"

for f in "$LOOM_BIOME" "$CLAUDE_BIOME"; do
  rel="${f#"$REPO_ROOT"/}"

  # Every `//` must be at the start of a line (modulo indentation) so the
  # comment stripper above — and any consumer tooling that does the same — can
  # never truncate a string value mid-line.
  stray="$(grep -n '//' "$f" | grep -vc '^[0-9]*:[[:space:]]*//')"
  assert_eq "$rel uses only whole-line // comments" "0" "$stray"

  if strip_jsonc_comments "$f" | jq empty 2>/dev/null; then
    parses=true
  else
    parses=false
  fi
  assert_true "$rel parses as JSON once comments are stripped" "$parses"

  # `root: false` is what makes Biome treat the file as a NESTED config that
  # applies to its own directory rather than as the repository's root config.
  root_val="$(strip_jsonc_comments "$f" | jq -r '.root' 2>/dev/null)"
  assert_eq "$rel declares \"root\": false" "false" "$root_val"
done

# ============================================================================
# Test 2: .loom/biome.jsonc excludes the whole .loom/ subtree
# ============================================================================
echo ""
echo "=== .loom/biome.jsonc excludes every path under .loom/ ==="

LOOM_INCLUDES="$(strip_jsonc_comments "$LOOM_BIOME" | jq -c '.files.includes' 2>/dev/null)"
assert_eq "files.includes is exactly [\"!**\"]" '["!**"]' "$LOOM_INCLUDES"

# ============================================================================
# Test 3: .claude/biome.jsonc excludes ONLY Loom-owned paths
# ============================================================================
echo ""
echo "=== .claude/biome.jsonc excludes Loom-owned paths and nothing else ==="

CLAUDE_INCLUDES="$(strip_jsonc_comments "$CLAUDE_BIOME" | jq -r '.files.includes[]' 2>/dev/null)"

# `**` must be present, or the negation-only list would exclude the consumer's
# own files under .claude/ too (that is exactly what .loom/biome.jsonc does).
assert_true "files.includes opts every path in with \"**\"" \
  "$(printf '%s\n' "$CLAUDE_INCLUDES" | grep -qx -- '\*\*' && echo true || echo false)"

for excluded in '!biome.jsonc' '!settings.json' '!agents' '!commands/loom'; do
  assert_true "files.includes excludes ${excluded#!}" \
    "$(printf '%s\n' "$CLAUDE_INCLUDES" | grep -qxF -- "$excluded" && echo true || echo false)"
done

# Since Biome 2.2.0 a folder exclusion must NOT carry a trailing `/**` — the
# `lint/suspicious/useBiomeIgnoreFolder` rule flags that form, which would make
# the config Loom ships fail the very check it exists to make pass.
stale_glob="$(printf '%s\n' "$CLAUDE_INCLUDES" | grep -c '^!.*/\*\*$')"
assert_eq "no folder exclusion uses the deprecated trailing /** form" "0" "$stale_glob"

# ============================================================================
# Test 4: both configs are registered in the install manifest
# ============================================================================
echo ""
echo "=== install manifest registers both configs as Loom-owned ==="

MANIFEST="$(
  LOOM_ROOT="$REPO_ROOT" TARGET_PATH="$REPO_ROOT" \
  bash -c 'source "$LOOM_ROOT/scripts/install/manifest.sh"; _emit_installed_files_manifest'
)"

for path in ".loom/biome.jsonc" ".claude/biome.jsonc"; do
  assert_true "manifest lists $path" \
    "$(printf '%s' "$MANIFEST" | grep -qF "\"$path\"" && echo true || echo false)"
done

# ============================================================================
# Test 5: every install/update path that must carry the configs is wired
# ============================================================================
echo ""
echo "=== install, reinstall, and resync paths all carry the configs ==="

# Fresh install + reinstall: loom-daemon init copies defaults/.loom/biome.jsonc.
# (`.claude/biome.jsonc` needs no dedicated call — setup_repository_scaffolding
# copies the whole defaults/.claude/ tree.)
assert_true "loom-daemon init copies .loom/biome.jsonc" \
  "$(grep -qF '".loom/biome.jsonc"' "$INIT_RS" && echo true || echo false)"

# Already-installed repos: resync must BACKFILL both (unconditional sync, not
# gated on the destination pre-existing — no repo installed before #6031 has
# either file, so a destination-gated sync would never deliver them).
for path in ".loom/biome.jsonc" ".claude/biome.jsonc"; do
  assert_true "resync-installed.sh syncs $path" \
    "$(grep -qF "\"\$REPO_ROOT/$path\"" "$RESYNC_SH" && echo true || echo false)"
done

assert_true "resync-installed.sh documents the configs in its surface map" \
  "$(grep -qF '.loom/biome.jsonc       <- defaults/.loom/biome.jsonc' "$RESYNC_SH" && echo true || echo false)"

# ============================================================================
# Test 6: the parse-erroring experiment script is actually under .loom/
# ============================================================================
echo ""
echo "=== the file that triggered #6031 falls inside the excluded subtree ==="

# defaults/scripts/X installs to .loom/scripts/X, so the Workflow script is
# covered by .loom/biome.jsonc's blanket exclusion. If it ever moves out of
# defaults/scripts/, this exclusion stops covering it.
assert_true "judge-fanout-workflow.js still installs under .loom/scripts/" \
  "$([[ -f "$REPO_ROOT/defaults/scripts/experiments/judge-fanout-workflow.js" ]] && echo true || echo false)"

# Sanity: it really does contain the top-level `return` that no standard-JS
# parser will accept (i.e. the exclusion is load-bearing, not cargo-culted).
assert_true "judge-fanout-workflow.js contains a top-level return" \
  "$(grep -qE '^return[[:space:]]' "$REPO_ROOT/defaults/scripts/experiments/judge-fanout-workflow.js" && echo true || echo false)"

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
