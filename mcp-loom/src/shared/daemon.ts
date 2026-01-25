/**
 * Daemon socket communication for Loom MCP server
 *
 * Handles direct socket communication with the Loom daemon process.
 */

import { Socket } from "node:net";
import { SOCKET_PATH } from "./config.js";

/**
 * Send a request to the Loom daemon and get the response
 *
 * Uses Unix domain socket to communicate with the running daemon.
 * The daemon expects JSON-encoded requests and returns JSON-encoded responses.
 *
 * @param request - The request object to send
 * @returns The parsed response from the daemon
 * @throws Error if connection fails or response cannot be parsed
 */
export async function sendDaemonRequest(request: unknown): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const socket = new Socket();
    let buffer = "";

    socket.on("data", (data) => {
      buffer += data.toString();
    });

    socket.on("end", () => {
      try {
        const response = JSON.parse(buffer);
        resolve(response);
      } catch (error) {
        reject(new Error(`Failed to parse daemon response: ${error}`));
      }
    });

    socket.on("error", (error) => {
      reject(new Error(`Failed to connect to Loom daemon at ${SOCKET_PATH}: ${error.message}`));
    });

    socket.connect(SOCKET_PATH, () => {
      socket.write(JSON.stringify(request));
      socket.write("\n");
    });
  });
}
