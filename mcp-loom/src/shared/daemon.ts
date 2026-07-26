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
 * Default bound (ms) for a unary daemon request when neither a per-call
 * override nor the `LOOM_DAEMON_IPC_TIMEOUT_MS` env override is supplied.
 *
 * Kept intentionally small (a couple of seconds) so a fire-and-forget ack
 * like `publish_event` fails fast instead of hanging until the outer ~1800s
 * MCP idle timeout aborts the whole tool call (issue #3945). Callers whose
 * request genuinely takes longer than an immediate ack — `dispatch_sweep`
 * (spawns a child under the registry mutex) and `cancel_sweep` (blocks for
 * the SIGTERM→grace→SIGKILL window) — pass an explicit wider `timeoutMs`
 * rather than raising this global default.
 */
export const DEFAULT_DAEMON_IPC_TIMEOUT_MS = 2000;

/**
 * Resolve the default unary-request timeout (ms).
 *
 * Reads the `LOOM_DAEMON_IPC_TIMEOUT_MS` env override — a positive integer
 * number of milliseconds — and falls back to {@link DEFAULT_DAEMON_IPC_TIMEOUT_MS}
 * for an absent, non-numeric, zero, or negative value.
 */
export function resolveDaemonIpcTimeoutMs(): number {
  const raw = process.env.LOOM_DAEMON_IPC_TIMEOUT_MS;
  if (raw !== undefined && raw.trim().length > 0) {
    const parsed = Number.parseInt(raw, 10);
    if (Number.isInteger(parsed) && parsed > 0) {
      return parsed;
    }
  }
  return DEFAULT_DAEMON_IPC_TIMEOUT_MS;
}

/**
 * Send a request to the Loom daemon and get the response
 *
 * Uses Unix domain socket to communicate with the running daemon.
 * The daemon expects JSON-encoded requests and returns JSON-encoded responses.
 *
 * A bounded timeout (issue #3945) guarantees this Promise always settles: if
 * the daemon accepts the connection but never writes a response frame and
 * never closes the socket (bridge/protocol skew, a blocked accept loop under
 * multi-repo load, or a request-variant mismatch), the timer fires and the
 * Promise rejects with a clear, greppable error instead of hanging until the
 * outer ~1800s MCP idle timeout. The reject surfaces through each caller's
 * existing try/catch as a returned `{ success: false, error }`, preserving the
 * sweep skill's soft-failure path (a dead bus never blocks a sweep).
 *
 * @param request - The request object to send
 * @param timeoutMs - Optional per-call timeout override (ms). When omitted,
 *   falls back to {@link resolveDaemonIpcTimeoutMs} (env override or the small
 *   default). A non-positive / non-finite value is ignored.
 * @returns The parsed response from the daemon
 * @throws Error if connection fails, the response cannot be parsed, or the
 *   daemon does not respond within the timeout.
 */
export async function sendDaemonRequest(
  request: unknown,
  timeoutMs?: number
): Promise<unknown> {
  const effectiveTimeout =
    typeof timeoutMs === "number" && Number.isFinite(timeoutMs) && timeoutMs > 0
      ? timeoutMs
      : resolveDaemonIpcTimeoutMs();
  const requestType =
    request && typeof request === "object" && "type" in request
      ? String((request as { type: unknown }).type)
      : "unknown";

  return new Promise((resolve, reject) => {
    const socket = new Socket();
    let buffer = "";
    let settled = false;
    let timer: NodeJS.Timeout | null = null;

    const clearTimer = () => {
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
    };

    // Guard against double-settle: a late `end`/`error` after a timeout
    // reject (or vice-versa) must not call resolve/reject twice.
    const settleResolve = (value: unknown) => {
      if (settled) return;
      settled = true;
      clearTimer();
      resolve(value);
    };
    const settleReject = (error: Error) => {
      if (settled) return;
      settled = true;
      clearTimer();
      if (!socket.destroyed) socket.destroy();
      reject(error);
    };

    socket.on("data", (data) => {
      buffer += data.toString();
    });

    socket.on("end", () => {
      try {
        const response = JSON.parse(buffer);
        settleResolve(response);
      } catch (error) {
        settleReject(new Error(`Failed to parse daemon response: ${error}`));
      }
    });

    socket.on("error", (error) => {
      settleReject(
        new Error(`Failed to connect to Loom daemon at ${SOCKET_PATH}: ${error.message}`)
      );
    });

    // Arm the timer up front so it bounds the entire operation — connect,
    // write, and the wait for the response frame.
    timer = setTimeout(() => {
      settleReject(
        new Error(
          `Loom daemon did not respond within ${effectiveTimeout}ms at ${SOCKET_PATH} ` +
            `(request type ${requestType}) — daemon may be unreachable or its MCP/IPC ` +
            `bridge is not answering`
        )
      );
    }, effectiveTimeout);

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
