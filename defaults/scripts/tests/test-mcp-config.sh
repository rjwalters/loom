#!/usr/bin/env bash
# test-mcp-config.sh — Tests for Loom's MCP configuration plumbing.
#
# Covers:
#   1. claude-wrapper.sh's MCP pre-flight resolves the `loom` entry point from a
#      multi-server config (the first server with args wins).
#   2. Demotion (#4230): scripts/setup-mcp.sh no longer emits a `loom` entry;
#      it strips any pre-existing project-scope `loom` entry (shadowing
#      migration) and preserves other servers.
#   3. claude-wrapper.sh pre-flight fallbacks, stale-bundle self-repair, node
#      resolution, global/workspace checks (#4349, #5032, #5033, #5314, #10870).
#
# The worker-session Matrix MCP injection this file originally covered was
# removed with the Matrix integration (#11112).
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

# ============================================================
# Section 5: claude-wrapper.sh MCP pre-flight resolves loom with 2 servers
# ============================================================
echo "Testing claude-wrapper.sh MCP pre-flight extraction with two servers..."

_pf_dir="$(mktemp -d)"
cat >"$_pf_dir/.mcp.json" <<'JSON'
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["/ws/root/mcp-loom/dist/index.js"],
      "env": { "LOOM_WORKSPACE": "/ws/root" }
    },
    "other": { "command": "other-mcp", "args": ["--stdio"] }
  }
}
JSON

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
    "pre-flight extracts the loom entry point (first server with args) with another server present"
rm -rf "$_pf_dir"

# ============================================================
# Section 6: DEMOTED setup-mcp.sh — no `loom` entry, shadowing migration (#4230)
# ============================================================
# scripts/setup-mcp.sh lives at the repo root, not under defaults/, so it is
# never shipped into an installed consumer repo. Section 6 exercises it
# directly and is therefore source-tree-only by design (#6194/#6241) — it
# SKIPs (rather than errors) when run outside Loom's own checkout, while the
# rest of this suite still runs normally.
if [[ ! -f "$REPO_ROOT/scripts/setup-mcp.sh" ]]; then
    echo -e "  ${YELLOW}SKIP${NC}: Section 6 (setup-mcp.sh demotion tests) — $REPO_ROOT/scripts/setup-mcp.sh not found (not shipped into an installed repo)"
else
echo "Testing setup-mcp.sh demotion: no loom entry + shadowing migration (#4230)..."

_seed_setup_mcp_ws() {
    # Build an isolated workspace with a pre-built (fake) mcp bundle so
    # setup-mcp.sh skips the npm build.
    local ws="$1"
    mkdir -p "$ws/scripts" "$ws/mcp-loom/dist"
    : >"$ws/mcp-loom/dist/index.js"
    cp "$REPO_ROOT/scripts/setup-mcp.sh" "$ws/scripts/setup-mcp.sh"
}

# 6a. No pre-existing .mcp.json: emits NO loom entry
#     (the loom server is user-scoped now). No .mcp.json is created.
_disabled_ws="$(mktemp -d)"
_seed_setup_mcp_ws "$_disabled_ws"
( bash "$_disabled_ws/scripts/setup-mcp.sh" >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if [[ ! -f "$_disabled_ws/.mcp.json" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: setup-mcp.sh emits no loom .mcp.json (#4230)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: setup-mcp.sh must not create a loom .mcp.json (found $(cat "$_disabled_ws/.mcp.json"))"
fi
rm -rf "$_disabled_ws"

# 6b. Shadowing migration: a pre-existing loom-only .mcp.json is REMOVED so it
#     can no longer outrank the user-scope server.
_stale_ws="$(mktemp -d)"
_seed_setup_mcp_ws "$_stale_ws"
cat >"$_stale_ws/.mcp.json" <<JSON
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$_stale_ws/mcp-loom/dist/index.js"],
      "env": { "LOOM_WORKSPACE": "$_stale_ws" }
    }
  }
}
JSON
( bash "$_stale_ws/scripts/setup-mcp.sh" >/dev/null 2>&1 )
TESTS_RUN=$((TESTS_RUN + 1))
if [[ ! -f "$_stale_ws/.mcp.json" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: setup-mcp.sh removes a stale loom-only .mcp.json (shadowing migration, #4230)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: stale loom-only .mcp.json must be removed ($(cat "$_stale_ws/.mcp.json"))"
fi
rm -rf "$_stale_ws"

# 6c. Shadowing migration preserves OTHER servers: a .mcp.json with loom + a
#     third-party server keeps the third-party server, drops only loom.
_mixed_ws="$(mktemp -d)"
_seed_setup_mcp_ws "$_mixed_ws"
cat >"$_mixed_ws/.mcp.json" <<JSON
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$_mixed_ws/mcp-loom/dist/index.js"],
      "env": { "LOOM_WORKSPACE": "$_mixed_ws" }
    },
    "other": { "command": "cat", "args": [] }
  }
}
JSON
( bash "$_mixed_ws/scripts/setup-mcp.sh" >/dev/null 2>&1 )
mixed_out="$(cat "$_mixed_ws/.mcp.json" 2>/dev/null || echo "")"
assert_not_contains '"loom"' "$mixed_out" \
    "setup-mcp.sh strips the loom entry from a mixed .mcp.json (#4230)"
assert_contains '"other"' "$mixed_out" \
    "setup-mcp.sh preserves non-loom servers during migration (#4230)"
rm -rf "$_mixed_ws"

fi

# ============================================================
# Section 7: claude-wrapper.sh MCP pre-flight candidate-list fallback (#4349)
#
# A broken worktree's dangling .mcp.json must not hard-fail the pre-flight
# when the git common dir (primary checkout) carries a healthy config —
# check_mcp_server() now tries an ORDERED candidate list (WORKSPACE first,
# then the git common dir) instead of pinning to whichever resolves first.
# ============================================================
echo ""
echo "Testing claude-wrapper.sh MCP pre-flight candidate-list fallback (#4349)..."

WRAPPER="$SCRIPTS_DIR/claude-wrapper.sh"
HAVE_NODE=false
command -v node >/dev/null 2>&1 && HAVE_NODE=true
HAVE_GIT=false
command -v git >/dev/null 2>&1 && HAVE_GIT=true

# Healthy mcp-loom stub: prints the historical startup banner (kept, so a
# banner-printing server is still exercised) AND responds to the
# protocol-level `initialize` handshake _mcp_smoke_test now performs (#143 /
# 2am#307) — the health check no longer greps for the banner string, it
# validates a JSON-RPC response on stdout.
_write_healthy_mcp_stub() {
    local dir="$1" # e.g. .../mcp-loom
    mkdir -p "$dir/dist"
    cat >"$dir/dist/index.js" <<'JS'
process.stderr.write("Loom MCP server running on stdio\n");
let buf = "";
process.stdin.on("data", (chunk) => {
    buf += chunk;
    if (buf.includes("\n")) {
        process.stdout.write(
            JSON.stringify({
                jsonrpc: "2.0",
                id: 1,
                result: { protocolVersion: "2024-11-05", capabilities: {}, serverInfo: { name: "stub", version: "0.0.0" } },
            }) + "\n"
        );
        process.exit(0);
    }
});
JS
}

if $HAVE_NODE && $HAVE_GIT; then
    _fx="$(mktemp -d)"
    git init -q "$_fx/main"
    git -C "$_fx/main" config user.email t@t.com
    git -C "$_fx/main" config user.name t
    echo x >"$_fx/main/README.md"
    git -C "$_fx/main" add -A
    git -C "$_fx/main" commit -qm init
    _main_root="$(cd "$_fx/main" && pwd)" # resolve any /private symlink up front

    _write_healthy_mcp_stub "$_main_root/mcp-loom"
    cat >"$_main_root/.mcp.json" <<JSON
{ "mcpServers": { "loom": { "command": "node", "args": ["$_main_root/mcp-loom/dist/index.js"] } } }
JSON
    git -C "$_main_root" add -A
    git -C "$_main_root" commit -qm "add mcp"
    git -C "$_main_root" worktree add -q -b feature/pf-test "$_fx/wt"
    _wt_root="$(cd "$_fx/wt" && pwd)"

    # 7a. Broken worktree candidate (no rebuildable package) + healthy common-dir
    #     candidate -> pre-flight PASSES via fallback, with one WARN naming it.
    _write_healthy_mcp_stub "$_wt_root/mcp-loom"
    cat >"$_wt_root/.mcp.json" <<JSON
{ "mcpServers": { "loom": { "command": "node", "args": ["$_wt_root/mcp-loom/dist/index.js"] } } }
JSON
    rm -rf "$_wt_root/mcp-loom" # dangling entry point, no package.json to rebuild

    _fallback_out="$(LOOM_WORKSPACE="$_wt_root" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_mcp_server
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "RC=0" "$_fallback_out" \
        "broken worktree + healthy common-dir config: pre-flight passes via fallback (#4349)"
    assert_contains "falling back to" "$_fallback_out" \
        "fallback path logs one WARN naming the fallback candidate (#4349)"
    assert_not_contains "MCP_PREFLIGHT_FAILED" "$_fallback_out" \
        "fallback path never emits the MCP_PREFLIGHT_FAILED sentinel"

    # 7b. Both candidates broken (no rebuildable package anywhere) -> hard FAIL.
    rm -rf "$_main_root/mcp-loom"
    _both_broken_out="$(LOOM_WORKSPACE="$_wt_root" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_mcp_server
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1 || true)"
    assert_not_contains "RC=0" "$_both_broken_out" \
        "both candidates broken: pre-flight does NOT report success"
    assert_contains "MCP pre-flight failed for all" "$_both_broken_out" \
        "both candidates broken: hard failure names all failed candidates (#4349)"

    # 7c. Neither candidate has a .mcp.json at all -> #4230 non-fatal skip preserved.
    _no_cfg_ws="$(mktemp -d)"
    _no_cfg_out="$(LOOM_WORKSPACE="$_no_cfg_ws" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_mcp_server
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "RC=0" "$_no_cfg_out" \
        "no .mcp.json anywhere: pre-flight is a non-fatal skip (#4230 preserved)"
    assert_contains "#4230" "$_no_cfg_out" \
        "no .mcp.json anywhere: skip message cites #4230"
    rm -rf "$_no_cfg_ws"

    git -C "$_main_root" worktree remove --force "$_wt_root" >/dev/null 2>&1 || true
    rm -rf "$_fx"
else
    echo -e "  ${YELLOW}SKIP${NC}: MCP pre-flight fallback tests (node and/or git not installed)"
fi

# ============================================================
# Section 8: stale-bundle rebuild — abort on failure + npm ci self-repair (#5032)
#
# Before #5032 a failed rebuild on the STALE-bundle path was swallowed
# ("continuing with existing bundle") and fell through to a smoke test against
# the very same known-broken dist — guaranteed to fail, with no actionable
# message. And _try_mcp_rebuild only ever ran `npm run build`, so a
# missing/half-installed node_modules (the laptop-host root cause: an EMPTY
# node_modules/@modelcontextprotocol/sdk directory) was indistinguishable from
# a genuine build-source error and needed an operator to run `npm ci` by hand.
# ============================================================
echo ""
echo "Testing stale-bundle rebuild abort + npm ci self-repair (#5032)..."

# A dist stub that announces itself on stderr. If the smoke test ever runs, the
# marker shows up in the captured wrapper output — that is how the tests below
# assert "did NOT fall through to the smoke test".
_write_broken_dist() {
    local dir="$1" # e.g. .../mcp-loom
    mkdir -p "$dir/dist"
    cat >"$dir/dist/index.js" <<'JS'
process.stderr.write("SMOKE_TEST_RAN: bundle is broken\n");
process.exit(1);
JS
}

# npm stub: records every invocation and simulates ci/build outcomes.
# Driven by marker files in $NPM_STUB_DIR so each fixture can pick a behavior
# without regenerating the script.
_write_npm_stub() {
    local stub="$1"
    mkdir -p "$(dirname "$stub")"
    cat >"$stub" <<'STUB'
#!/usr/bin/env bash
echo "$*" >>"${NPM_STUB_LOG}"
case "${1:-}" in
    ci)
        if [[ -f "${NPM_STUB_DIR}/ci-fails" ]]; then
            echo "npm ERR! network request to https://registry.npmjs.org failed" >&2
            exit 1
        fi
        mkdir -p node_modules/@modelcontextprotocol/sdk node_modules/typescript
        echo '{"name":"@modelcontextprotocol/sdk","version":"1.0.0"}' \
            >node_modules/@modelcontextprotocol/sdk/package.json
        echo '{"name":"typescript","version":"5.0.0"}' \
            >node_modules/typescript/package.json
        exit 0
        ;;
    run)
        if [[ -f "${NPM_STUB_DIR}/build-fails" ]]; then
            echo "src/index.ts(1,1): error TS1005: build source error" >&2
            exit 1
        fi
        if [[ ! -f node_modules/@modelcontextprotocol/sdk/package.json ]]; then
            echo "Error: Cannot find module '@modelcontextprotocol/sdk' (MODULE_NOT_FOUND)" >&2
            exit 1
        fi
        mkdir -p dist
        cat >dist/index.js <<'JSEOF'
process.stderr.write("Loom MCP server running on stdio\n");
let buf = "";
process.stdin.on("data", (chunk) => {
    buf += chunk;
    if (buf.includes("\n")) {
        process.stdout.write(
            JSON.stringify({
                jsonrpc: "2.0",
                id: 1,
                result: { protocolVersion: "2024-11-05", capabilities: {}, serverInfo: { name: "stub", version: "0.0.0" } },
            }) + "\n"
        );
        process.exit(0);
    }
});
JSEOF
        exit 0
        ;;
esac
exit 0
STUB
    chmod +x "$stub"
}

# Build an isolated candidate workspace with a STALE bundle (src newer than
# dist) and a package.json declaring one runtime + one dev dependency.
# $2 selects the node_modules state: complete | empty-shell | absent
# $3 = "lock" to drop a package-lock.json alongside.
_seed_stale_mcp_ws() {
    local ws="$1" deps_state="$2" lock="${3:-}"
    local pkg="$ws/mcp-loom"
    mkdir -p "$pkg/src"
    _write_broken_dist "$pkg"
    cat >"$pkg/package.json" <<'JSON'
{
  "name": "mcp-loom",
  "version": "1.0.0",
  "dependencies": { "@modelcontextprotocol/sdk": "^1.0.0" },
  "devDependencies": { "typescript": "^5.0.0" }
}
JSON
    [[ "$lock" == "lock" ]] && echo '{"lockfileVersion":3}' >"$pkg/package-lock.json"
    case "$deps_state" in
        complete)
            mkdir -p "$pkg/node_modules/@modelcontextprotocol/sdk" "$pkg/node_modules/typescript"
            echo '{"name":"@modelcontextprotocol/sdk"}' \
                >"$pkg/node_modules/@modelcontextprotocol/sdk/package.json"
            echo '{"name":"typescript"}' >"$pkg/node_modules/typescript/package.json"
            ;;
        empty-shell)
            # The laptop-host shape: node_modules exists and is non-empty, but the
            # sdk package directory is an empty husk.
            mkdir -p "$pkg/node_modules/@modelcontextprotocol/sdk" "$pkg/node_modules/typescript"
            echo '{"name":"typescript"}' >"$pkg/node_modules/typescript/package.json"
            ;;
        absent) : ;;
    esac
    cat >"$ws/.mcp.json" <<JSON
{ "mcpServers": { "loom": { "command": "node", "args": ["$pkg/dist/index.js"] } } }
JSON
    # Force staleness deterministically (no sleep): dist in 2020, src in 2025.
    touch -t 202001010000 "$pkg/dist/index.js"
    : >"$pkg/src/index.ts"
    touch -t 202501010000 "$pkg/src/index.ts"
}

# Run _check_mcp_candidate against a workspace with the npm stub wired in.
# The rc is captured with `|| rc=$?` because the wrapper sets `set -e` when
# sourced — a bare failing call would kill the harness before the RC echo.
_run_check_candidate() {
    local ws="$1" stub_dir="$2"
    NPM_STUB_DIR="$stub_dir" NPM_STUB_LOG="$stub_dir/npm-calls.log" \
    LOOM_NPM_BIN="$stub_dir/npm" LOOM_WORKSPACE="$ws" CLAUDE_WRAPPER_SOURCE_ONLY=1 \
        bash -c '
            source "$1"
            rc=0
            _check_mcp_candidate "$2" || rc=$?
            echo "RC=$rc"
        ' _ "$WRAPPER" "$ws" 2>&1 || true
}

if $HAVE_NODE; then
    # --- 8a. Stale bundle, healthy deps, genuine build-source failure ---------
    _s8a="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8a/ws" complete lock
    _write_npm_stub "$_s8a/stub/npm"
    : >"$_s8a/stub/npm-calls.log"
    : >"$_s8a/stub/build-fails"
    _out8a="$(_run_check_candidate "$_s8a/ws" "$_s8a/stub")"
    _log8a="$(cat "$_s8a/stub/npm-calls.log")"

    assert_contains "RC=1" "$_out8a" \
        "8a: stale bundle + failed rebuild aborts the candidate (rc 1, #5032)"
    assert_contains "aborting this candidate" "$_out8a" \
        "8a: abort message replaces 'continuing with existing bundle' (#5032)"
    assert_contains "Repair manually" "$_out8a" \
        "8a: abort message names the repair command (#5032)"
    assert_not_contains "continuing with existing bundle" "$_out8a" \
        "8a: the swallow-and-continue branch is gone (#5032)"
    assert_not_contains "SMOKE_TEST_RAN" "$_out8a" \
        "8a: does NOT fall through to a smoke test against the broken bundle (#5032)"
    assert_contains "run build" "$_log8a" \
        "8a: npm run build was attempted"
    assert_not_contains "ci" "$_log8a" \
        "8a: npm ci is NOT run when node_modules is complete (not a repairable case)"
    rm -rf "$_s8a"

    # --- 8b. Stale bundle, empty-shell dependency + lockfile -> npm ci repair --
    _s8b="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8b/ws" empty-shell lock
    _write_npm_stub "$_s8b/stub/npm"
    : >"$_s8b/stub/npm-calls.log"
    _out8b="$(_run_check_candidate "$_s8b/ws" "$_s8b/stub")"
    _log8b="$(cat "$_s8b/stub/npm-calls.log")"

    assert_contains "ci" "$_log8b" \
        "8b: empty node_modules package + package-lock.json triggers npm ci (#5032)"
    assert_contains "run build" "$_log8b" \
        "8b: npm run build still runs after the npm ci repair (#5032)"
    assert_contains "RC=0" "$_out8b" \
        "8b: candidate becomes healthy after self-repair, no operator action (#5032)"
    assert_contains "self-repair" "$_out8b" \
        "8b: the self-repair attempt is logged"
    rm -rf "$_s8b"

    # --- 8b2. node_modules entirely absent + lockfile -> npm ci repair --------
    _s8b2="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8b2/ws" absent lock
    _write_npm_stub "$_s8b2/stub/npm"
    : >"$_s8b2/stub/npm-calls.log"
    _out8b2="$(_run_check_candidate "$_s8b2/ws" "$_s8b2/stub")"
    assert_contains "ci" "$(cat "$_s8b2/stub/npm-calls.log")" \
        "8b2: missing node_modules + package-lock.json triggers npm ci (#5032)"
    assert_contains "RC=0" "$_out8b2" \
        "8b2: candidate healthy after repairing a missing node_modules (#5032)"
    rm -rf "$_s8b2"

    # --- 8c. npm ci itself fails (no network) -> abort, no fall-through -------
    _s8c="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8c/ws" empty-shell lock
    _write_npm_stub "$_s8c/stub/npm"
    : >"$_s8c/stub/npm-calls.log"
    : >"$_s8c/stub/ci-fails"
    _out8c="$(_run_check_candidate "$_s8c/ws" "$_s8c/stub")"
    _log8c="$(cat "$_s8c/stub/npm-calls.log")"

    assert_contains "RC=1" "$_out8c" \
        "8c: failed npm ci aborts the candidate (#5032)"
    assert_contains "npm ci failed" "$_out8c" \
        "8c: failed npm ci produces an actionable error (#5032)"
    assert_contains "Repair manually" "$_out8c" \
        "8c: failed npm ci names the repair command (#5032)"
    assert_not_contains "SMOKE_TEST_RAN" "$_out8c" \
        "8c: failed npm ci does NOT fall through to the smoke test (#5032)"
    assert_not_contains "run build" "$_log8c" \
        "8c: failed npm ci short-circuits before npm run build (#5032)"
    rm -rf "$_s8c"

    # --- 8d. No lockfile: cannot self-repair, but still aborts actionably ----
    _s8d="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8d/ws" empty-shell   # no lockfile
    _write_npm_stub "$_s8d/stub/npm"
    : >"$_s8d/stub/npm-calls.log"
    _out8d="$(_run_check_candidate "$_s8d/ws" "$_s8d/stub")"
    _log8d="$(cat "$_s8d/stub/npm-calls.log")"
    assert_not_contains "ci" "$_log8d" \
        "8d: no package-lock.json -> npm ci is not attempted (#5032)"
    assert_contains "no package-lock.json" "$_out8d" \
        "8d: the un-repairable case is explained in the log (#5032)"
    assert_contains "RC=1" "$_out8d" \
        "8d: un-repairable stale bundle still aborts rather than falling through (#5032)"
    rm -rf "$_s8d"

    # --- 8e. Missing entry point (regression guard: behavior UNCHANGED) ------
    # The sibling path already returned 1 on rebuild failure and 0 on success.
    _s8e="$(mktemp -d)"
    _seed_stale_mcp_ws "$_s8e/ws" complete lock
    rm -f "$_s8e/ws/mcp-loom/dist/index.js"   # entry point missing, not stale
    _write_npm_stub "$_s8e/stub/npm"
    : >"$_s8e/stub/npm-calls.log"
    _out8e_ok="$(_run_check_candidate "$_s8e/ws" "$_s8e/stub")"
    assert_contains "RC=0" "$_out8e_ok" \
        "8e: missing entry point + successful rebuild still returns 0 (unchanged)"
    assert_contains "MCP entry point missing" "$_out8e_ok" \
        "8e: missing entry point still logs its own warning (unchanged)"

    rm -f "$_s8e/ws/mcp-loom/dist/index.js"
    : >"$_s8e/stub/build-fails"
    _out8e_fail="$(_run_check_candidate "$_s8e/ws" "$_s8e/stub")"
    assert_contains "RC=1" "$_out8e_fail" \
        "8e: missing entry point + failed rebuild still returns 1 (unchanged)"
    rm -rf "$_s8e"
else
    echo -e "  ${YELLOW}SKIP${NC}: stale-bundle rebuild tests (node not installed)"
fi

# ============================================================
# 8f. Pre-flight applies the server's declared .mcp.json `env` (#10870)
# ============================================================
# Claude Code launches an MCP server with its `.mcp.json` `env` merged over the
# inherited env; the pre-flight must too, or a server that needs its env
# (squad's SQUAD_RUNTIME) fails MCP_PREFLIGHT_FAILED while working in a session.
echo ""
echo "Testing MCP pre-flight applies the server's declared env (#10870)..."

if $HAVE_NODE; then
    _s8f="$(mktemp -d)"
    mkdir -p "$_s8f/ws" "$_s8f/squad/dist" # no package.json: a failed smoke test cannot rebuild
    # Exits non-zero (no handshake) unless SQUAD_RUNTIME carries the exact
    # declared value — spaces and quotes included, so word-splitting fails it.
    cat >"$_s8f/squad/dist/mcp.js" <<'JS'
if (process.env.SQUAD_RUNTIME !== "rt with 'quotes' \"and\" spaces") process.exit(3);
process.stdin.once("data", () => {
    process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }) + "\n");
    process.exit(0);
});
JS
    _run_8f() {
        env -u SQUAD_RUNTIME LOOM_WORKSPACE="$_s8f/ws" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
            source "$1"
            rc=0
            _check_mcp_candidate "$2" || rc=$?
            echo "RC=$rc LEAK=${SQUAD_RUNTIME:-none}"
        ' _ "$WRAPPER" "$_s8f/ws" 2>&1 || true
    }

    # Negative: no declared env -> the same fixture still fails pre-flight.
    cat >"$_s8f/ws/.mcp.json" <<JSON
{ "mcpServers": { "squad": { "command": "node", "args": ["$_s8f/squad/dist/mcp.js"] } } }
JSON
    _out8f_neg="$(_run_8f)"
    assert_contains "RC=1" "$_out8f_neg" \
        "8f: fixture needing SQUAD_RUNTIME fails pre-flight when .mcp.json declares no env"

    # Positive: .mcp.json declares it -> pre-flight passes.
    cat >"$_s8f/ws/.mcp.json" <<JSON
{ "mcpServers": { "squad": { "command": "node", "args": ["$_s8f/squad/dist/mcp.js"],
  "env": { "SQUAD_RUNTIME": "rt with 'quotes' \\"and\\" spaces", "SQUAD_PORT": 7 } } } }
JSON
    _out8f_pos="$(_run_8f)"
    assert_contains "RC=0" "$_out8f_pos" \
        "8f: declared .mcp.json env is merged into the smoke-test launch (#10870)"
    assert_contains "smoke-testing server 'squad'" "$_out8f_pos" \
        "8f: pre-flight log names the server it smoke-tested (#10870)"
    assert_contains "LEAK=none" "$_out8f_pos" \
        "8f: declared env is confined to the server launch, not the wrapper"
    assert_not_contains "rt with" "$_out8f_pos" \
        "8f: declared env values are never logged"
    rm -rf "$_s8f"
else
    echo -e "  ${YELLOW}SKIP${NC}: declared-env pre-flight tests (node not installed)"
fi

# ============================================================
# Section 9: node/npm resolution without a login PATH (#5032)
#
# claude-wrapper runs from launchd / `ssh host 'cmd'` non-login shells that
# never source the login profile, so /opt/homebrew/bin is not on PATH. The
# rebuild path must resolve npm/node from an ordered explicit candidate list
# (the lib/locate-daemon-bin.sh pattern from #4875), not a bare PATH lookup.
# ============================================================
echo ""
echo "Testing node/npm resolution with a minimal PATH (#5032)..."

_BASH_BIN="$(command -v bash)"

_locate_node_tool_in_env() {
    # $1 = tool, remaining args = env assignments.
    # `bash` is invoked by absolute path so the assignments may narrow $PATH.
    local tool="$1"; shift
    env "$@" CLAUDE_WRAPPER_SOURCE_ONLY=1 "$_BASH_BIN" -c '
        source "$1" >/dev/null 2>&1
        _locate_node_tool "$2" || true
    ' _ "$WRAPPER" "$tool" 2>/dev/null || true
}

_fake_node="$(mktemp -d)/node"
mkdir -p "$(dirname "$_fake_node")"
printf '#!/usr/bin/env bash\necho fake\n' >"$_fake_node"
chmod +x "$_fake_node"
assert_eq "$_fake_node" "$(_locate_node_tool_in_env node "LOOM_NODE_BIN=$_fake_node")" \
    "LOOM_NODE_BIN override wins over PATH (#5032)"
assert_eq "$_fake_node" "$(_locate_node_tool_in_env npm "LOOM_NPM_BIN=$_fake_node")" \
    "LOOM_NPM_BIN override wins over PATH (#5032)"
rm -rf "$(dirname "$_fake_node")"

if $HAVE_NODE; then
    _real_node="$(command -v node)"
    _real_node_dir="$(dirname "$_real_node")"
    case "$_real_node_dir" in
        # node lives in a candidate dir that a non-login shell's default
        # PATH (/usr/bin:/bin) does NOT include — exactly the #5032 scenario.
        /opt/homebrew/bin|/usr/local/bin|/opt/local/bin|/snap/bin)
            assert_eq "$_real_node" \
                "$(_locate_node_tool_in_env node 'PATH=/usr/bin:/bin')" \
                "node resolves from the explicit candidate list under a non-login PATH (#5032)"
            ;;
        *)
            echo -e "  ${YELLOW}SKIP${NC}: minimal-PATH node resolution (node is already on the non-login PATH: $_real_node_dir)"
            ;;
    esac
else
    echo -e "  ${YELLOW}SKIP${NC}: minimal-PATH node resolution (node not installed)"
fi

# ============================================================
# Section 10: _mcp_node_modules_unusable classification (#5032)
# ============================================================
echo ""
echo "Testing _mcp_node_modules_unusable classification (#5032)..."

_deps_unusable_rc() {
    CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1" >/dev/null 2>&1
        if _mcp_node_modules_unusable "$2"; then echo "unusable"; else echo "ok"; fi
    ' _ "$WRAPPER" "$1" 2>/dev/null
}

_cls="$(mktemp -d)"
for _state in complete empty-shell absent; do
    mkdir -p "$_cls/$_state"
    _seed_stale_mcp_ws "$_cls/$_state" "$_state" lock
done
assert_eq "ok" "$(_deps_unusable_rc "$_cls/complete/mcp-loom")" \
    "complete node_modules is usable (a build failure there is a real source error)"
assert_eq "unusable" "$(_deps_unusable_rc "$_cls/empty-shell/mcp-loom")" \
    "an EMPTY dependency directory is classified unusable (laptop-host root cause, #5032)"
assert_eq "unusable" "$(_deps_unusable_rc "$_cls/absent/mcp-loom")" \
    "a missing node_modules is classified unusable (#5032)"
mkdir -p "$_cls/emptydir/mcp-loom/node_modules"
cp "$_cls/complete/mcp-loom/package.json" "$_cls/emptydir/mcp-loom/package.json"
assert_eq "unusable" "$(_deps_unusable_rc "$_cls/emptydir/mcp-loom")" \
    "a completely empty node_modules directory is classified unusable (#5032)"
rm -rf "$_cls"

# ============================================================
# Section 11: check_global_mcp_configs presence check (#5033)
#
# The function must warn when the user-scope `loom` MCP registration
# (#4230) is absent from ~/.claude.json's mcpServers — not just validate
# already-present entries' binary paths — while remaining warning-only
# (never aborts, RC always 0).
# ============================================================
echo ""
echo "Testing check_global_mcp_configs presence check (#5033)..."

HAVE_PYTHON3=false
command -v python3 >/dev/null 2>&1 && HAVE_PYTHON3=true

if $HAVE_PYTHON3; then
    # 11a. No mcpServers key at all (the laptop-host incident case) -> warns.
    _home_missing_key="$(mktemp -d)"
    cat >"$_home_missing_key/.claude.json" <<'JSON'
{ "other": "stuff" }
JSON
    _out_missing_key="$(HOME="$_home_missing_key" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_global_mcp_configs
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "No 'loom' entry" "$_out_missing_key" \
        "mcpServers key entirely absent: warns that the loom registration is missing"
    assert_contains "install-loom.sh" "$_out_missing_key" \
        "missing-registration warning names the install-loom.sh / loom update fix"
    assert_contains "RC=0" "$_out_missing_key" \
        "missing-registration check never aborts (warning-only contract preserved)"
    rm -rf "$_home_missing_key"

    # 11b. mcpServers present but no `loom` entry -> warns.
    _home_no_loom="$(mktemp -d)"
    cat >"$_home_no_loom/.claude.json" <<'JSON'
{ "mcpServers": { "other": { "command": "cat", "args": [] } } }
JSON
    _out_no_loom="$(HOME="$_home_no_loom" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_global_mcp_configs
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "No 'loom' entry" "$_out_no_loom" \
        "mcpServers present without a loom key: warns that the loom registration is missing"
    assert_contains "RC=0" "$_out_no_loom" \
        "mcpServers present without a loom key: check never aborts"
    rm -rf "$_home_no_loom"

    # 11c. mcpServers.loom present and healthy -> no presence warning.
    _home_healthy="$(mktemp -d)"
    cat >"$_home_healthy/.claude.json" <<JSON
{ "mcpServers": { "loom": { "command": "$(command -v cat)", "args": [] } } }
JSON
    _out_healthy="$(HOME="$_home_healthy" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_global_mcp_configs
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_not_contains "No 'loom' entry" "$_out_healthy" \
        "healthy loom entry present: no presence warning"
    assert_contains "RC=0" "$_out_healthy" \
        "healthy loom entry present: check succeeds"
    rm -rf "$_home_healthy"

    # 11d. ~/.claude.json missing entirely -> unaffected (early return preserved).
    _home_no_file="$(mktemp -d)"
    _out_no_file="$(HOME="$_home_no_file" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_global_mcp_configs
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_eq "RC=0" "$_out_no_file" \
        "user .claude.json missing entirely: no output, RC=0 (early-return regression check)"
    rm -rf "$_home_no_file"
else
    echo -e "  ${YELLOW}SKIP${NC}: check_global_mcp_configs presence tests (python3 not installed)"
fi

# ============================================================
# Section 12: check_workspace_trust preflight check (#5314)
#
# The function must warn — but never abort — when the current workspace's
# ~/.claude.json entry is missing or `hasTrustDialogAccepted` is not `true`,
# and stay silent when it is `true`. Mirrors Section 11's structure.
# ============================================================
echo ""
echo "Testing check_workspace_trust preflight check (#5314)..."

if $HAVE_PYTHON3; then
    _ws_dir="$(mktemp -d)"

    # 12a. ~/.claude.json missing entirely -> warns (with the fix command),
    # never aborts.
    _home_no_state="$(mktemp -d)"
    _out_no_state="$(HOME="$_home_no_state" LOOM_WORKSPACE="$_ws_dir" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_workspace_trust
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "not marked trusted" "$_out_no_state" \
        "user .claude.json missing entirely: warns that the workspace is not trusted"
    assert_contains "workspace add" "$_out_no_state" \
        "missing-state-file warning names the 'loom-daemon workspace add' fix"
    assert_contains "RC=0" "$_out_no_state" \
        "missing-state-file check never aborts (warning-only contract)"
    rm -rf "$_home_no_state"

    # 12b. ~/.claude.json present, but no entry for this workspace -> warns.
    _home_no_entry="$(mktemp -d)"
    cat >"$_home_no_entry/.claude.json" <<JSON
{ "projects": { "/some/other/workspace": { "hasTrustDialogAccepted": true } } }
JSON
    _out_no_entry="$(HOME="$_home_no_entry" LOOM_WORKSPACE="$_ws_dir" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_workspace_trust
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "is not trusted" "$_out_no_entry" \
        "workspace has no projects[] entry: warns that it is not trusted"
    assert_contains "RC=0" "$_out_no_entry" \
        "no-entry check never aborts"
    rm -rf "$_home_no_entry"

    # 12c. ~/.claude.json has this workspace with hasTrustDialogAccepted=false
    # -> warns.
    _home_false="$(mktemp -d)"
    cat >"$_home_false/.claude.json" <<JSON
{ "projects": { "$_ws_dir": { "hasTrustDialogAccepted": false } } }
JSON
    _out_false="$(HOME="$_home_false" LOOM_WORKSPACE="$_ws_dir" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_workspace_trust
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_contains "is not trusted" "$_out_false" \
        "workspace explicitly untrusted (hasTrustDialogAccepted=false): warns"
    assert_contains "RC=0" "$_out_false" \
        "explicitly-untrusted check never aborts"
    rm -rf "$_home_false"

    # 12d. ~/.claude.json has this workspace trusted -> no warning.
    _home_trusted="$(mktemp -d)"
    cat >"$_home_trusted/.claude.json" <<JSON
{ "projects": { "$_ws_dir": { "hasTrustDialogAccepted": true } } }
JSON
    _out_trusted="$(HOME="$_home_trusted" LOOM_WORKSPACE="$_ws_dir" CLAUDE_WRAPPER_SOURCE_ONLY=1 bash -c '
        source "$1"
        check_workspace_trust
        echo "RC=$?"
    ' _ "$WRAPPER" 2>&1)"
    assert_not_contains "is not trusted" "$_out_trusted" \
        "workspace trusted: no warning"
    assert_eq "RC=0" "$_out_trusted" \
        "workspace trusted: no output at all, RC=0"
    rm -rf "$_home_trusted"

    rm -rf "$_ws_dir"
else
    echo -e "  ${YELLOW}SKIP${NC}: check_workspace_trust tests (python3 not installed)"
fi

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
