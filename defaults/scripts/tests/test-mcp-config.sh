#!/usr/bin/env bash
# test-mcp-config.sh — Tests for the safehouse MCP injection layer (issue #3999).
#
# Covers:
#   1. Resolver precedence (env > config > default) for enabled/socket/persona.
#   2. Per-worker persona pool round-robin (distinct per concurrent worker).
#   3. loom_mcp_emit_config emits `loom` FIRST and appends `safehouse` only when
#      socket + persona + command are all present; no token/key is emitted.
#   4. Byte-identical regression: with safehouse disabled, scripts/setup-mcp.sh
#      produces the exact pre-#3999 loom-only .mcp.json.
#   5. claude-wrapper.sh's MCP pre-flight still resolves the `loom` entry point
#      with two servers present.
#
# Style matches test-spawn-claude.sh — plain bash, hand-rolled assertions.
# Bats is NOT used in this repository.
#
# Usage:
#   ./.loom/scripts/tests/test-mcp-config.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# Repo root: defaults/scripts/tests -> defaults/scripts -> defaults -> repo
REPO_ROOT="$(cd "$SCRIPTS_DIR/../.." && pwd)"
LIB="$SCRIPTS_DIR/lib/mcp-config.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

assert_contains() {
    local needle="$1" haystack="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" == *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

assert_not_contains() {
    local needle="$1" haystack="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" != *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Unexpected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

# Isolate every resolver call from the ambient operator environment.
unset LOOM_SAFEHOUSE_ENABLED LOOM_SAFEHOUSE_SOCKET LOOM_SAFEHOUSE_PERSONA \
    LOOM_SAFEHOUSE_WORKER_PERSONAS LOOM_SAFEHOUSE_MCP_COMMAND SAFEHOUSED_SOCKET \
    2>/dev/null || true

# shellcheck source=../lib/mcp-config.sh
source "$LIB"

HAVE_JQ=false
command -v jq >/dev/null 2>&1 && HAVE_JQ=true

# ============================================================
# Section 1: enabled resolution (env > config > default)
# ============================================================
echo "Testing loom_mcp_safehouse_enabled..."

_empty_repo="$(mktemp -d)"
assert_eq "false" "$(loom_mcp_safehouse_enabled "$_empty_repo")" \
    "default is false when no config and no env"

LOOM_SAFEHOUSE_ENABLED=1 \
    assert_eq "true" "$(LOOM_SAFEHOUSE_ENABLED=1 loom_mcp_safehouse_enabled "$_empty_repo")" \
    "env LOOM_SAFEHOUSE_ENABLED=1 forces true"

assert_eq "false" "$(LOOM_SAFEHOUSE_ENABLED=off loom_mcp_safehouse_enabled "$_empty_repo")" \
    "env LOOM_SAFEHOUSE_ENABLED=off forces false"

assert_eq "false" "$(LOOM_SAFEHOUSE_ENABLED='' loom_mcp_safehouse_enabled "$_empty_repo")" \
    "env LOOM_SAFEHOUSE_ENABLED='' forces false (matches daemon env_bool)"

if $HAVE_JQ; then
    _cfg_repo="$(mktemp -d)"
    mkdir -p "$_cfg_repo/.loom"
    cat >"$_cfg_repo/.loom/config.json" <<'JSON'
{ "safehouse": { "enabled": true } }
JSON
    assert_eq "true" "$(loom_mcp_safehouse_enabled "$_cfg_repo")" \
        "config safehouse.enabled=true resolves true"
    assert_eq "false" "$(LOOM_SAFEHOUSE_ENABLED=0 loom_mcp_safehouse_enabled "$_cfg_repo")" \
        "env overrides config (0 beats config true)"
    rm -rf "$_cfg_repo"
else
    echo -e "  ${YELLOW}SKIP${NC}: config-layer enabled tests (jq not installed)"
fi

# ============================================================
# Section 2: socket resolution (config > LOOM_SAFEHOUSE_SOCKET > SAFEHOUSED_SOCKET)
# ============================================================
echo "Testing loom_mcp_safehouse_socket..."

assert_eq "" "$(loom_mcp_safehouse_socket "$_empty_repo")" \
    "empty when nothing resolves"
assert_eq "/tmp/a.sock" \
    "$(LOOM_SAFEHOUSE_SOCKET=/tmp/a.sock loom_mcp_safehouse_socket "$_empty_repo")" \
    "LOOM_SAFEHOUSE_SOCKET is used"
assert_eq "/tmp/b.sock" \
    "$(SAFEHOUSED_SOCKET=/tmp/b.sock loom_mcp_safehouse_socket "$_empty_repo")" \
    "SAFEHOUSED_SOCKET fallback is used"
assert_eq "/tmp/a.sock" \
    "$(LOOM_SAFEHOUSE_SOCKET=/tmp/a.sock SAFEHOUSED_SOCKET=/tmp/b.sock loom_mcp_safehouse_socket "$_empty_repo")" \
    "LOOM_SAFEHOUSE_SOCKET wins over SAFEHOUSED_SOCKET"

if $HAVE_JQ; then
    _sock_repo="$(mktemp -d)"
    mkdir -p "$_sock_repo/.loom"
    cat >"$_sock_repo/.loom/config.json" <<'JSON'
{ "safehouse": { "socket": "/run/cfg.sock" } }
JSON
    assert_eq "/run/cfg.sock" \
        "$(LOOM_SAFEHOUSE_SOCKET=/tmp/env.sock loom_mcp_safehouse_socket "$_sock_repo")" \
        "config safehouse.socket wins over env (matches daemon resolve order)"
    rm -rf "$_sock_repo"
fi

# ============================================================
# Section 3: persona fallback + worker pool round-robin
# ============================================================
echo "Testing persona resolution..."

assert_eq "loom_daemon" "$(loom_mcp_safehouse_persona_fallback "$_empty_repo")" \
    "persona fallback default is loom_daemon"
assert_eq "loom_x" \
    "$(LOOM_SAFEHOUSE_PERSONA=loom_x loom_mcp_safehouse_persona_fallback "$_empty_repo")" \
    "LOOM_SAFEHOUSE_PERSONA overrides fallback"

# pick_persona with empty pool -> fallback
assert_eq "loom_daemon" "$(loom_mcp_pick_persona 42 "" loom_daemon)" \
    "empty pool falls back to scalar persona"

# pick_persona round-robin: distinct concurrent issues -> distinct personas
pool="loom_b1,loom_b2,loom_b3,loom_b4"
p10="$(loom_mcp_pick_persona 10 "$pool" loom_daemon)"
p11="$(loom_mcp_pick_persona 11 "$pool" loom_daemon)"
p12="$(loom_mcp_pick_persona 12 "$pool" loom_daemon)"
assert_eq "loom_b3" "$p10" "issue 10 -> pool[10%4]=pool[2]"
assert_eq "loom_b4" "$p11" "issue 11 -> pool[11%4]=pool[3]"
assert_eq "loom_b1" "$p12" "issue 12 -> pool[12%4]=pool[0]"
# The acceptance-critical property: two concurrent workers differ.
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$p10" != "$p11" && "$p11" != "$p12" && "$p10" != "$p12" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: three concurrent issues (10,11,12) get three distinct personas"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: concurrent personas not distinct ($p10/$p11/$p12)"
fi

# unknown issue slot -> deterministic first pool entry
assert_eq "loom_b1" "$(loom_mcp_pick_persona "" "$pool" loom_daemon)" \
    "unknown issue slot -> first pool entry"
assert_eq "loom_b1" "$(loom_mcp_pick_persona "not-a-number" "$pool" loom_daemon)" \
    "non-numeric issue slot -> first pool entry"

# worker personas from env
assert_eq "a,b,c" \
    "$(LOOM_SAFEHOUSE_WORKER_PERSONAS=a,b,c loom_mcp_worker_personas "$_empty_repo")" \
    "LOOM_SAFEHOUSE_WORKER_PERSONAS env pool is read"

if $HAVE_JQ; then
    _pool_repo="$(mktemp -d)"
    mkdir -p "$_pool_repo/.loom"
    cat >"$_pool_repo/.loom/config.json" <<'JSON'
{ "safehouse": { "workerPersonas": ["loom_builder_1", "loom_builder_2"] } }
JSON
    assert_eq "loom_builder_1,loom_builder_2" \
        "$(loom_mcp_worker_personas "$_pool_repo")" \
        "config safehouse.workerPersonas array is joined to CSV"
    rm -rf "$_pool_repo"
fi

# ============================================================
# Section 4: emit config — loom first, safehouse additive, no secrets
# ============================================================
echo "Testing loom_mcp_emit_config..."

loom_only="$(loom_mcp_emit_config /ws/root)"
assert_contains '"loom"' "$loom_only" "loom-only config contains loom server"
assert_not_contains '"safehouse"' "$loom_only" \
    "loom-only config has NO safehouse entry (no socket/persona/command)"
assert_contains '/ws/root/mcp-loom/dist/index.js' "$loom_only" \
    "loom-only config points at mcp-loom entry"

two_server="$(loom_mcp_emit_config /ws/root /run/sh.sock loom_builder_1 safehouse-mcp)"
assert_contains '"safehouse"' "$two_server" "two-server config contains safehouse"
assert_contains '"SAFEHOUSED_SOCKET": "/run/sh.sock"' "$two_server" \
    "safehouse env carries the socket path"
assert_contains '"SAFEHOUSE_PERSONA": "loom_builder_1"' "$two_server" \
    "safehouse env carries the per-worker persona"

# loom MUST appear before safehouse in iteration order.
loom_pos="$(printf '%s' "$two_server" | grep -n '"loom"' | head -1 | cut -d: -f1)"
sh_pos="$(printf '%s' "$two_server" | grep -n '"safehouse"' | head -1 | cut -d: -f1)"
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -n "$loom_pos" && -n "$sh_pos" && "$loom_pos" -lt "$sh_pos" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: loom server appears before safehouse (line $loom_pos < $sh_pos)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: loom must precede safehouse (loom@$loom_pos safehouse@$sh_pos)"
fi

# Security: no token/key/secret words in the emitted config.
for word in token TOKEN api_key API_KEY secret SECRET password OAUTH oauth; do
    assert_not_contains "$word" "$two_server" "emitted config contains no '$word'"
done

# ============================================================
# Section 5: claude-wrapper.sh MCP pre-flight resolves loom with 2 servers
# ============================================================
echo "Testing claude-wrapper.sh MCP pre-flight extraction with two servers..."

_pf_dir="$(mktemp -d)"
loom_mcp_emit_config /ws/root /run/sh.sock loom_builder_1 safehouse-mcp \
    >"$_pf_dir/.mcp.json"

# This is the EXACT extraction logic from defaults/scripts/claude-wrapper.sh
# check_mcp_server(): the first server with args wins.
_entry="$(python3 -c "
import json, sys
with open('$_pf_dir/.mcp.json') as f:
    cfg = json.load(f)
servers = cfg.get('mcpServers', {})
for name, srv in servers.items():
    args = srv.get('args', [])
    if args:
        print(args[-1])
        sys.exit(0)
")"
assert_eq "/ws/root/mcp-loom/dist/index.js" "$_entry" \
    "pre-flight extracts the loom entry point (first server with args) with safehouse present"
rm -rf "$_pf_dir"

# ============================================================
# Section 6: byte-identical setup-mcp.sh output when safehouse disabled
# ============================================================
echo "Testing setup-mcp.sh byte-identical loom-only output (safehouse disabled)..."

_run_setup_mcp() {
    # Build an isolated workspace with a pre-built (fake) mcp bundle so
    # setup-mcp.sh skips the npm build, then run it and echo the produced JSON.
    local ws="$1"
    mkdir -p "$ws/scripts" "$ws/mcp-loom/dist" \
        "$ws/defaults/scripts/lib"
    # Fake, non-stale bundle (no src/ dir -> no staleness rebuild).
    : >"$ws/mcp-loom/dist/index.js"
    cp "$REPO_ROOT/scripts/setup-mcp.sh" "$ws/scripts/setup-mcp.sh"
    cp "$SCRIPTS_DIR/lib/mcp-config.sh" "$ws/defaults/scripts/lib/mcp-config.sh"
    cp "$SCRIPTS_DIR/lib/config-resolver.sh" "$ws/defaults/scripts/lib/config-resolver.sh"
    ( bash "$ws/scripts/setup-mcp.sh" >/dev/null 2>&1 )
    cat "$ws/.mcp.json"
}

_disabled_ws="$(mktemp -d)"
actual_disabled="$(_run_setup_mcp "$_disabled_ws")"
read -r -d '' expected_disabled <<EOF || true
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$_disabled_ws/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$_disabled_ws"
      }
    }
  }
}
EOF
assert_eq "$expected_disabled" "$actual_disabled" \
    "setup-mcp.sh with no safehouse block is byte-identical to pre-#3999 output"
rm -rf "$_disabled_ws"

# setup-mcp.sh with safehouse enabled emits a two-server file, loom first.
if $HAVE_JQ; then
    _enabled_ws="$(mktemp -d)"
    mkdir -p "$_enabled_ws/scripts" "$_enabled_ws/mcp-loom/dist" \
        "$_enabled_ws/defaults/scripts/lib" "$_enabled_ws/.loom"
    : >"$_enabled_ws/mcp-loom/dist/index.js"
    : >"$_enabled_ws/sh.sock" # a resolvable socket path
    cp "$REPO_ROOT/scripts/setup-mcp.sh" "$_enabled_ws/scripts/setup-mcp.sh"
    cp "$SCRIPTS_DIR/lib/mcp-config.sh" "$_enabled_ws/defaults/scripts/lib/mcp-config.sh"
    cp "$SCRIPTS_DIR/lib/config-resolver.sh" "$_enabled_ws/defaults/scripts/lib/config-resolver.sh"
    cat >"$_enabled_ws/.loom/config.json" <<JSON
{ "safehouse": { "enabled": true, "socket": "$_enabled_ws/sh.sock",
  "persona": "loom_daemon", "mcpCommand": "cat" } }
JSON
    ( bash "$_enabled_ws/scripts/setup-mcp.sh" >/dev/null 2>&1 )
    enabled_out="$(cat "$_enabled_ws/.mcp.json")"
    assert_contains '"safehouse"' "$enabled_out" \
        "setup-mcp.sh with safehouse enabled emits a safehouse server"
    assert_contains "$_enabled_ws/sh.sock" "$enabled_out" \
        "setup-mcp.sh safehouse entry carries the socket path"
    # loom still first
    e_loom="$(printf '%s' "$enabled_out" | grep -n '"loom"' | head -1 | cut -d: -f1)"
    e_sh="$(printf '%s' "$enabled_out" | grep -n '"safehouse"' | head -1 | cut -d: -f1)"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ -n "$e_loom" && -n "$e_sh" && "$e_loom" -lt "$e_sh" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: setup-mcp.sh keeps loom before safehouse"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: setup-mcp.sh loom must precede safehouse"
    fi
    rm -rf "$_enabled_ws"
else
    echo -e "  ${YELLOW}SKIP${NC}: setup-mcp.sh enabled test (jq not installed)"
fi

rm -rf "$_empty_repo"

# ============================================================
# Summary
# ============================================================
echo ""
echo "========================================"
echo "Test Results:"
echo "  Total:  $TESTS_RUN"
echo -e "  ${GREEN}Passed: $TESTS_PASSED${NC}"
if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo -e "  ${RED}Failed: $TESTS_FAILED${NC}"
    exit 1
fi
echo -e "  ${GREEN}All tests passed!${NC}"
