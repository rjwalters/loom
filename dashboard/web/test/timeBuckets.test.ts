import { describe, expect, it } from "vitest";
import { bucketKey, groupByTimeBucket } from "../src/charts/timeBuckets.js";

describe("bucketKey", () => {
  it("buckets daily to the UTC calendar day", () => {
    expect(bucketKey("2026-07-30T23:59:59Z", "daily")).toBe("2026-07-30");
    expect(bucketKey("2026-07-30T00:00:00Z", "daily")).toBe("2026-07-30");
  });

  it("buckets weekly to the ISO week's Monday", () => {
    // 2026-07-30 is a Thursday.
    expect(bucketKey("2026-07-30T12:00:00Z", "weekly")).toBe("2026-07-27");
    // Monday itself maps to its own date.
    expect(bucketKey("2026-07-27T00:00:00Z", "weekly")).toBe("2026-07-27");
    // Sunday maps to the *preceding* Monday (still that ISO week).
    expect(bucketKey("2026-08-02T23:00:00Z", "weekly")).toBe("2026-07-27");
    // The following Monday starts a new bucket.
    expect(bucketKey("2026-08-03T00:00:00Z", "weekly")).toBe("2026-08-03");
  });

  it("throws on an invalid timestamp", () => {
    expect(() => bucketKey("not-a-date", "daily")).toThrow(/invalid emittedAt/);
  });
});

describe("groupByTimeBucket", () => {
  it("groups items into ascending chronological bucket order", () => {
    const items = [
      { id: "c", ts: "2026-07-30T00:00:00Z" },
      { id: "a", ts: "2026-07-28T00:00:00Z" },
      { id: "b", ts: "2026-07-28T12:00:00Z" },
    ];
    const buckets = groupByTimeBucket(items, (item) => item.ts, "daily");
    expect([...buckets.keys()]).toEqual(["2026-07-28", "2026-07-30"]);
    expect(buckets.get("2026-07-28")?.map((item) => item.id)).toEqual(["a", "b"]);
    expect(buckets.get("2026-07-30")?.map((item) => item.id)).toEqual(["c"]);
  });

  it("returns an empty map for empty input", () => {
    const buckets = groupByTimeBucket<{ ts: string }>([], (item) => item.ts, "daily");
    expect(buckets.size).toBe(0);
  });
});
