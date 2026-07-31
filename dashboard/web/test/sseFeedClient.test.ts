import { describe, expect, it, vi } from "vitest";
import { LiveFeedClient, SseStreamParser, frameKey } from "../src/sseFeedClient.js";
import type { LiveTailFrame } from "../src/types.js";

function frame(overrides: Partial<LiveTailFrame["event"]> = {}, topic = "sweep.phase"): string {
  const event = {
    hostId: "host-abc",
    emittedAt: "2026-07-30T12:03:20Z",
    schemaVersion: 1,
    record: { kind: topic, sweep_id: "sweep-issue-4703-0", phase: "builder", entered_at: "2026-07-30T12:03:20Z" },
    ...overrides,
  };
  return `data: ${JSON.stringify({ topic, event })}\n\n`;
}

function parsedFrame(raw: string): LiveTailFrame {
  return JSON.parse(raw.slice("data: ".length).trimEnd());
}

describe("SseStreamParser", () => {
  it("parses a data frame delivered in a single chunk", () => {
    const parser = new SseStreamParser();
    const frames = parser.feed(frame());
    expect(frames).toHaveLength(1);
    expect(frames[0]?.type).toBe("data");
  });

  it("parses the retry + connect-comment preamble as two frames", () => {
    const parser = new SseStreamParser();
    const frames = parser.feed(
      "retry: 3000\n: connected to loom fleet telemetry live tail\n\n",
    );
    expect(frames).toEqual([
      { type: "retry", retryMs: 3000 },
      { type: "comment", text: "connected to loom fleet telemetry live tail" },
    ]);
  });

  it("parses a bare keepalive comment", () => {
    const parser = new SseStreamParser();
    const frames = parser.feed(": keepalive\n\n");
    expect(frames).toEqual([{ type: "comment", text: "keepalive" }]);
  });

  it("buffers a frame split across multiple chunks", () => {
    const parser = new SseStreamParser();
    const whole = frame();
    const splitPoint = Math.floor(whole.length / 2);

    const firstHalf = parser.feed(whole.slice(0, splitPoint));
    expect(firstHalf).toHaveLength(0); // no complete frame yet

    const secondHalf = parser.feed(whole.slice(splitPoint));
    expect(secondHalf).toHaveLength(1);
    expect(secondHalf[0]?.type).toBe("data");
  });

  it("handles CRLF line endings the same as LF", () => {
    const parser = new SseStreamParser();
    const frames = parser.feed("retry: 3000\r\n: keepalive\r\n\r\n");
    expect(frames).toEqual([
      { type: "retry", retryMs: 3000 },
      { type: "comment", text: "keepalive" },
    ]);
  });

  it("parses multiple frames delivered in one chunk", () => {
    const parser = new SseStreamParser();
    const frames = parser.feed(frame() + ": keepalive\n\n" + frame({ emittedAt: "2026-07-30T12:03:21Z" }));
    expect(frames.map((f) => f.type)).toEqual(["data", "comment", "data"]);
  });
});

describe("frameKey", () => {
  it("is stable for identical frames and distinct for differing ones", () => {
    const a = parsedFrame(frame());
    const b = parsedFrame(frame());
    const c = parsedFrame(frame({ emittedAt: "2026-07-30T12:03:21Z" }));
    expect(frameKey(a)).toBe(frameKey(b));
    expect(frameKey(a)).not.toBe(frameKey(c));
  });
});

/** A fake reader that yields a fixed script of string chunks, then ends the stream. */
function scriptedReader(chunks: string[]): ReadableStreamDefaultReader<Uint8Array> {
  let index = 0;
  const encoder = new TextEncoder();
  return {
    async read() {
      if (index >= chunks.length) {
        return { done: true, value: undefined } as ReadableStreamReadResult<Uint8Array>;
      }
      const value = encoder.encode(chunks[index]!);
      index += 1;
      return { done: false, value };
    },
    releaseLock() {},
    cancel: async () => undefined,
    closed: Promise.resolve(undefined),
  } as unknown as ReadableStreamDefaultReader<Uint8Array>;
}

describe("LiveFeedClient", () => {
  it("emits parsed frames and ignores keepalive/preamble comments", async () => {
    let client!: LiveFeedClient;
    const connect = vi.fn(async () =>
      scriptedReader([
        "retry: 3000\n: connected to loom fleet telemetry live tail\n\n",
        ": keepalive\n\n",
        frame(),
      ]),
    );
    const onEvent = vi.fn((f: LiveTailFrame) => {
      void f;
      client.stop(); // self-terminate once we've observed the one real event
    });

    client = new LiveFeedClient({ url: "/api/events", connect, onEvent, sleep: async () => undefined });
    client.start();
    await client.whenIdle();

    expect(connect).toHaveBeenCalledTimes(1);
    expect(onEvent).toHaveBeenCalledTimes(1);
    expect(onEvent.mock.calls[0]?.[0]?.topic).toBe("sweep.phase");
  });

  it("does not duplicate an event re-delivered across a reconnect", async () => {
    const sameFrame = frame();
    const newFrame = frame({ emittedAt: "2026-07-30T12:03:22Z" });
    const connections = [
      scriptedReader([sameFrame]), // connection 1: delivers one frame, then the stream ends
      scriptedReader([sameFrame, newFrame]), // reconnect: server redelivers + adds one new frame
    ];
    const connect = vi.fn(async () => connections.shift()!);
    const events: string[] = [];

    // A manually-released gate stands in for the reconnect delay so the test
    // controls exactly when the client reconnects, instead of racing a timer.
    let releaseGate: (() => void) | undefined;
    const gate = () => new Promise<void>((resolve) => (releaseGate = resolve));

    const client = new LiveFeedClient({
      url: "/api/events",
      connect,
      onEvent: (f) => events.push(f.event.emittedAt),
      sleep: async () => gate(),
    });

    client.start();

    await vi.waitFor(() => expect(events).toEqual(["2026-07-30T12:03:20Z"]));
    expect(connect).toHaveBeenCalledTimes(1);

    releaseGate?.();
    await vi.waitFor(() =>
      expect(events).toEqual(["2026-07-30T12:03:20Z", "2026-07-30T12:03:22Z"]),
    );
    expect(connect).toHaveBeenCalledTimes(2);

    client.stop();
  });

  it("honors a retry: directive for the reconnect delay", async () => {
    let client!: LiveFeedClient;
    const recordedDelays: number[] = [];
    const connect = vi.fn(async () => scriptedReader(["retry: 9000\n: connected\n\n"]));
    const sleep = vi.fn(async (ms: number) => {
      recordedDelays.push(ms);
      client.stop(); // stop right after observing the delay so the loop ends cleanly
    });

    client = new LiveFeedClient({ url: "/api/events", connect, onEvent: vi.fn(), sleep });
    client.start();
    await client.whenIdle();

    expect(recordedDelays).toEqual([9000]);
  });

  it("reconnects using the default retry delay before any retry: directive arrives", async () => {
    let client!: LiveFeedClient;
    const recordedDelays: number[] = [];
    const connect = vi.fn(async () => scriptedReader([]));
    const sleep = vi.fn(async (ms: number) => {
      recordedDelays.push(ms);
      client.stop();
    });

    client = new LiveFeedClient({ url: "/api/events", connect, onEvent: vi.fn(), sleep });
    client.start();
    await client.whenIdle();

    expect(recordedDelays).toEqual([3000]);
  });

  it("builds the connect URL with host/repo query params", async () => {
    let client!: LiveFeedClient;
    let calledUrl: string | undefined;
    const connect = vi.fn(async (url: string) => {
      calledUrl = url;
      client.stop();
      return scriptedReader([]);
    });

    client = new LiveFeedClient({
      url: "/api/events",
      host: "host-abc",
      repo: "rjwalters/loom",
      connect,
      onEvent: vi.fn(),
      sleep: async () => undefined,
    });
    client.start();
    await client.whenIdle();

    expect(calledUrl).toContain("host=host-abc");
    expect(calledUrl).toContain("repo=rjwalters%2Floom");
  });

  it("reports a JSON parse error without throwing", async () => {
    let client!: LiveFeedClient;
    const connect = vi.fn(async () => scriptedReader(["data: {not json\n\n"]));
    const onParseError = vi.fn((_payload: string, _err: unknown) => {
      client.stop();
    });

    client = new LiveFeedClient({
      url: "/api/events",
      connect,
      onEvent: vi.fn(),
      onParseError,
      sleep: async () => undefined,
    });
    client.start();
    await client.whenIdle();

    expect(onParseError).toHaveBeenCalledTimes(1);
    expect(onParseError.mock.calls[0]?.[0]).toBe("{not json");
  });
});
