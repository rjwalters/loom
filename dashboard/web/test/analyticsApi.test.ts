import { beforeEach, describe, expect, it, vi } from "vitest";

import { fetchHistory } from "../src/analytics/api.js";
import { T0, resetIds, tokensSnapshot } from "./analyticsFixtures.js";

beforeEach(resetIds);

function jsonResponse(body: unknown): Response {
  return { ok: true, status: 200, json: async () => body } as unknown as Response;
}

describe("fetchHistory", () => {
  it("requests the authenticated prefix at the API's maximum page size", async () => {
    const calls: string[] = [];
    const fetchImpl = vi.fn(async (url: string) => {
      calls.push(url);
      return jsonResponse({ records: [], nextCursor: null });
    }) as unknown as typeof fetch;

    await fetchHistory({ since: T0, fetchImpl });

    expect(calls).toHaveLength(1);
    expect(calls[0]).toContain("/api/history?");
    expect(calls[0]).toContain("limit=500");
    expect(calls[0]).toContain(encodeURIComponent(new Date(T0).toISOString()));
  });

  it("walks the keyset cursor until the API stops issuing one", async () => {
    const pages = [
      { records: [tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }])], nextCursor: 9 },
      { records: [tokensSnapshot(T0, [{ account: "agent-1", usage: 0.2 }])], nextCursor: null },
    ];
    let index = 0;
    const seen: string[] = [];
    const fetchImpl = vi.fn(async (url: string) => {
      seen.push(url);
      return jsonResponse(pages[index++]);
    }) as unknown as typeof fetch;

    const records = await fetchHistory({ fetchImpl });

    expect(records).toHaveLength(2);
    expect(seen).toHaveLength(2);
    expect(seen[1]).toContain("cursor=9");
  });

  it("stops at maxPages so a busy fleet cannot hang the page", async () => {
    const fetchImpl = vi.fn(async () =>
      jsonResponse({ records: [tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }])], nextCursor: 5 }),
    ) as unknown as typeof fetch;

    await fetchHistory({ fetchImpl, maxPages: 3 });

    expect(fetchImpl).toHaveBeenCalledTimes(3);
  });

  it("throws on a non-2xx response", async () => {
    const fetchImpl = vi.fn(async () => ({ ok: false, status: 401 }) as unknown as Response) as unknown as typeof fetch;
    await expect(fetchHistory({ fetchImpl })).rejects.toThrow("401");
  });

  it("drops rows missing the fields every consumer needs", async () => {
    const good = tokensSnapshot(T0, [{ account: "agent-1", usage: 0.1 }]);
    const fetchImpl = vi.fn(async () =>
      jsonResponse({
        records: [good, { id: "not-a-number" }, { id: 2, hostId: "host-a" }, null, "nope"],
        nextCursor: null,
      }),
    ) as unknown as typeof fetch;

    const records = await fetchHistory({ fetchImpl });

    expect(records).toHaveLength(1);
    expect(records[0]?.kind).toBe("tokens.snapshot");
  });

  it("treats an unparseable body as an empty final page", async () => {
    const fetchImpl = vi.fn(async () =>
      ({
        ok: true,
        status: 200,
        json: async () => {
          throw new Error("not json");
        },
      }) as unknown as Response,
    ) as unknown as typeof fetch;

    await expect(fetchHistory({ fetchImpl })).resolves.toEqual([]);
    expect(fetchImpl).toHaveBeenCalledTimes(1);
  });
});
