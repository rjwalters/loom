/**
 * Loom MCP Server - Unified Model Context Protocol server for Loom
 *
 * Consolidates three previously separate MCP servers:
 * - mcp-loom-logs: Log monitoring tools
 * - mcp-loom-ui: UI control and state management tools
 * - mcp-loom-terminals: Terminal management tools
 *
 * All tools are now available through a single MCP server instance.
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

import { handleLogTool, logTools } from "./tools/logs.js";
import { handleTerminalTool, terminalTools } from "./tools/terminals.js";
import { handleUITool, uiTools } from "./tools/ui.js";

// Combine all tools from all modules
const allTools = [...logTools, ...uiTools, ...terminalTools];

// Create the unified MCP server
const server = new Server(
  {
    name: "loom",
    version: "0.3.0",
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

// Register tool list handler
server.setRequestHandler(ListToolsRequestSchema, async () => {
  return { tools: allTools };
});

// Register tool call handler
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    // Determine which module handles this tool
    const logToolNames = logTools.map((t) => t.name);
    const uiToolNames = uiTools.map((t) => t.name);
    const terminalToolNames = terminalTools.map((t) => t.name);

    let content: { type: "text"; text: string }[];

    if (logToolNames.includes(name)) {
      content = await handleLogTool(name, args as Record<string, unknown>);
    } else if (uiToolNames.includes(name)) {
      content = await handleUITool(name, args as Record<string, unknown>);
    } else if (terminalToolNames.includes(name)) {
      content = await handleTerminalTool(name, args as Record<string, unknown>);
    } else {
      return {
        content: [
          {
            type: "text",
            text: `Unknown tool: ${name}`,
          },
        ],
        isError: true,
      };
    }

    return { content };
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    return {
      content: [
        {
          type: "text",
          text: `Error: ${errorMessage}`,
        },
      ],
      isError: true,
    };
  }
});

// Start the server
async function main() {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("Loom MCP server running on stdio (unified: logs + ui + terminals)");
}

main().catch((error) => {
  console.error("Fatal error in main():", error);
  process.exit(1);
});
