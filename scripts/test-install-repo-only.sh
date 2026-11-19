#!/usr/bin/env bash
# Test suite for the installers' --repo-only / LOOM_INSTALL_REPO_ONLY=1 option
# (issue #14570): write only the target repository's files, skipping every
# machine-level provisioning step.
#
# A real install builds loom-daemon and writes to $HOME, so this suite does not
# run one. It checks, with a throwaway HOME and local temp git repos (no network,
# no build):
#   1. both installers document --repo-only in --help;
#   2. install-loom.sh: every machine-level provision_* call / user-scope MCP
#      registration sits in the `else`/`elif` arm of a REPO_ONLY guard, and the
#      flag + env var both set REPO_ONLY;
#   3. install.sh: wire_quick_install_guard_hooks (extracted and run with stubs)
#      skips provision_loom_hooks under --repo-only but still asserts the
#      project-level wiring (ensure_project_hook_wiring); and unset, does both;
#   4. install.sh --dry-run preview states nothing is written outside the repo,
#      and leaves the temp HOME empty.
#
# Usage: bash scripts/test-install-repo-only.sh

set -uo pipefail

passed=0
failed=0
pass() { echo "PASS: $1"; passed=$((passed + 1)); }
fail() { echo "FAIL: $1"; failed=$((failed + 1)); }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FULL="$LOOM_ROOT/scripts/install-loom.sh"
QUICK="$LOOM_ROOT/install.sh"

TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

# --- 1. --help documents the flag -------------------------------------------
full_help="$(bash "$FULL" --help 2>&1)"
quick_help="$(bash "$QUICK" --help 2>&1)"
if grep -q -- '--repo-only' <<<"$full_help"; then
  pass "install-loom.sh --help documents --repo-only"
else
  fail "install-loom.sh --help does not mention --repo-only"
fi
if grep -q -- '--repo-only' <<<"$quick_help"; then
  pass "install.sh --help documents --repo-only"
else
  fail "install.sh --help does not mention --repo-only"
fi

# --- 2. install-loom.sh: every machine-level step is gated ------------------
# Walk the file; a machine-level call is OK only while inside the else/elif arm
# of a top-level `if [[ "$REPO_ONLY" == "true" ]]` block.
gate_report="$(awk '
  /^if \[\[ "\$REPO_ONLY" == "true" \]\]; then/ { guard=1; arm="then"; next }
  guard && /^(else|elif)/ { arm="else"; next }
  guard && /^fi/ { guard=0; arm=""; next }
  /^[[:space:]]*(provision_machine_daemon|provision_loom_dispatcher|provision_loom_skills|provision_loom_hooks) / {
    n++
    if (!(guard && arm == "else")) { print "UNGATED line " NR ": " $0 }
  }
  /claude mcp add --scope user loom -- node "\$MCP_MACHINE_ENTRY"; then/ {
    m++
    if (!(guard && arm == "else")) { print "UNGATED mcp line " NR ": " $0 }
  }
  END { print "COUNT " n " " m }
' "$FULL")"
if grep -q '^UNGATED' <<<"$gate_report"; then
  fail "install-loom.sh has an ungated machine-level step: $(grep '^UNGATED' <<<"$gate_report" | head -3)"
elif grep -q '^COUNT 4 1$' <<<"$gate_report"; then
  pass "install-loom.sh gates all 4 provision_* steps and the user-scope MCP registration"
else
  fail "install-loom.sh gate scan found unexpected counts: $(grep '^COUNT' <<<"$gate_report")"
fi

skips="$(grep -c 'Skipping .*(--repo-only)' "$FULL")"
if [[ "$skips" -ge 5 ]]; then
  pass "install-loom.sh prints a skip line for each of the 5 machine-level steps ($skips found)"
else
  fail "install-loom.sh has only $skips '--repo-only' skip lines (expected >= 5)"
fi

if grep -q 'LOOM_INSTALL_REPO_ONLY' "$FULL" && grep -q -- '--repo-only) REPO_ONLY=true' "$FULL"; then
  pass "install-loom.sh sets REPO_ONLY from both the flag and LOOM_INSTALL_REPO_ONLY"
else
  fail "install-loom.sh does not wire both the flag and the env var to REPO_ONLY"
fi

# --- 3. install.sh: hook wiring split (behavioural, with stubs) -------------
run_wire() {
  local repo_only="$1" home="$2"
  local fn
  fn="$(sed -n '/^wire_quick_install_guard_hooks() {/,/^}/p' "$QUICK")"
  [[ -n "$fn" ]] || { echo "EXTRACT_FAILED"; return 1; }
  HOME="$home" REPO_ONLY="$repo_only" bash -c '
    info() { :; }
    warning() { echo "WARN $*"; }
    provision_loom_hooks() { mkdir -p "$HOME/.claude"; echo x > "$HOME/.claude/settings.json"; echo "CALLED provision_loom_hooks"; }
    ensure_project_hook_wiring() { echo "CALLED ensure_project_hook_wiring"; }
    '"$fn"'
    wire_quick_install_guard_hooks /some/target
  '
}

H1="$TEST_DIR/home-repo-only"; mkdir -p "$H1"
out="$(run_wire true "$H1")"
if grep -q 'CALLED provision_loom_hooks' <<<"$out" || [[ -e "$H1/.claude" ]]; then
  fail "install.sh --repo-only still provisioned user-scope hooks"
else
  pass "install.sh --repo-only skips provision_loom_hooks; HOME/.claude untouched"
fi
if grep -q 'CALLED ensure_project_hook_wiring' <<<"$out"; then
  pass "install.sh --repo-only still asserts project-level hook wiring"
else
  fail "install.sh --repo-only dropped ensure_project_hook_wiring"
fi

H2="$TEST_DIR/home-default"; mkdir -p "$H2"
out="$(run_wire false "$H2")"
if grep -q 'CALLED provision_loom_hooks' <<<"$out" && grep -q 'CALLED ensure_project_hook_wiring' <<<"$out"; then
  pass "install.sh default (no flag) still provisions user-scope hooks and project wiring"
else
  fail "install.sh default behaviour changed: $out"
fi

# The quick-install daemon provisioning call sites must be gated too.
ungated_daemon="$(awk '
  /if \[\[ "\$REPO_ONLY" == "true" \]\]; then/ { guard=1; arm="then"; next }
  guard && /^[[:space:]]*else/ { arm="else"; next }
  guard && /^[[:space:]]*fi/ { guard=0; arm=""; next }
  /^[[:space:]]*provision_machine_daemon "\$LOOM_CARGO_TARGET_DIR/ {
    n++; if (!(guard && arm == "else")) print "UNGATED line " NR
  }
  END { print "COUNT " n }
' "$QUICK")"
if grep -q '^UNGATED' <<<"$ungated_daemon"; then
  fail "install.sh has an ungated provision_machine_daemon call: $ungated_daemon"
elif grep -q '^COUNT 2$' <<<"$ungated_daemon"; then
  pass "install.sh gates both quick-install provision_machine_daemon calls"
else
  fail "install.sh provision_machine_daemon gate scan unexpected: $ungated_daemon"
fi

# --- 4. install.sh --dry-run preview under --repo-only ----------------------
REPO="$TEST_DIR/target"; mkdir -p "$REPO"
git -C "$REPO" init --quiet
git -C "$REPO" config user.email t@example.com
git -C "$REPO" config user.name T
git -C "$REPO" commit -q --allow-empty -m init
H3="$TEST_DIR/home-dryrun"; mkdir -p "$H3"
plan="$(HOME="$H3" bash "$QUICK" --dry-run --quick --repo-only "$REPO" 2>&1)"; rc=$?
if [[ $rc -eq 0 ]] && grep -q 'Outside the target repository: nothing' <<<"$plan" \
   && ! grep -q 'user-level, written on this machine' <<<"$plan"; then
  pass "install.sh --dry-run --repo-only preview reports no machine-level writes"
else
  fail "install.sh --dry-run --repo-only preview wrong (rc=$rc)"
fi
plan="$(HOME="$H3" LOOM_INSTALL_REPO_ONLY=1 bash "$QUICK" --dry-run --quick "$REPO" 2>&1)"
if grep -q 'Outside the target repository: nothing' <<<"$plan"; then
  pass "LOOM_INSTALL_REPO_ONLY=1 (env only) behaves like --repo-only"
else
  fail "LOOM_INSTALL_REPO_ONLY=1 did not enable repo-only behaviour"
fi
plan="$(HOME="$H3" bash "$QUICK" --dry-run --quick "$REPO" 2>&1)"
if grep -q 'user-level, written on this machine' <<<"$plan"; then
  pass "install.sh default preview still lists the machine-level writes"
else
  fail "install.sh default preview lost the machine-level section"
fi
if [[ -z "$(ls -A "$H3")" ]]; then
  pass "temp HOME is still empty after the dry-runs"
else
  fail "dry-run wrote into HOME: $(ls -A "$H3")"
fi

echo ""
echo "passed=$passed failed=$failed"
[[ "$failed" -eq 0 ]]
