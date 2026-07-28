#!/usr/bin/env bash
# Generate .mcp.json with current workspace path
# Builds the unified MCP server if dist/index.js is missing OR stale
# (older than any TypeScript source under mcp-loom/src/). The staleness
# check prevents the built bundle from silently drifting behind source —
# e.g. new sweep-dispatch tools added to src/tools/sweeps.ts never showing
# up in dist/index.js because the artifact merely *exists* (see #3803, same
# failure shape as the installed-copy drift fixed in #3777).
#
# Usage:
#   ./scripts/setup-mcp.sh                          # Generate .mcp.json in this checkout
#   ./scripts/setup-mcp.sh --target /path/to/consumer-repo
#                                                     # Write .mcp.json into a consumer
#                                                     # repository instead of this Loom
#                                                     # source checkout, with LOOM_WORKSPACE
#                                                     # (and safehouse config resolution)
#                                                     # pointed at the consumer. --workspace
#                                                     # is accepted as an alias.
#
# `mcp-loom` itself only ever lives in the Loom SOURCE checkout (it is not
# installed into consumer repos), so the generated `loom` server entry always
# points at this checkout's mcp-loom/dist/index.js regardless of --target.
# Only the OUTPUT location (where .mcp.json gets written), LOOM_WORKSPACE (the
# env var mcp-loom uses to find the repo it operates on), and safehouse config
# resolution (read from the target's own .loom/config.json) move to the target
# when --target/--workspace is given. See issue #4188.

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
      sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
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

if [[ "$SAFEHOUSE_ENABLED" == "true" && -n "$SH_SOCKET" ]]; then
  # Two-server config. `loom` stays FIRST (claude-wrapper.sh's pre-flight keys
  # off the first server with args) and byte-identical to the loom-only block;
  # `safehouse` is appended second. Only the socket path is credential-adjacent.
  cat > "$OUTPUT_TARGET/.mcp.json" <<EOF
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$LOOM_SOURCE_ROOT/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$OUTPUT_TARGET"
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
  echo "  Workspace: $OUTPUT_TARGET"
  echo "  Safehouse persona: $SH_PERSONA (socket: $SH_SOCKET)"
else
  # Generate .mcp.json with unified loom server
  cat > "$OUTPUT_TARGET/.mcp.json" <<EOF
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["$LOOM_SOURCE_ROOT/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "$OUTPUT_TARGET"
      }
    }
  }
}
EOF

  echo "Generated .mcp.json with unified loom MCP server"
  echo "  Workspace: $OUTPUT_TARGET"
  echo "  Server: mcp-loom/dist/index.js"
fi
