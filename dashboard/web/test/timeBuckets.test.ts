import { describe, expect, it } from "vitest";
import { bucketKey, groupByTimeBucket } from "../src/charts/timeBuckets.js";

// Every case passes an explicit zone. The default resolves through
// `displayTimeZone()`, which falls back to the *machine's* zone when nothing
// is injected — a default-using assertion would pass in California and fail
// in CI.
const UTC = "UTC";
const PACIFIC = "America/Los_Angeles";

describe("bucketKey", () => {
  it("buckets daily to the calendar day in the given zone", () => {
    expect(bucketKey("2026-07-30T23:59:59Z", "daily", UTC)).toBe("2026-07-30");
    expect(bucketKey("2026-07-30T00:00:00Z", "daily", UTC)).toBe("2026-07-30");
  });

  it("buckets weekly to the ISO week's Monday", () => {
    // 2026-07-30 is a Thursday.
    expect(bucketKey("2026-07-30T12:00:00Z", "weekly", UTC)).toBe("2026-07-27");
    // Monday itself maps to its own date.
    expect(bucketKey("2026-07-27T00:00:00Z", "weekly", UTC)).toBe("2026-07-27");
    // Sunday maps to the *preceding* Monday (still that ISO week).
    expect(bucketKey("2026-08-02T23:00:00Z", "weekly", UTC)).toBe("2026-07-27");
    // The following Monday starts a new bucket.
    expect(bucketKey("2026-08-03T00:00:00Z", "weekly", UTC)).toBe("2026-08-03");
  });

  it("throws on an invalid timestamp", () => {
    expect(() => bucketKey("not-a-date", "daily", UTC)).toThrow(/invalid emittedAt/);
  });
});

// ---------------------------------------------------------------------------
// Issue #4857: the regression this change exists for.
// ---------------------------------------------------------------------------

describe("bucketKey — zone attribution", () => {
  // The original bug, stated as a test: 23:30Z is 16:30 PDT *the same day*.
  // Bucketing in UTC pushed every sweep after 17:00 local into tomorrow's
  // bar, so the daily chart was cut at 5pm rather than midnight.
  it("attributes a late-evening UTC instant to the current Pacific day", () => {
    expect(bucketKey("2026-07-31T23:30:00Z", "daily", PACIFIC)).toBe("2026-07-31");

    // The instant that actually crosses Pacific midnight is 07:00Z.
    expect(bucketKey("2026-08-01T06:59:59Z", "daily", PACIFIC)).toBe("2026-07-31");
    expect(bucketKey("2026-08-01T07:00:00Z", "daily", PACIFIC)).toBe("2026-08-01");

    // The same pre-midnight instant is already "Aug 1" in UTC — the
    // mis-attribution this fixes.
    expect(bucketKey("2026-08-01T06:59:59Z", "daily", UTC)).toBe("2026-08-01");
  });

  it("shifts the weekly boundary with the zone too", () => {
    // 2026-08-03T05:00:00Z is Monday in UTC but still Sunday 22:00 PDT, so
    // Pacific keeps it in the week beginning 2026-07-27.
    expect(bucketKey("2026-08-03T05:00:00Z", "weekly", UTC)).toBe("2026-08-03");
    expect(bucketKey("2026-08-03T05:00:00Z", "weekly", PACIFIC)).toBe("2026-07-27");
  });

  // A zone east of UTC moves the boundary the other way, so this is not just
  // "subtract some hours".
  it("handles a zone ahead of UTC", () => {
    // 2026-07-31T16:00:00Z is 2026-08-01 01:00 in Tokyo.
    expect(bucketKey("2026-07-31T16:00:00Z", "daily", "Asia/Tokyo")).toBe("2026-08-01");
    expect(bucketKey("2026-07-31T16:00:00Z", "daily", UTC)).toBe("2026-07-31");
  });

  describe("DST", () => {
    // US Pacific springs forward 2026-03-08 (PST -08:00 → PDT -07:00). The
    // civil-date arithmetic must not drift across it.
    it("keeps day boundaries at local midnight across a spring-forward", () => {
      // 2026-03-08T07:59:59Z == 2026-03-07 23:59:59 PST — still the 7th.
      expect(bucketKey("2026-03-08T07:59:59Z", "daily", PACIFIC)).toBe("2026-03-07");
      // 2026-03-08T08:00:00Z == 2026-03-08 00:00 PST — the 8th begins.
      expect(bucketKey("2026-03-08T08:00:00Z", "daily", PACIFIC)).toBe("2026-03-08");
      // Later that day, after the 02:00 → 03:00 jump, still the 8th.
      expect(bucketKey("2026-03-08T18:00:00Z", "daily", PACIFIC)).toBe("2026-03-08");
    });

    it("keeps day boundaries at local midnight across a fall-back", () => {
      // US Pacific falls back 2026-11-01 (PDT -07:00 → PST -08:00).
      expect(bucketKey("2026-11-01T06:59:59Z", "daily", PACIFIC)).toBe("2026-10-31");
      expect(bucketKey("2026-11-01T07:00:00Z", "daily", PACIFIC)).toBe("2026-11-01");
      expect(bucketKey("2026-11-02T07:59:59Z", "daily", PACIFIC)).toBe("2026-11-01");
    });

    // The weekly path does calendar arithmetic across a transition — the
    // failure mode a naive "subtract 24h per day" would produce is landing an
    // hour off and, near midnight, on the wrong date.
    it("finds the right Monday for a week containing a DST transition", () => {
      // 2026-03-08 is a Sunday; its ISO week began Monday 2026-03-02.
      expect(bucketKey("2026-03-08T20:00:00Z", "weekly", PACIFIC)).toBe("2026-03-02");
      // 2026-11-01 is a Sunday; its ISO week began Monday 2026-10-26.
      expect(bucketKey("2026-11-01T20:00:00Z", "weekly", PACIFIC)).toBe("2026-10-26");
    });
  });
});

describe("groupByTimeBucket", () => {
  it("groups items into ascending chronological bucket order", () => {
    const items = [
      { id: "c", ts: "2026-07-30T00:00:00Z" },
      { id: "a", ts: "2026-07-28T00:00:00Z" },
      { id: "b", ts: "2026-07-28T12:00:00Z" },
    ];
    const buckets = groupByTimeBucket(items, (item) => item.ts, "daily", UTC);
    expect([...buckets.keys()]).toEqual(["2026-07-28", "2026-07-30"]);
    expect(buckets.get("2026-07-28")?.map((item) => item.id)).toEqual(["a", "b"]);
    expect(buckets.get("2026-07-30")?.map((item) => item.id)).toEqual(["c"]);
  });

  it("returns an empty map for empty input", () => {
    const buckets = groupByTimeBucket<{ ts: string }>([], (item) => item.ts, "daily", UTC);
    expect(buckets.size).toBe(0);
  });

  it("regroups the same items differently under a different zone", () => {
    // Two sweeps 40 minutes apart that straddle Pacific midnight: one day in
    // UTC, two in Pacific.
    const items = [
      { id: "before", ts: "2026-08-01T06:40:00Z" },
      { id: "after", ts: "2026-08-01T07:20:00Z" },
    ];
    expect([...groupByTimeBucket(items, (i) => i.ts, "daily", UTC).keys()]).toEqual(["2026-08-01"]);
    expect([...groupByTimeBucket(items, (i) => i.ts, "daily", PACIFIC).keys()]).toEqual([
      "2026-07-31",
      "2026-08-01",
    ]);
  });
});
