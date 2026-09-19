#!/usr/bin/env bash
# test-role-tool-policy-spawn.sh — session-spawn-time half of the per-role
# tool restriction (issue #8256).
#
# The guard-hook half — the backstop that actually holds once a role is
# persuaded — is covered by tests/hooks/test-guard-destructive-role-tool-policy.sh.
# THIS suite covers the other half: what `spawn-claude.sh` and `spawn-codex.sh`
# do with the same `toolPolicy.allowedCapabilities` declaration at launch.
#
# A SEPARATE FILE, not new sections in test-spawn-claude.sh / test-spawn-codex.sh:
# both of those are frozen by the file-size ratchet (scripts/file-size-baseline.txt),
# which admits shrinkage only — the same reason test-token-model-class.sh was split
# out for #8058.
#
# WHAT IS ASSERTED, AND WHY IT IS THE ARGV AND NOT THE OUTCOME. `--disallowedTools`
# is a permission rule keyed on the Bash tool's leading command text: it is a
# useful first line and a trivially-evadable sole line (`bash -c 'ssh …'` walks
# straight past it). So the claim this suite can honestly make is "the restriction
# derived from the role's declaration reaches the CLI, and reaches it only for the
# roles that declared one" — the claim that the restriction HOLDS belongs to the
# guard-hook suite, and is made there.
#
# Usage: ./.loom/scripts/tests/test-role-tool-policy-spawn.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Pin the #5979 concurrent-sweep divisor, as test-spawn-claude.sh does: without
# it spawn-claude.sh's CPU-budget block shells out to `loom-daemon status --json`
# against whatever daemon happens to be running on the host, making this suite
# depend on live fleet state. `1` is the value a host with no daemon produces.
export LOOM_SWEEP_INFLIGHT_SWEEPS=1

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() {
    TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
    [[ -n "${2:-}" ]] && echo "    $2"
    [[ -n "${3:-}" ]] && echo "    Output: $3"
}

assert_contains() {
    local needle="$1" haystack="$2" msg="$3"
    if [[ "$haystack" == *"$needle"* ]]; then pass "$msg"
    else fail "$msg" "Expected substring: '$needle'" "$haystack"; fi
}

assert_not_contains() {
    local needle="$1" haystack="$2" msg="$3"
    if [[ "$haystack" != *"$needle"* ]]; then pass "$msg"
    else fail "$msg" "Did NOT expect substring: '$needle'" "$haystack"; fi
}

# --- Fixtures --------------------------------------------------------------
#
# A throwaway workspace carrying fixture role declarations, plus a stub `claude`
# on PATH that simply echoes the argv it was handed. The stub is what makes the
# spawn-time half observable at all: the real CLI is never invoked, so this suite
# is hermetic and needs no token pool (LOOM_SPAWN_NO_EXPORT + a dummy
# CLAUDE_CODE_OAUTH_TOKEN short-circuit selection entirely).

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

STUB_DIR="$WORKDIR/stub"
mkdir -p "$STUB_DIR"
cat > "$STUB_DIR/claude" <<'STUB'
#!/usr/bin/env bash
echo "stub-claude args=$*"
echo "stub-claude role=${LOOM_ROLE:-unset}"
STUB
chmod +x "$STUB_DIR/claude"

WS="$WORKDIR/ws"
mkdir -p "$WS/.loom/roles"
printf '%s' '{"name":"Fixture RO","toolPolicy":{"allowedCapabilities":[]}}' \
    > "$WS/.loom/roles/fixture-restricted.json"
printf '%s' '{"name":"Fixture Open","toolPolicy":{"allowedCapabilities":["*"]}}' \
    > "$WS/.loom/roles/fixture-open.json"
printf '%s' '{"name":"Fixture Cloud","toolPolicy":{"allowedCapabilities":["cloud-cli"]}}' \
    > "$WS/.loom/roles/fixture-cloud.json"
printf '%s' '{"name":"Fixture Undeclared","suggestedModel":"sonnet"}' \
    > "$WS/.loom/roles/fixture-undeclared.json"

# Run spawn-claude.sh with a role, returning the stub's echoed argv + our log.
run_spawn() {
    local role="$1"; shift
    if [[ -n "$role" ]]; then
        env LOOM_WORKSPACE="$WS" LOOM_SPAWN_NO_EXPORT=1 \
            CLAUDE_CODE_OAUTH_TOKEN=stub-token LOOM_ROLE="$role" \
            PATH="$STUB_DIR:$PATH" \
            bash "$SCRIPTS_DIR/spawn-claude.sh" "$@" 2>&1 || true
    else
        env -u LOOM_ROLE LOOM_WORKSPACE="$WS" LOOM_SPAWN_NO_EXPORT=1 \
            CLAUDE_CODE_OAUTH_TOKEN=stub-token \
            PATH="$STUB_DIR:$PATH" \
            bash "$SCRIPTS_DIR/spawn-claude.sh" "$@" 2>&1 || true
    fi
}

echo ""
echo -e "${YELLOW}=== Per-role tool restriction at spawn time (#8256) ===${NC}"
echo ""

# --- 1. A restricted role gets every capability's specs -------------------
echo -e "${YELLOW}[1] spawn-claude: a restricted role is launched with --disallowedTools${NC}"

OUT="$(run_spawn fixture-restricted -p ping)"
assert_contains "--disallowedTools" "$OUT" \
    "restricted role: --disallowedTools is injected"
assert_contains "Bash(ssh:*)" "$OUT" \
    "restricted role: remote-shell -> Bash(ssh:*)"
assert_contains "Bash(aws:*)" "$OUT" \
    "restricted role: cloud-cli -> Bash(aws:*)"
assert_contains "Bash(gh secret:*)" "$OUT" \
    "restricted role: forge-secrets -> Bash(gh secret:*)"
assert_contains "Write(//~/.ssh/**)" "$OUT" \
    "restricted role: credential-store -> Write(//~/.ssh/**)"
assert_contains "per-role tool restriction (#8256)" "$OUT" \
    "restricted role: the injection is logged, naming the issue"
assert_contains "$WS/.loom/roles/fixture-restricted.json" "$OUT" \
    "restricted role: the log names the declaration file it read"
# `gh auth status` must never be restricted — every role runs it, and a spec
# that blocked it would push roles toward evasion for a routine command.
assert_not_contains "Bash(gh auth status" "$OUT" \
    "restricted role: 'gh auth status' is NOT restricted"
# The role identity must reach the child, or the guard-hook backstop is blind.
assert_contains "stub-claude role=fixture-restricted" "$OUT" \
    "restricted role: LOOM_ROLE is exported to the session"

# --- 2. Unrestricted declarations are a byte-for-byte no-op ---------------
echo -e "${YELLOW}[2] spawn-claude: unrestricted roles are unchanged${NC}"

OUT="$(run_spawn fixture-open -p ping)"
assert_not_contains "--disallowedTools" "$OUT" \
    "\"*\" role: nothing is injected"
assert_contains "stub-claude args=-p ping" "$OUT" \
    "\"*\" role: argv is unchanged"

OUT="$(run_spawn fixture-undeclared -p ping)"
assert_not_contains "--disallowedTools" "$OUT" \
    "role with no toolPolicy: nothing is injected (consumer-repo compatibility)"

OUT="$(run_spawn fixture-does-not-exist -p ping)"
assert_not_contains "--disallowedTools" "$OUT" \
    "role with no role file: nothing is injected (fails open on identity)"

OUT="$(run_spawn "" -p ping)"
assert_not_contains "--disallowedTools" "$OUT" \
    "LOOM_ROLE unset: nothing is injected"
assert_contains "stub-claude args=-p ping" "$OUT" \
    "LOOM_ROLE unset: argv is unchanged"

# --- 3. Partial grants restrict only what was not granted -----------------
echo -e "${YELLOW}[3] spawn-claude: a partial grant restricts only the rest${NC}"

OUT="$(run_spawn fixture-cloud -p ping)"
assert_not_contains "Bash(aws:*)" "$OUT" \
    "cloud-cli grant: aws is NOT restricted"
assert_contains "Bash(ssh:*)" "$OUT" \
    "cloud-cli grant: ssh is still restricted"
assert_contains "Bash(gh secret:*)" "$OUT" \
    "cloud-cli grant: gh secret is still restricted"

# --- 4. An operator's own --disallowedTools is never clobbered ------------
echo -e "${YELLOW}[4] spawn-claude: an explicit --disallowedTools wins${NC}"

OUT="$(run_spawn fixture-restricted -p ping --disallowedTools 'Bash(rm:*)')"
assert_not_contains "Bash(ssh:*)" "$OUT" \
    "explicit --disallowedTools: no second injection"
assert_contains "Bash(rm:*)" "$OUT" \
    "explicit --disallowedTools: the operator's own value survives"
assert_contains "the guard-hook backstop still enforces" "$OUT" \
    "explicit --disallowedTools: the log says the backstop still applies"

# --- 5. Shipped roles ------------------------------------------------------
#
# Resolved from defaults/roles/ (the workspace fixture has no file for these),
# so this pins what actually ships rather than what a fixture asserts.
echo -e "${YELLOW}[5] spawn-claude: shipped roles${NC}"

for _role in architect auditor champion curator guide hermit judge; do
    OUT="$(run_spawn "$_role" -p ping)"
    assert_contains "Bash(ssh:*)" "$OUT" \
        "shipped role '$_role' is launched with the remote-shell restriction"
done

for _role in builder doctor driver loom; do
    OUT="$(run_spawn "$_role" -p ping)"
    assert_not_contains "--disallowedTools" "$OUT" \
        "shipped role '$_role' is launched unrestricted"
done

# The daemon's dispatch aliases must resolve to the role they name — a sweep
# child launched with a restricted policy because its LOOM_ROLE spelling was
# not recognised would break every sweep on the fleet.
for _alias in sweep-lifecycle development-worker pr-fixer; do
    OUT="$(run_spawn "$_alias" -p ping)"
    assert_not_contains "--disallowedTools" "$OUT" \
        "dispatch alias '$_alias' resolves to an unrestricted policy"
done

# --- 6. spawn-codex.sh -----------------------------------------------------
#
# The Codex CLI has no `--disallowedTools` equivalent, so there is no argv half
# to assert there. What spawn-codex.sh owes this issue instead is the WARNING:
# on that path the managed pre_tool_use hook is the ONLY enforcement, so a
# restricted role starting without it has no restriction at all, and that has to
# be said out loud rather than inferred from the absence of a message.
#
# Driven through LOOM_SPAWN_PRINT_ARGV-style short-circuiting is not available
# here, so the assertion is made against the source: the warning must exist, be
# conditional on a restrictive declaration, and name the remediation. A behavioral
# invocation would require a provisioned CODEX_HOME, which is not hermetic.
echo -e "${YELLOW}[6] spawn-codex: the unenforced-policy warning exists and is conditional${NC}"

CODEX_SRC="$(cat "$SCRIPTS_DIR/spawn-codex.sh")"
assert_contains "per-role tool restriction (#8256) is NOT ENFORCED" "$CODEX_SRC" \
    "spawn-codex: warns when a restrictive policy cannot be enforced"
assert_contains 'hooks=$_hook_status' "$CODEX_SRC" \
    "spawn-codex: the warning names the hook status that caused it"
assert_contains "provision-codex-hooks.sh install" "$CODEX_SRC" \
    "spawn-codex: the warning names the remediation"
assert_contains 'export LOOM_ROLE' "$CODEX_SRC" \
    "spawn-codex: LOOM_ROLE is exported so the bridge's sub-guards see it"
# Conditional on BOTH a declared array AND the absence of a wildcard — a role
# with no policy, or one granting "*", must never produce this warning.
assert_contains 'allowedCapabilities | type) == "array"' "$CODEX_SRC" \
    "spawn-codex: the warning requires a declared allowlist"
assert_contains 'index("*")) | not' "$CODEX_SRC" \
    "spawn-codex: the warning excludes a wildcard grant"

# --- 7. One source: the two halves read the same declaration --------------
#
# #8256's acceptance criterion is "the allowlist is per role, declared in the
# role's JSON, and the guard reads the same declaration (one source)". The
# structural evidence for that is that BOTH the spawn path and the guard read
# `toolPolicy.allowedCapabilities`, and that neither carries a second, private
# list of roles or capabilities that could drift from it.
echo -e "${YELLOW}[7] one source: both halves read toolPolicy.allowedCapabilities${NC}"

CLAUDE_SRC="$(cat "$SCRIPTS_DIR/spawn-claude.sh")"
GUARD_SRC="$(cat "$SCRIPTS_DIR/../hooks/guard-destructive-generic.sh")"
assert_contains "toolPolicy.allowedCapabilities" "$CLAUDE_SRC" \
    "spawn-claude reads toolPolicy.allowedCapabilities"
assert_contains "toolPolicy.allowedCapabilities" "$GUARD_SRC" \
    "guard-destructive-generic reads toolPolicy.allowedCapabilities"

# The three daemon dispatch aliases must be resolved identically in all three
# places, or one dispatch shape lands on a different policy than another.
for _place in "$SCRIPTS_DIR/spawn-claude.sh" "$SCRIPTS_DIR/spawn-codex.sh" \
              "$SCRIPTS_DIR/../hooks/guard-destructive-generic.sh"; do
    SRC="$(cat "$_place")"
    ok=true
    for _alias in "development-worker" "pr-fixer" "sweep-lifecycle"; do
        [[ "$SRC" == *"$_alias"* ]] || ok=false
    done
    if [[ "$ok" == "true" ]]; then
        pass "$(basename "$_place") resolves all three daemon dispatch aliases"
    else
        fail "$(basename "$_place") resolves all three daemon dispatch aliases" \
            "one of development-worker / pr-fixer / sweep-lifecycle is missing"
    fi
done

echo ""
echo "========================================="
echo "  Total:  $TESTS_RUN"
echo -e "  ${GREEN}Passed${NC}: $TESTS_PASSED"
echo -e "  ${RED}Failed${NC}: $TESTS_FAILED"
echo "========================================="
if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo -e "\n${RED}TESTS FAILED${NC}"
    exit 1
fi
echo -e "\n${GREEN}ALL TESTS PASSED${NC}"
exit 0
