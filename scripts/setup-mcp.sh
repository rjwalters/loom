#!/usr/bin/env bash
# Generate .mcp.json with current workspace path

set -euo pipefail

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Get the workspace root (parent of scripts/)
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

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
