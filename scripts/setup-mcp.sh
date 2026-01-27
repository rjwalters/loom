#!/usr/bin/env bash
# Generate .mcp.json with current workspace path
# Builds the unified MCP server if dist/index.js is missing

set -euo pipefail

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Get the workspace root (parent of scripts/)
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MCP_DIR="$WORKSPACE_ROOT/mcp-loom"
MCP_ENTRY="$MCP_DIR/dist/index.js"

# Build the unified MCP server if not already built
if [[ ! -f "$MCP_ENTRY" ]]; then
  echo "MCP server not built, building mcp-loom..."
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
