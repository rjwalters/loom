// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { LiveFeedPanel } from "../src/liveFeedPanel.js";
import type { LiveTailFrame } from "../src/types.js";

function frame(
  topic: string,
  emittedAt: string,
  recordOverrides: Record<string, unknown> = {},
): LiveTailFrame {
  return {
    topic,
    event: {
      hostId: "host-abc",
      emittedAt,
      schemaVersion: 1,
      record: { kind: topic, sweep_id: "sweep-issue-4703-0", ...recordOverrides },
    },
  };
}

function dataFrame(frameObj: LiveTailFrame): string {
  return `data: ${JSON.stringify(frameObj)}\n\n`;
}

/** A fake reader that yields one already-encoded chunk, then ends. */
function readerFor(text: string): ReadableStreamDefaultReader<Uint8Array> {
  let served = false;
  const encoder = new TextEncoder();
  return {
    async read() {
      if (served) return { done: true, value: undefined } as ReadableStreamReadResult<Uint8Array>;
      served = true;
      return { done: false, value: encoder.encode(text) };
    },
    releaseLock() {},
    cancel: async () => undefined,
    closed: Promise.resolve(undefined),
  } as unknown as ReadableStreamDefaultReader<Uint8Array>;
}

describe("LiveFeedPanel", () => {
  it("renders an incoming frame as a row, newest first", async () => {
    const container = document.createElement("ul");
    const data = dataFrame(frame("sweep.phase", "2026-07-30T12:03:20Z"));

    let panel!: LiveFeedPanel;
    const connect = vi.fn(async () => readerFor(data));

    panel = new LiveFeedPanel({
      container,
      url: "/api/events",
      connect,
      sleep: async () => {
        panel.stop();
      },
    });
    panel.start();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(container.childElementCount).toBe(1);
    expect(container.firstElementChild?.getAttribute("data-topic")).toBe("sweep.phase");
  });

  it("applies a client-side model/result filter (the live-tail endpoint doesn't filter server-side)", async () => {
    const container = document.createElement("ul");
    const wrongModel = dataFrame(
      frame("sweep.started", "2026-07-30T12:00:00Z", { model: "haiku" }),
    );
    const rightModel = dataFrame(
      frame("sweep.started", "2026-07-30T12:00:01Z", { model: "opus" }),
    );

    let panel!: LiveFeedPanel;
    const connect = vi.fn(async () => readerFor(wrongModel + rightModel));

    panel = new LiveFeedPanel({
      container,
      url: "/api/events",
      connect,
      filter: { model: "opus" },
      sleep: async () => {
        panel.stop();
      },
    });
    panel.start();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(container.childElementCount).toBe(1);
  });

  it("caps rendered rows at maxRows, dropping the oldest", async () => {
    const container = document.createElement("ul");
    const frames = Array.from({ length: 5 }, (_, i) =>
      dataFrame(frame("sweep.phase", `2026-07-30T12:0${i}:00Z`)),
    ).join("");

    let panel!: LiveFeedPanel;
    const connect = vi.fn(async () => readerFor(frames));

    panel = new LiveFeedPanel({
      container,
      url: "/api/events",
      connect,
      maxRows: 3,
      sleep: async () => {
        panel.stop();
      },
    });
    panel.start();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(container.childElementCount).toBe(3);
  });
});
