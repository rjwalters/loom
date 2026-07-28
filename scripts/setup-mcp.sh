#!/usr/bin/env bash
# DEMOTED (#4230, epic #3835 Phase 3c): this is NO LONGER the primary MCP
# registration path, and it NO LONGER emits a `loom` server entry.
#
# The `loom` MCP server is now registered ONCE PER MACHINE at USER SCOPE
# (`claude mcp add --scope user loom -- node <checkout>/mcp-loom/dist/index.js`,
# run by scripts/install-loom.sh against the machine-level checkout). A single
# user-scoped instance serves every repo — it resolves the invoking repo from
# its CWD (see mcp-loom/src/shared/config.ts getWorkspacePath) — which kills the
# #3803 per-repo stale-dist drift class: there is exactly one served bundle,
# refreshed by `loom update`, instead of one built-and-pinned copy per repo.
#
# Claude Code MCP scope precedence is local > project > user, so a lingering
# PROJECT-scope `loom` entry in a repo's .mcp.json would SILENTLY outrank the
# user-scope server forever (a shadowing drift). This script therefore also
# STRIPS any pre-existing `loom` entry from the target's .mcp.json (migration),
# not just stops generating new ones.
#
# RESIDUAL ROLE — safehouse only: the `safehouse` server stays inherently
# per-repo/session (its socket + persona resolve from the target repo's own
# .loom/config.json, #3999; per-worker personas are injected at spawn time by
# spawn-claude.sh via --mcp-config, #3997). When safehouse is enabled+resolved
# this script emits a .mcp.json containing ONLY the `safehouse` entry.
#
# It still builds the mcp-loom bundle when missing/stale — that bundle is what
# the user-scoped server serves out of this checkout — so running this script
# keeps the served bundle fresh even though it no longer references it in a file.
#
# Usage:
#   ./scripts/setup-mcp.sh                          # Migrate this checkout's .mcp.json
#   ./scripts/setup-mcp.sh --target /path/to/consumer-repo
#                                                     # Operate on a consumer repository's
#                                                     # .mcp.json instead of this Loom
#                                                     # source checkout (safehouse config
#                                                     # resolution + LOOM_WORKSPACE pointed
#                                                     # at the consumer). --workspace is
#                                                     # accepted as an alias.
#
# `mcp-loom` itself only ever lives in the Loom SOURCE checkout (it is not
# installed into consumer repos). Only the OUTPUT location (where .mcp.json gets
# migrated/written) and safehouse config resolution (read from the target's own
# .loom/config.json) move to the target when --target/--workspace is given.
# See issues #4188 and #4230.

set -euo pipefail

TARGET_ARG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target|--workspace)
      if [[ $# -lt 2 ]]; then
        echo "Error: $1 requires a path argument" >&2
        exit 2
      fi
      TARGET_ARG="$2"
      shift 2
      ;;
    --target=*|--workspace=*)
      TARGET_ARG="${1#*=}"
      shift
      ;;
    -h|--help)
      sed -n '2,43p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown argument: $1 (supported: --target/--workspace <path>)" >&2
      exit 2
      ;;
  esac
done

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The Loom SOURCE checkout root (parent of scripts/) -- mcp-loom and the
# shared mcp-config.sh resolver lib always live here, regardless of --target.
LOOM_SOURCE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# OUTPUT_TARGET is where .mcp.json gets written, what LOOM_WORKSPACE gets set
# to, and which repo's .loom/config.json safehouse resolution reads from.
# Defaults to the Loom source checkout itself (unchanged, self-targeting
# behavior) unless --target/--workspace names a consumer repository.
if [[ -n "$TARGET_ARG" ]]; then
  # Expand tilde and resolve to an absolute path; must already exist.
  TARGET_ARG="${TARGET_ARG/#\~/$HOME}"
  if [[ ! -d "$TARGET_ARG" ]]; then
    echo "Error: --target/--workspace directory does not exist: $TARGET_ARG" >&2
    exit 2
  fi
  OUTPUT_TARGET="$(cd "$TARGET_ARG" && pwd)"
else
  OUTPUT_TARGET="$LOOM_SOURCE_ROOT"
fi

# Optional safehouse MCP server injection (#3999). Sourcing the shared resolver
# is best-effort: when the lib is absent (e.g. an older installed tree) or the
# safehouse block is disabled, generation falls through to the loom-only heredoc
# below, byte-for-byte identical to the pre-#3999 output. The lib itself always
# resolves from the source checkout (mirrors mcp-loom); the repo_root argument
# passed to its resolvers below is OUTPUT_TARGET, so safehouse settings are
# read from the target repo's own .loom/config.json.
MCP_CONFIG_LIB="$LOOM_SOURCE_ROOT/defaults/scripts/lib/mcp-config.sh"
# shellcheck source=../defaults/scripts/lib/mcp-config.sh
[[ -f "$MCP_CONFIG_LIB" ]] && source "$MCP_CONFIG_LIB"

MCP_DIR="$LOOM_SOURCE_ROOT/mcp-loom"
MCP_SRC="$MCP_DIR/src"
MCP_ENTRY="$MCP_DIR/dist/index.js"

# Decide whether the MCP server needs (re)building.
#   - missing artifact               -> build
#   - artifact older than any source -> rebuild (stale)
NEEDS_BUILD=0
BUILD_REASON=""
if [[ ! -f "$MCP_ENTRY" ]]; then
  NEEDS_BUILD=1
  BUILD_REASON="MCP server not built"
elif [[ -d "$MCP_SRC" ]] && [[ -n "$(find "$MCP_SRC" -type f -newer "$MCP_ENTRY" -print -quit 2>/dev/null)" ]]; then
  # At least one source file is newer than the built bundle.
  NEEDS_BUILD=1
  BUILD_REASON="MCP server bundle is stale (src newer than dist)"
fi

if [[ "$NEEDS_BUILD" -eq 1 ]]; then
  echo "$BUILD_REASON, building mcp-loom..."
  if command -v node &> /dev/null; then
    (cd "$MCP_DIR" && npm install --silent && npm run build) || {
      echo "Warning: Failed to build mcp-loom. MCP tools will not be available." >&2
      echo "  Run manually: cd mcp-loom && npm install && npm run build" >&2
      exit 1
    }
    echo "MCP server built successfully"
  else
    echo "Warning: node not found. Cannot build mcp-loom." >&2
    echo "  Install Node.js and run: cd mcp-loom && npm install && npm run build" >&2
    exit 1
  fi
fi

# Resolve the optional safehouse server (#3999). When the block is enabled and
# a socket + launch command resolve, emit a second `safehouse` server AFTER the
# unchanged `loom` entry. This workspace-root file uses the scalar
# `safehouse.persona` (workspace-wide); per-worker personas are injected at
# spawn time by spawn-claude.sh via --mcp-config. When disabled/unresolved, the
# loom-only heredoc below is byte-for-byte identical to the pre-#3999 output.
SAFEHOUSE_ENABLED="false"
SH_SOCKET=""
SH_PERSONA=""
SH_COMMAND=""
if declare -F loom_mcp_safehouse_enabled >/dev/null 2>&1; then
  SAFEHOUSE_ENABLED="$(loom_mcp_safehouse_enabled "$OUTPUT_TARGET")"
fi
if [[ "$SAFEHOUSE_ENABLED" == "true" ]]; then
  SH_SOCKET="$(loom_mcp_safehouse_socket "$OUTPUT_TARGET")"
  SH_PERSONA="$(loom_mcp_safehouse_persona_fallback "$OUTPUT_TARGET")"
  SH_COMMAND="$(loom_mcp_safehouse_command "$OUTPUT_TARGET")"
  if [[ -z "$SH_SOCKET" ]]; then
    echo "Warning: safehouse enabled but no socket resolves (safehouse.socket / \$LOOM_SAFEHOUSE_SOCKET / \$SAFEHOUSED_SOCKET); omitting safehouse server." >&2
    SH_SOCKET=""
  elif ! command -v "$SH_COMMAND" >/dev/null 2>&1; then
    echo "Warning: safehouse launch command '$SH_COMMAND' not found in PATH; omitting safehouse server (loom MCP unaffected)." >&2
    SH_SOCKET=""
  fi
fi

TARGET_MCP="$OUTPUT_TARGET/.mcp.json"

# Strip any pre-existing `loom` server entry from an existing .mcp.json, in
# place (#4230 shadowing migration). Prints one of:
#   skip         -> file absent or unparseable (nothing done)
#   noop         -> file present but had no `loom` entry
#   stripped     -> `loom` entry removed; other servers preserved
#   removed-file -> `loom` was the only server; the file was deleted
strip_loom_entry() {
  local file="$1"
  [[ -f "$file" ]] || { echo "skip"; return 0; }
  python3 - "$file" <<'PYEOF'
import json, os, sys
path = sys.argv[1]
try:
    with open(path) as f:
        cfg = json.load(f)
except Exception:
    print("skip"); sys.exit(0)
servers = cfg.get("mcpServers", {})
if "loom" not in servers:
    print("noop"); sys.exit(0)
del servers["loom"]
cfg["mcpServers"] = servers
if not servers:
    os.remove(path)
    print("removed-file")
else:
    with open(path, "w") as f:
        json.dump(cfg, f, indent=2)
        f.write("\n")
    print("stripped")
PYEOF
}

if [[ "$SAFEHOUSE_ENABLED" == "true" && -n "$SH_SOCKET" ]]; then
  # Residual role: emit a .mcp.json with ONLY the per-repo `safehouse` server.
  # The `loom` entry is DELIBERATELY omitted — it is owned by the user-scoped
  # registration now (#4230). Only the socket path is credential-adjacent.
  cat > "$TARGET_MCP" <<EOF
{
  "mcpServers": {
    "safehouse": {
      "command": "$SH_COMMAND",
      "args": [],
      "env": {
        "SAFEHOUSED_SOCKET": "$SH_SOCKET",
        "SAFEHOUSE_PERSONA": "$SH_PERSONA"
      }
    }
  }
}
EOF
  echo "Generated .mcp.json with safehouse MCP server only (loom is now user-scoped — see below)."
  echo "  Safehouse persona: $SH_PERSONA (socket: $SH_SOCKET)"
else
  # No safehouse server to emit. Migrate away any stale project-scope `loom`
  # entry so it can no longer shadow the user-scope server (#4230).
  migrate_result="$(strip_loom_entry "$TARGET_MCP")"
  case "$migrate_result" in
    stripped)
      echo "Removed a stale project-scope 'loom' entry from $TARGET_MCP" >&2
      echo "  (a project entry outranks user scope — it would have shadowed the machine server; see #4230)." >&2
      ;;
    removed-file)
      echo "Removed $TARGET_MCP (its only server was the now-user-scoped 'loom' entry; see #4230)." >&2
      ;;
    *)
      : # skip/noop — nothing to migrate
      ;;
  esac
fi

echo ""
echo "NOTE: setup-mcp.sh is DEMOTED (#4230). The 'loom' MCP server is now"
echo "      registered ONCE PER MACHINE at user scope. If it is not registered,"
echo "      run (against the machine-level checkout):"
echo "        claude mcp add --scope user loom -- node \"$LOOM_SOURCE_ROOT/mcp-loom/dist/index.js\""
echo "      'loom update' refreshes the served bundle for every repo at once."
