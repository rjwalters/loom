#!/usr/bin/env bash
# test-dashboard-link.sh — the byte-format pin between the bash footer twin and
# the Rust core (#9774).
#
# defaults/scripts/lib/dashboard-link.sh is the binary-absent path for the
# dashboard footer every posted comment carries (#9772);
# loom-daemon/src/forge_comment.rs's build_dashboard_footer is the canonical
# format. This suite asserts the two produce IDENTICAL bytes — so a format
# change on either side alone fails here — and covers the twin's own contract
# (idempotence on the marker, the /pull vs /issues kind, and the
# LOOM_DASHBOARD_URL override with trailing-slash + whitespace trimming).
#
# Usage:
#   bash defaults/scripts/tests/test-dashboard-link.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_DIR="$SCRIPT_DIR"
SCRIPTS_DIR="$SCRIPT_DIR/.."
LIB="$SCRIPT_DIR/../lib/dashboard-link.sh"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
TESTS_RUN=0; TESTS_PASSED=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN+1)); TESTS_PASSED=$((TESTS_PASSED+1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN+1)); TESTS_FAILED=$((TESTS_FAILED+1)); echo -e "  ${RED}FAIL${NC}: $1"; }
assert_eq() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (got '$1' want '$2')"; fi; }

# shellcheck source=../lib/dashboard-link.sh
source "$LIB"

# --- The twin's own contract (no daemon needed) -------------------------------

# (`$( )` strips trailing newlines, so the expected literals below end at the
# marker — the function's own trailing newline is asserted by the byte-cmp
# against the Rust core further down.)
expected_issue=$'body text\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/issues/42)\n<!-- loom:dashboard-link -->'
assert_eq "$(forge_append_dashboard_footer "o/r" 42 0 "body text")" "$expected_issue" \
  "footer is the pinned byte format (issues)"

expected_pull=$'b\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/pull/9)\n<!-- loom:dashboard-link -->'
assert_eq "$(forge_append_dashboard_footer "o/r" 9 1 "b")" "$expected_pull" \
  "IS_PR=1 says /pull/N"

once="$(forge_append_dashboard_footer "o/r" 42 0 "body")"
assert_eq "$(forge_append_dashboard_footer "o/r" 42 0 "$once")" "$once" \
  "idempotent on the hidden marker"

(
  export LOOM_DASHBOARD_URL="https://d.example.com///"
  assert_eq "$(forge_dashboard_url "o/r" 1 0)" "https://d.example.com/github.com/o/r/issues/1" \
    "override: trailing slashes trimmed"
)

(
  export LOOM_DASHBOARD_URL="   "
  assert_eq "$(forge_dashboard_base_url)" "https://dashboard.2amlogic.com" \
    "override: blank falls back to the default"
)

# --- The pin: bash twin == Rust core, byte for byte --------------------------
# (issue #9772's acceptance: the two implementations cannot drift.) Pinned
# through the standard harness seam so the comparison always execs the binary
# built from this tree, never whatever the host has installed (#8176).
# shellcheck source=lib/require-daemon-bin.sh
source "$TEST_DIR/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin "$SCRIPTS_DIR" "forge"

probe=$'probe body\nwith two lines'
for spec in "o/r 42 0" "o/r 9 1"; do
  read -r nwo number is_pr <<<"$spec"
  kind="issues"; [[ "$is_pr" == "1" ]] && kind="pull"
  pr_flag=""
  [[ "$is_pr" == "1" ]] && pr_flag="--pr"
  if cmp -s \
    <(printf '%s' "$(forge_append_dashboard_footer "$nwo" "$number" "$is_pr" "$probe")") \
    <(printf '%s' "$("$LOOM_DAEMON_SELF_BIN" forge dashboard-link "$nwo" "$number" $pr_flag --body "$probe")"); then
    pass "bash twin == Rust core ($kind)"
  else
    fail "bash twin == Rust core ($kind) — the format drifted; change both or neither"
  fi
done

(
  export LOOM_DASHBOARD_URL="https://d.example.com/"
  if cmp -s \
    <(printf '%s' "$(forge_append_dashboard_footer "o/r" 1 0 "x")") \
    <(printf '%s' "$("$LOOM_DAEMON_SELF_BIN" forge dashboard-link o/r 1 --body x)"); then
    pass "bash twin == Rust core under LOOM_DASHBOARD_URL"
  else
    fail "bash twin == Rust core under LOOM_DASHBOARD_URL"
  fi
)

echo
echo "Passed: $TESTS_PASSED / $TESTS_RUN  (failed: $TESTS_FAILED)"
[[ "$TESTS_FAILED" -eq 0 ]]
