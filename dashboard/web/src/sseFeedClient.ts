/**
 * SSE frame parsing + reconnecting client for the live event feed
 * (`GET /api/events`, see `dashboard/docs/query-api.md`).
 *
 * Two layers, deliberately separated so the wire-level parsing is testable
 * without any network/stream plumbing:
 *
 * - `SseStreamParser` — a pure, incremental parser: feed it raw text chunks
 *   (as they arrive from a `ReadableStream`), get back structured
 *   `SseFrame`s (`retry` / `comment` / `data`). Handles partial frames split
 *   across chunk boundaries.
 * - `LiveFeedClient` — owns the reconnect loop (honoring the server's
 *   `retry: 3000` preamble), skips `: keepalive` comments, and dedups
 *   `data:` frames across reconnects so a browser tab never renders the same
 *   telemetry record twice.
 */

import type { LiveTailFrame } from "./types.js";

export type SseFrame =
  | { type: "retry"; retryMs: number }
  | { type: "comment"; text: string }
  | { type: "data"; payload: string };

/**
 * Splits one blank-line-terminated SSE block into zero or more frames.
 * Unknown fields (`event:`, `id:`, ...) are ignored — this API's frames
 * only ever use `data:`, `retry:`, and bare comment lines (`:...`).
 */
function parseBlock(block: string): SseFrame[] {
  const frames: SseFrame[] = [];
  const dataLines: string[] = [];

  for (const rawLine of block.split("\n")) {
    if (rawLine === "") continue;

    if (rawLine.startsWith(":")) {
      frames.push({ type: "comment", text: rawLine.slice(1).trimStart() });
      continue;
    }

    const colonIdx = rawLine.indexOf(":");
    const field = colonIdx === -1 ? rawLine : rawLine.slice(0, colonIdx);
    let value = colonIdx === -1 ? "" : rawLine.slice(colonIdx + 1);
    if (value.startsWith(" ")) value = value.slice(1);

    if (field === "retry") {
      const ms = Number.parseInt(value, 10);
      if (!Number.isNaN(ms)) frames.push({ type: "retry", retryMs: ms });
    } else if (field === "data") {
      dataLines.push(value);
    }
    // event:/id:/anything else: not used by this API, ignored.
  }

  if (dataLines.length > 0) {
    frames.push({ type: "data", payload: dataLines.join("\n") });
  }

  return frames;
}

/**
 * Incremental SSE parser. Feed it text chunks in arrival order; it buffers
 * any trailing partial block (split across `read()` boundaries) until the
 * terminating blank line arrives.
 */
export class SseStreamParser {
  private buffer = "";

  /** Parse a newly-arrived chunk, returning any complete frames it produced. */
  feed(chunk: string): SseFrame[] {
    // Normalize CRLF up front so a chunk boundary landing inside a "\r\n"
    // pair is handled correctly (normalization runs over the full buffer,
    // not just the new chunk).
    this.buffer = (this.buffer + chunk).replace(/\r\n/g, "\n");

    const frames: SseFrame[] = [];
    let sepIndex: number;
    while ((sepIndex = this.buffer.indexOf("\n\n")) !== -1) {
      const block = this.buffer.slice(0, sepIndex);
      this.buffer = this.buffer.slice(sepIndex + 2);
      frames.push(...parseBlock(block));
    }
    return frames;
  }
}

/** Stable identity for a live-tail frame, used for reconnect-safe dedup. */
export function frameKey(frame: LiveTailFrame): string {
  const record = frame.event.record as Record<string, unknown>;
  const sweepId = typeof record.sweep_id === "string" ? record.sweep_id : "";
  return `${frame.event.hostId}|${frame.event.emittedAt}|${frame.topic}|${sweepId}`;
}

export type ConnectionState = "connecting" | "open" | "closed";

export interface LiveFeedClientOptions {
  /** Base URL for the live tail endpoint, e.g. "/api/events" or a full origin. */
  url: string;
  /** Scope the stream to a single host (matches the API's `host` param). */
  host?: string;
  /** Scope the stream to a single repo (matches the API's `repo` param). */
  repo?: string;
  onEvent: (frame: LiveTailFrame) => void;
  onConnectionChange?: (state: ConnectionState) => void;
  onParseError?: (payload: string, error: unknown) => void;
  /** Reconnect delay used before any `retry:` directive has been seen. */
  defaultRetryMs?: number;
  /** How many recently-seen frame keys to retain for cross-reconnect dedup. */
  dedupWindow?: number;
  /**
   * Opens a connection and returns a byte reader. Defaults to
   * `fetch(url).body.getReader()`; injectable for tests and for
   * environments without a global `fetch`.
   */
  connect?: (url: string, signal: AbortSignal) => Promise<ReadableStreamDefaultReader<Uint8Array>>;
  /** Injectable sleep, so tests don't wait on real reconnect timers. */
  sleep?: (ms: number) => Promise<void>;
}

function buildUrl(base: string, host: string | undefined, repo: string | undefined): string {
  const [path, existingQuery] = base.split("?", 2);
  const params = new URLSearchParams(existingQuery ?? "");
  if (host) params.set("host", host);
  if (repo) params.set("repo", repo);
  const query = params.toString();
  return query ? `${path}?${query}` : path!;
}

async function defaultConnect(
  url: string,
  signal: AbortSignal,
): Promise<ReadableStreamDefaultReader<Uint8Array>> {
  const response = await fetch(url, { signal, headers: { accept: "text/event-stream" } });
  if (!response.ok || !response.body) {
    throw new Error(`live feed connect failed: ${response.status}`);
  }
  return response.body.getReader();
}

const defaultSleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Reconnecting SSE client for `GET /api/events`. Call `start()` once; it
 * runs its own connect/read/reconnect loop until `stop()` is called.
 */
export class LiveFeedClient {
  private readonly options: Required<
    Pick<LiveFeedClientOptions, "url" | "onEvent" | "defaultRetryMs" | "dedupWindow" | "connect" | "sleep">
  > &
    LiveFeedClientOptions;
  private stopped = true;
  private abortController: AbortController | null = null;
  private retryMs: number;
  private readonly seenKeys = new Set<string>();
  private readonly seenOrder: string[] = [];
  private runPromise: Promise<void> | null = null;

  constructor(options: LiveFeedClientOptions) {
    this.options = {
      defaultRetryMs: 3000,
      dedupWindow: 500,
      connect: defaultConnect,
      sleep: defaultSleep,
      ...options,
    };
    this.retryMs = this.options.defaultRetryMs;
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    this.runPromise = this.runLoop();
  }

  stop(): void {
    this.stopped = true;
    this.abortController?.abort();
    this.options.onConnectionChange?.("closed");
  }

  /** Awaits the current run loop — mainly for tests to know a cycle settled. */
  async whenIdle(): Promise<void> {
    await this.runPromise;
  }

  private rememberKey(key: string): boolean {
    if (this.seenKeys.has(key)) return false;
    this.seenKeys.add(key);
    this.seenOrder.push(key);
    if (this.seenOrder.length > this.options.dedupWindow) {
      const evicted = this.seenOrder.shift();
      if (evicted !== undefined) this.seenKeys.delete(evicted);
    }
    return true;
  }

  private async runLoop(): Promise<void> {
    const url = buildUrl(this.options.url, this.options.host, this.options.repo);

    while (!this.stopped) {
      this.options.onConnectionChange?.("connecting");
      this.abortController = new AbortController();
      const parser = new SseStreamParser();
      const decoder = new TextDecoder();

      try {
        const reader = await this.options.connect(url, this.abortController.signal);
        this.options.onConnectionChange?.("open");

        while (!this.stopped) {
          const { value, done } = await reader.read();
          if (done) break;

          const chunk = typeof value === "string" ? value : decoder.decode(value, { stream: true });
          for (const frame of parser.feed(chunk)) {
            this.handleFrame(frame);
          }
        }
      } catch (err) {
        if (this.stopped) break;
        // Connection error — fall through to the reconnect delay below.
        void err;
      }

      if (this.stopped) break;
      this.options.onConnectionChange?.("closed");
      await this.options.sleep(this.retryMs);
    }
  }

  private handleFrame(frame: SseFrame): void {
    if (frame.type === "retry") {
      this.retryMs = frame.retryMs;
      return;
    }
    if (frame.type === "comment") {
      // Includes `: keepalive` and the connect preamble — never data.
      return;
    }

    // frame.type === "data"
    let parsed: LiveTailFrame;
    try {
      parsed = JSON.parse(frame.payload) as LiveTailFrame;
    } catch (err) {
      this.options.onParseError?.(frame.payload, err);
      return;
    }

    const key = frameKey(parsed);
    if (!this.rememberKey(key)) return; // duplicate across a reconnect — drop it
    this.options.onEvent(parsed);
  }
}
