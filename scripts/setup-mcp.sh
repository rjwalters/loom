#!/usr/bin/env bash
# Generate .mcp.json with current workspace path

set -euo pipefail

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Get the workspace root (parent of scripts/)
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Generate .mcp.json
cat > "$WORKSPACE_ROOT/.mcp.json" <<EOF
{
  "mcpServers": {
    "loom-logs": {
      "command": "node",
      "args": ["$WORKSPACE_ROOT/mcp-loom-logs/dist/index.js"]
    },
    "loom-terminals": {
      "command": "node",
      "args": ["$WORKSPACE_ROOT/mcp-loom-terminals/dist/index.js"]
    },
    "loom-ui": {
      "command": "node",
      "args": ["$WORKSPACE_ROOT/mcp-loom-ui/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$WORKSPACE_ROOT"
      }
    }
  }
}
EOF

echo "✓ Generated .mcp.json with workspace: $WORKSPACE_ROOT"
