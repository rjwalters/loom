#!/usr/bin/env bash
# Generate .mcp.json with current workspace path
# Builds the unified MCP server if dist/index.js is missing OR stale
# (older than any TypeScript source under mcp-loom/src/). The staleness
# check prevents the built bundle from silently drifting behind source —
# e.g. new sweep-dispatch tools added to src/tools/sweeps.ts never showing
# up in dist/index.js because the artifact merely *exists* (see #3803, same
# failure shape as the installed-copy drift fixed in #3777).

set -euo pipefail

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Get the workspace root (parent of scripts/)
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Optional safehouse MCP server injection (#3999). Sourcing the shared resolver
# is best-effort: when the lib is absent (e.g. an older installed tree) or the
# safehouse block is disabled, generation falls through to the loom-only heredoc
# below, byte-for-byte identical to the pre-#3999 output.
MCP_CONFIG_LIB="$WORKSPACE_ROOT/defaults/scripts/lib/mcp-config.sh"
# shellcheck source=../defaults/scripts/lib/mcp-config.sh
[[ -f "$MCP_CONFIG_LIB" ]] && source "$MCP_CONFIG_LIB"

MCP_DIR="$WORKSPACE_ROOT/mcp-loom"
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
  SAFEHOUSE_ENABLED="$(loom_mcp_safehouse_enabled "$WORKSPACE_ROOT")"
fi
if [[ "$SAFEHOUSE_ENABLED" == "true" ]]; then
  SH_SOCKET="$(loom_mcp_safehouse_socket "$WORKSPACE_ROOT")"
  SH_PERSONA="$(loom_mcp_safehouse_persona_fallback "$WORKSPACE_ROOT")"
  SH_COMMAND="$(loom_mcp_safehouse_command "$WORKSPACE_ROOT")"
  if [[ -z "$SH_SOCKET" ]]; then
    echo "Warning: safehouse enabled but no socket resolves (safehouse.socket / \$LOOM_SAFEHOUSE_SOCKET / \$SAFEHOUSED_SOCKET); omitting safehouse server." >&2
    SH_SOCKET=""
  elif ! command -v "$SH_COMMAND" >/dev/null 2>&1; then
    echo "Warning: safehouse launch command '$SH_COMMAND' not found in PATH; omitting safehouse server (loom MCP unaffected)." >&2
    SH_SOCKET=""
  fi
fi

if [[ "$SAFEHOUSE_ENABLED" == "true" && -n "$SH_SOCKET" ]]; then
  # Two-server config. `loom` stays FIRST (claude-wrapper.sh's pre-flight keys
  # off the first server with args) and byte-identical to the loom-only block;
  # `safehouse` is appended second. Only the socket path is credential-adjacent.
  cat > "$WORKSPACE_ROOT/.mcp.json" <<EOF
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$WORKSPACE_ROOT/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$WORKSPACE_ROOT"
      }
    },
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
  echo "Generated .mcp.json with loom + safehouse MCP servers"
  echo "  Workspace: $WORKSPACE_ROOT"
  echo "  Safehouse persona: $SH_PERSONA (socket: $SH_SOCKET)"
else
  # Generate .mcp.json with unified loom server
  cat > "$WORKSPACE_ROOT/.mcp.json" <<EOF
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$WORKSPACE_ROOT/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$WORKSPACE_ROOT"
      }
    }
  }
}
EOF

  echo "Generated .mcp.json with unified loom MCP server"
  echo "  Workspace: $WORKSPACE_ROOT"
  echo "  Server: mcp-loom/dist/index.js"
fi
