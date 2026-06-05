/**
 * Daemon socket communication for Loom MCP server
 *
 * Handles direct socket communication with the Loom daemon process.
 */

import { Socket } from "node:net";
import { SOCKET_PATH } from "./config.js";

/**
 * Options for the streaming subscribe path (Issue #3455, Phase C).
 */
export interface StreamSubscribeOptions {
  /**
   * Optional duration after which the subscription closes automatically.
   * Used by `tail_event_bus --since <duration>` to cap the streaming
   * window. When undefined the connection stays open until the daemon
   * closes it or `abort()` is called.
   */
  durationMs?: number;
  /**
   * Optional per-line callback invoked once per JSON frame from the
   * daemon. Each frame is a `Response::EventStream { events: [...] }`
   * payload. Implementations should treat the JSON as opaque and let
   * the caller decode.
   */
  onLine?: (line: string) => void;
  /**
   * Maximum number of frames to collect before resolving. When set,
   * acts as an upper bound on the returned array.
   */
  maxLines?: number;
}

/**
 * Result of a streaming subscribe call.
 */
export interface StreamSubscribeResult {
  /** Line-delimited JSON frames the daemon emitted during the window. */
  lines: string[];
  /** True when the duration window elapsed; false when the daemon closed first. */
  closedByTimeout: boolean;
  /** Total time the connection stayed open (ms). */
  elapsedMs: number;
}

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

/**
 * Send a streaming request (e.g. `SubscribeEvents`) and collect frames
 * until the duration window elapses, the line cap is hit, or the daemon
 * closes the connection.
 *
 * The daemon writes line-delimited JSON frames; this helper buffers
 * partial reads and emits one entry per newline.
 *
 * Used by Phase C's `subscribe_to_events` and `tail_event_bus` tools.
 */
export async function sendDaemonStreamRequest(
  request: unknown,
  options: StreamSubscribeOptions = {}
): Promise<StreamSubscribeResult> {
  return new Promise((resolve, reject) => {
    const socket = new Socket();
    const lines: string[] = [];
    let buffer = "";
    let closedByTimeout = false;
    let timer: NodeJS.Timeout | null = null;
    const startedAt = Date.now();

    const finish = () => {
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      if (!socket.destroyed) {
        socket.end();
        socket.destroy();
      }
      resolve({
        lines,
        closedByTimeout,
        elapsedMs: Date.now() - startedAt,
      });
    };

    const consumeLine = (line: string) => {
      const trimmed = line.trim();
      if (trimmed.length === 0) return;
      lines.push(trimmed);
      if (options.onLine) {
        try {
          options.onLine(trimmed);
        } catch {
          // Swallow user-callback errors — they shouldn't tear down
          // the stream collection path.
        }
      }
      if (options.maxLines !== undefined && lines.length >= options.maxLines) {
        finish();
      }
    };

    socket.on("data", (data) => {
      buffer += data.toString();
      let nl = buffer.indexOf("\n");
      while (nl !== -1) {
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        consumeLine(line);
        nl = buffer.indexOf("\n");
      }
    });

    socket.on("end", () => {
      // Flush any trailing partial line.
      if (buffer.length > 0) {
        consumeLine(buffer);
        buffer = "";
      }
      finish();
    });

    socket.on("close", () => {
      finish();
    });

    socket.on("error", (error) => {
      if (timer) clearTimeout(timer);
      reject(new Error(`Failed to stream from Loom daemon at ${SOCKET_PATH}: ${error.message}`));
    });

    socket.connect(SOCKET_PATH, () => {
      socket.write(JSON.stringify(request));
      socket.write("\n");
      if (options.durationMs !== undefined && options.durationMs > 0) {
        timer = setTimeout(() => {
          closedByTimeout = true;
          finish();
        }, options.durationMs);
      }
    });
  });
}
