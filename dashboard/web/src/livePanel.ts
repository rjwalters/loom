/**
 * The `#/live` tab's controller (issue #9077): owns the three clocks that keep
 * the status board moving, and releases all of them on teardown.
 *
 * 1. **Snapshot poll** — `/api/fleet-state` (or `/public/fleet-state`) every
 *    `pollIntervalMs`. Faster than the Fleet tab's 10s, because this tab is
 *    for watching.
 * 2. **Live tail** — the SSE stream. Each frame goes on the ticker at once,
 *    and a `sweep.*` frame also pulls the next snapshot forward (debounced), so
 *    a card appears, advances or leaves within about a second of the event
 *    rather than on the next poll.
 * 3. **Tick** — once a second, advancing timers without touching the rest of
 *    the DOM (see `views/liveBoard.ts`).
 *
 * Like `app.ts`, every clock and the network are injected, so tests run with
 * no real timers or sockets.
 */

import { fetchFleetState } from "./api";
import { buildFleetView } from "./fleet";
import { LiveFeedClient, type ConnectionState } from "./sseFeedClient";
import type { FleetSnapshot, LiveTailFrame } from "./types";
import { LiveBoard } from "./views/liveBoard";

export const LIVE_POLL_INTERVAL_MS = 5_000;
export const LIVE_TICK_MS = 1_000;
/** How long to wait after a `sweep.*` event before refetching. Events tend to
 * arrive in bursts (a phase change, then an outcome), so one fetch covers the
 * burst. */
export const EVENT_REFRESH_DEBOUNCE_MS = 800;

export interface Timers {
  setTimeout: (handler: () => void, ms: number) => number;
  clearTimeout: (handle: number) => void;
  setInterval: (handler: () => void, ms: number) => number;
  clearInterval: (handle: number) => void;
}

/** The part of `LiveFeedClient` the panel uses — injectable for tests. */
export interface FeedHandle {
  start(): void;
  stop(): void;
}

export interface LivePanelOptions {
  container: HTMLElement;
  /** SSE endpoint: `/api/events` or `/public/events`. */
  feedUrl: string;
  fetchState?: () => Promise<FleetSnapshot>;
  now?: () => Date;
  pollIntervalMs?: number;
  tickMs?: number;
  timers?: Timers;
  createFeed?: (handlers: {
    onEvent: (frame: LiveTailFrame) => void;
    onConnectionChange: (state: ConnectionState) => void;
  }) => FeedHandle;
}

const browserTimers: Timers = {
  setTimeout: (handler, ms) => globalThis.setTimeout(handler, ms) as unknown as number,
  clearTimeout: (handle) => globalThis.clearTimeout(handle),
  setInterval: (handler, ms) => globalThis.setInterval(handler, ms) as unknown as number,
  clearInterval: (handle) => globalThis.clearInterval(handle),
};

export class LivePanel {
  readonly board: LiveBoard;
  private readonly container: HTMLElement;
  private readonly fetchState: () => Promise<FleetSnapshot>;
  private readonly now: () => Date;
  private readonly pollIntervalMs: number;
  private readonly tickMs: number;
  private readonly timers: Timers;
  private readonly feed: FeedHandle;

  private pollTimer: number | null = null;
  private tickTimer: number | null = null;
  private inFlight = false;
  /** A refresh was requested while one was in flight; run it after. */
  private pending = false;
  private stopped = true;

  constructor(options: LivePanelOptions) {
    this.container = options.container;
    this.fetchState = options.fetchState ?? (() => fetchFleetState());
    this.now = options.now ?? (() => new Date());
    this.pollIntervalMs = options.pollIntervalMs ?? LIVE_POLL_INTERVAL_MS;
    this.tickMs = options.tickMs ?? LIVE_TICK_MS;
    this.timers = options.timers ?? browserTimers;
    this.board = new LiveBoard((handler, ms) => void this.timers.setTimeout(handler, ms));

    const handlers = {
      onEvent: (frame: LiveTailFrame) => this.handleEvent(frame),
      onConnectionChange: (state: ConnectionState) => this.board.setConnection(state),
    };
    this.feed = options.createFeed
      ? options.createFeed(handlers)
      : new LiveFeedClient({ url: options.feedUrl, ...handlers });
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    this.container.replaceChildren(this.board.root);
    this.board.tick(this.now());
    this.tickTimer = this.timers.setInterval(() => this.board.tick(this.now()), this.tickMs);
    this.feed.start();
    void this.refresh();
  }

  stop(): void {
    this.stopped = true;
    this.feed.stop();
    if (this.tickTimer !== null) this.timers.clearInterval(this.tickTimer);
    if (this.pollTimer !== null) this.timers.clearTimeout(this.pollTimer);
    this.tickTimer = null;
    this.pollTimer = null;
  }

  async refresh(): Promise<void> {
    if (this.stopped) return;
    if (this.inFlight) {
      this.pending = true;
      return;
    }
    this.inFlight = true;
    try {
      const snapshot = await this.fetchState();
      if (this.stopped) return;
      const now = this.now();
      this.board.update(buildFleetView(snapshot, now), now);
      this.board.setError(null);
    } catch (caught) {
      if (this.stopped) return;
      this.board.setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      this.inFlight = false;
    }
    if (this.pending) {
      this.pending = false;
      void this.refresh();
      return;
    }
    this.schedule(this.pollIntervalMs);
  }

  private schedule(ms: number): void {
    if (this.stopped || this.pollIntervalMs <= 0) return;
    if (this.pollTimer !== null) this.timers.clearTimeout(this.pollTimer);
    this.pollTimer = this.timers.setTimeout(() => {
      this.pollTimer = null;
      void this.refresh();
    }, ms);
  }

  private handleEvent(frame: LiveTailFrame): void {
    if (this.stopped) return;
    this.board.pushEvent(frame, this.now());
    if (!frame.topic.startsWith("sweep.")) return;
    // A fetch already in flight may have left before this event; queue one
    // more rather than letting its completion push the next poll out.
    if (this.inFlight) this.pending = true;
    else this.schedule(EVENT_REFRESH_DEBOUNCE_MS);
  }
}
