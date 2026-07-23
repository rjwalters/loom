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
