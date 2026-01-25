/**
 * File-based IPC utilities for Loom MCP server
 *
 * Implements a file-based command/acknowledgment pattern for
 * communication with the Loom UI application.
 */

import { mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { LOOM_DIR, MCP_ACK_FILE, MCP_COMMAND_FILE } from "./config.js";

/**
 * Write MCP command to control file for Loom to pick up with retry and exponential backoff
 *
 * This is a file-based IPC mechanism with acknowledgment. The process:
 * 1. Write command to MCP_COMMAND_FILE
 * 2. Wait for acknowledgment in MCP_ACK_FILE
 * 3. Retry with exponential backoff until ack or timeout
 *
 * @param command - The command string to send
 * @returns Status message about command processing
 */
export async function writeMCPCommand(command: string): Promise<string> {
  // Ensure .loom directory exists
  try {
    await mkdir(LOOM_DIR, { recursive: true });
  } catch (_error) {
    // Directory might already exist, that's fine
  }

  // Clean up old acknowledgment file before writing new command
  try {
    await rm(MCP_ACK_FILE);
  } catch (_error) {
    // Ack file might not exist, that's fine
  }

  // Write command with timestamp
  const commandData = {
    command,
    timestamp: new Date().toISOString(),
  };

  await writeFile(MCP_COMMAND_FILE, JSON.stringify(commandData, null, 2));

  // Retry with exponential backoff to wait for acknowledgment
  const maxRetries = 8; // Max 8 retries
  const baseDelay = 100; // Start with 100ms
  const maxDelay = 5000; // Cap at 5 seconds
  let totalWaitTime = 0;

  for (let attempt = 0; attempt < maxRetries; attempt++) {
    // Calculate exponential backoff delay: 100ms, 200ms, 400ms, 800ms, 1600ms, 3200ms, 5000ms, 5000ms
    const delay = Math.min(baseDelay * 2 ** attempt, maxDelay);
    totalWaitTime += delay;

    // Wait before checking for acknowledgment
    await new Promise((resolve) => setTimeout(resolve, delay));

    // Check if acknowledgment file exists
    try {
      await stat(MCP_ACK_FILE);

      // Read acknowledgment data
      const ackContent = await readFile(MCP_ACK_FILE, "utf-8");
      const ackData = JSON.parse(ackContent);

      // Verify the ack is for our command
      if (ackData.command === command && ackData.timestamp === commandData.timestamp) {
        // Clean up acknowledgment file
        try {
          await rm(MCP_ACK_FILE);
        } catch (_error) {
          // Ignore cleanup errors
        }

        if (ackData.success) {
          return `MCP command '${command}' processed successfully (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        } else {
          return `MCP command '${command}' acknowledged but execution failed (waited ${totalWaitTime}ms, attempt ${attempt + 1}/${maxRetries})`;
        }
      }
    } catch (_error) {
      // Ack file doesn't exist yet or couldn't be read, continue retrying
    }
  }

  // Max retries exceeded - give up but don't error
  return `MCP command '${command}' written but no acknowledgment received after ${maxRetries} retries (${totalWaitTime}ms total). The command may still be processing.`;
}
