/**
 * The `#/live` controller (issue #9077): three clocks (poll, tick, SSE) that
 * must all run while mounted and all stop on teardown.
 */

import { describe, expect, it, vi } from "vitest";

import { EVENT_REFRESH_DEBOUNCE_MS, LivePanel, type FeedHandle, type Timers } from "../src/livePanel";
import type { ConnectionState } from "../src/sseFeedClient";
import type { FleetSnapshot, LiveTailFrame } from "../src/types";

const NOW = new Date("2026-09-26T12:00:00Z");

function fakeTimers(): Timers & {
  timeouts: Map<number, { handler: () => void; ms: number }>;
  intervals: Map<number, { handler: () => void; ms: number }>;
  runTimeouts: () => void;
} {
  let next = 1;
  const timeouts = new Map<number, { handler: () => void; ms: number }>();
  const intervals = new Map<number, { handler: () => void; ms: number }>();
  return {
    timeouts,
    intervals,
    setTimeout: (handler, ms) => {
      timeouts.set(next, { handler, ms });
      return next++;
    },
    clearTimeout: (handle) => void timeouts.delete(handle),
    setInterval: (handler, ms) => {
      intervals.set(next, { handler, ms });
      return next++;
    },
    clearInterval: (handle) => void intervals.delete(handle),
    runTimeouts: () => {
      const due = [...timeouts.entries()];
      timeouts.clear();
      for (const [, { handler }] of due) handler();
    },
  };
}

interface FakeFeed extends FeedHandle {
  started: boolean;
  stopped: boolean;
  emit: (frame: LiveTailFrame) => void;
  connect: (state: ConnectionState) => void;
}

function setup(fetchState: () => Promise<FleetSnapshot>) {
  const timers = fakeTimers();
  let feed!: FakeFeed;
  const container = document.createElement("div");
  const panel = new LivePanel({
    container,
    feedUrl: "/api/events",
    fetchState,
    now: () => NOW,
    timers,
    createFeed: (handlers) => {
      feed = {
        started: false,
        stopped: false,
        start() {
          this.started = true;
        },
        stop() {
          this.stopped = true;
        },
        emit: handlers.onEvent,
        connect: handlers.onConnectionChange,
      };
      return feed;
    },
  });
  return { panel, container, timers, feed: () => feed };
}

const EMPTY: FleetSnapshot = { hosts: {}, activeSweeps: [] };

function sweepFrame(topic: string): LiveTailFrame {
  return {
    topic,
    event: {
      hostId: "mac-1",
      emittedAt: NOW.toISOString(),
      schemaVersion: 1,
      record: { kind: topic, issue: 5, phase: "judge" },
    },
  };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("LivePanel", () => {
  it("mounts the board, fetches, starts the feed and a one-second tick", async () => {
    const fetchState = vi.fn(async () => EMPTY);
    const { panel, container, timers, feed } = setup(fetchState);

    panel.start();
    await settle();

    expect(container.querySelector('[data-testid="live-board"]')).not.toBeNull();
    expect(fetchState).toHaveBeenCalledTimes(1);
    expect(feed().started).toBe(true);
    expect([...timers.intervals.values()].map((t) => t.ms)).toEqual([1000]);
    // The next poll is armed.
    expect([...timers.timeouts.values()].map((t) => t.ms)).toEqual([5000]);
  });

  it("polls again when the poll timer fires", async () => {
    const fetchState = vi.fn(async () => EMPTY);
    const { panel, timers } = setup(fetchState);
    panel.start();
    await settle();

    timers.runTimeouts();
    await settle();
    expect(fetchState).toHaveBeenCalledTimes(2);
  });

  it("pulls the next snapshot forward on a sweep event, not on housekeeping", async () => {
    const { panel, timers, feed } = setup(async () => EMPTY);
    panel.start();
    await settle();

    feed().emit(sweepFrame("host.health"));
    expect([...timers.timeouts.values()].map((t) => t.ms)).toEqual([5000]);

    feed().emit(sweepFrame("sweep.phase"));
    expect([...timers.timeouts.values()].map((t) => t.ms)).toEqual([EVENT_REFRESH_DEBOUNCE_MS]);
  });

  it("refetches once more when an event lands during an in-flight fetch", async () => {
    let resolve!: (value: FleetSnapshot) => void;
    const fetchState = vi
      .fn<() => Promise<FleetSnapshot>>()
      .mockImplementationOnce(() => new Promise((r) => (resolve = r)))
      .mockResolvedValue(EMPTY);
    const { panel, feed } = setup(fetchState);
    panel.start();

    feed().emit(sweepFrame("sweep.completed"));
    resolve(EMPTY);
    await settle();
    await settle();

    expect(fetchState).toHaveBeenCalledTimes(2);
  });

  it("shows a fetch failure as a banner, not a crash", async () => {
    const { panel, container } = setup(async () => {
      throw new Error("HTTP 502");
    });
    panel.start();
    await settle();
    const banner = container.querySelector<HTMLElement>('[data-testid="live-error"]')!;
    expect(banner.hidden).toBe(false);
    expect(banner.textContent).toContain("HTTP 502");
  });

  it("releases every clock and the feed on stop", async () => {
    const fetchState = vi.fn(async () => EMPTY);
    const { panel, timers, feed } = setup(fetchState);
    panel.start();
    await settle();

    panel.stop();

    expect(feed().stopped).toBe(true);
    expect(timers.intervals.size).toBe(0);
    expect(timers.timeouts.size).toBe(0);
    // An event racing the teardown does nothing.
    feed().emit(sweepFrame("sweep.phase"));
    expect(timers.timeouts.size).toBe(0);
  });

  it("does not render a fetch that resolves after stop", async () => {
    let resolve!: (value: FleetSnapshot) => void;
    const { panel, container } = setup(() => new Promise((r) => (resolve = r)));
    panel.start();
    panel.stop();
    resolve({
      hosts: {},
      activeSweeps: [{ hostId: "h", sweepId: "s", startedAt: NOW.toISOString() }],
    });
    await settle();
    expect(container.querySelector('[data-testid="live-card"]')).toBeNull();
  });
});
