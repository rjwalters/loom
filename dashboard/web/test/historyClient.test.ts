import { describe, expect, it, vi } from "vitest";
import { fetchAllHistory, fetchHistoryPage, HistoryApiError, type FetchLike } from "../src/historyClient.js";
import type { HistoryQueryResult, HistoryRecord } from "../src/types.js";

function record(id: number): HistoryRecord {
  return {
    id,
    schemaVersion: 1,
    emittedAt: "2026-07-30T12:00:00Z",
    hostId: "host-a",
    kind: "sweep.completed",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 1000,
    sweepId: `sweep-${id}`,
    ingestedAt: "2026-07-30T12:00:01Z",
    record: { kind: "sweep.completed", result: "success" },
  };
}

/** Build a `FetchLike` stub backed by an in-memory map of pages, keyed by
 * the `cursor` query param (`"none"` for the first page). Records every URL
 * it was called with so tests can assert on query-string construction. */
function stubFetch(pages: Record<string, HistoryQueryResult>): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    const parsed = new URL(url, "http://example.test");
    const cursor = parsed.searchParams.get("cursor") ?? "none";
    const page = pages[cursor];
    if (!page) throw new Error(`stubFetch: no page registered for cursor=${cursor} (url=${url})`);
    return {
      ok: true,
      status: 200,
      async json() {
        return page;
      },
    };
  };
  return { fetchImpl, calls };
}

describe("fetchHistoryPage", () => {
  it("builds the query string with the documented filter params", async () => {
    const { fetchImpl, calls } = stubFetch({
      none: { records: [record(1)], nextCursor: null },
    });
    await fetchHistoryPage(
      "/api/history",
      { host: "host-a", repo: "rjwalters/loom", model: "opus", result: "success", since: "2026-07-01T00:00:00Z", until: "2026-08-01T00:00:00Z", limit: 500 },
      undefined,
      fetchImpl,
    );
    expect(calls).toHaveLength(1);
    const url = new URL(calls[0] ?? "", "http://example.test");
    expect(url.pathname).toBe("/api/history");
    expect(Object.fromEntries(url.searchParams.entries())).toEqual({
      host: "host-a",
      repo: "rjwalters/loom",
      model: "opus",
      result: "success",
      since: "2026-07-01T00:00:00Z",
      until: "2026-08-01T00:00:00Z",
      limit: "500",
    });
  });

  it("includes cursor when paginating", async () => {
    const { fetchImpl, calls } = stubFetch({
      "42": { records: [record(1)], nextCursor: null },
    });
    await fetchHistoryPage("/api/history", {}, 42, fetchImpl);
    const url = new URL(calls[0] ?? "", "http://example.test");
    expect(url.searchParams.get("cursor")).toBe("42");
  });

  it("works unchanged against /public/history", async () => {
    const { fetchImpl, calls } = stubFetch({
      none: { records: [record(1)], nextCursor: null },
    });
    await fetchHistoryPage("/public/history", { repo: "rjwalters/loom" }, undefined, fetchImpl);
    expect(calls[0]).toMatch(/^\/public\/history\?/);
  });

  it("throws HistoryApiError with the server's error message on a non-ok response", async () => {
    const fetchImpl: FetchLike = async () => ({
      ok: false,
      status: 400,
      async json() {
        return { error: "since must be an RFC 3339 datetime" };
      },
    });
    await expect(fetchHistoryPage("/api/history", { since: "bad" }, undefined, fetchImpl)).rejects.toThrow(
      HistoryApiError,
    );
    await expect(fetchHistoryPage("/api/history", { since: "bad" }, undefined, fetchImpl)).rejects.toThrow(
      "since must be an RFC 3339 datetime",
    );
  });
});

describe("fetchAllHistory", () => {
  it("accumulates records across multiple nextCursor pages without dropping or duplicating", async () => {
    const { fetchImpl, calls } = stubFetch({
      none: { records: [record(30), record(29), record(28)], nextCursor: 28 },
      "28": { records: [record(27), record(26)], nextCursor: 26 },
      "26": { records: [record(25)], nextCursor: null },
    });

    const records = await fetchAllHistory("/api/history", {}, { fetchImpl });

    expect(records.map((r) => r.id)).toEqual([30, 29, 28, 27, 26, 25]);
    expect(calls).toHaveLength(3);
  });

  it("stops after a single page when nextCursor is null immediately", async () => {
    const { fetchImpl, calls } = stubFetch({
      none: { records: [record(1)], nextCursor: null },
    });
    const records = await fetchAllHistory("/api/history", {}, { fetchImpl });
    expect(records.map((r) => r.id)).toEqual([1]);
    expect(calls).toHaveLength(1);
  });

  it("returns an empty array when the first page has no records", async () => {
    const { fetchImpl } = stubFetch({ none: { records: [], nextCursor: null } });
    const records = await fetchAllHistory("/api/history", {}, { fetchImpl });
    expect(records).toEqual([]);
  });

  it("propagates the caller's filter to every page request", async () => {
    const calls: string[] = [];
    const fetchImpl: FetchLike = async (url: string) => {
      calls.push(url);
      const parsed = new URL(url, "http://example.test");
      const cursor = parsed.searchParams.get("cursor");
      if (cursor === null) return { ok: true, status: 200, async json() { return { records: [record(2)], nextCursor: 2 }; } };
      return { ok: true, status: 200, async json() { return { records: [record(1)], nextCursor: null }; } };
    };
    await fetchAllHistory("/api/history", { repo: "rjwalters/loom", model: "opus" }, { fetchImpl });
    for (const url of calls) {
      const parsed = new URL(url, "http://example.test");
      expect(parsed.searchParams.get("repo")).toBe("rjwalters/loom");
      expect(parsed.searchParams.get("model")).toBe("opus");
    }
  });

  it("aborts with a clear error instead of looping forever if the server never returns a null nextCursor", async () => {
    let callCount = 0;
    const fetchImpl: FetchLike = async () => {
      callCount += 1;
      return {
        ok: true,
        status: 200,
        async json() {
          // Always claims there's a next page, at a cursor that never
          // changes meaningfully — simulates a buggy/misbehaving server.
          return { records: [record(callCount)], nextCursor: 1 } satisfies HistoryQueryResult;
        },
      };
    };
    await expect(fetchAllHistory("/api/history", {}, { fetchImpl, maxPages: 5 })).rejects.toThrow(/maxPages=5/);
    expect(callCount).toBe(5);
  });

  it("uses vi.fn() as a drop-in FetchLike for simple single-page cases", async () => {
    const fetchImpl = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => ({ records: [record(1)], nextCursor: null }),
    }));
    const records = await fetchAllHistory("/api/history", {}, { fetchImpl });
    expect(records).toHaveLength(1);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });
});
