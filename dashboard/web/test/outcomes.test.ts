import { beforeEach, describe, expect, it } from "vitest";
import { buildOutcomesOverTime } from "../src/charts/outcomes.js";
import { makeCompletedSweepPair, makeSweepCompleted, resetFixtureIds } from "./fixtures.js";

beforeEach(() => {
  resetFixtureIds();
});

describe("buildOutcomesOverTime", () => {
  it("counts sweeps by result, bucketed daily", () => {
    const records = [
      ...makeCompletedSweepPair({
        sweepId: "s1",
        emittedAt: "2026-07-28T10:00:00Z",
        result: "success",
        model: "opus",
        totalDurationSec: 100,
        phaseDurations: [],
      }),
      ...makeCompletedSweepPair({
        sweepId: "s2",
        emittedAt: "2026-07-28T14:00:00Z",
        result: "failure",
        model: "opus",
        totalDurationSec: 50,
        phaseDurations: [],
      }),
      ...makeCompletedSweepPair({
        sweepId: "s3",
        emittedAt: "2026-07-29T09:00:00Z",
        result: "success",
        model: "sonnet",
        totalDurationSec: 200,
        phaseDurations: [],
      }),
    ];

    const buckets = buildOutcomesOverTime(records, "daily", "UTC");
    expect(buckets).toEqual([
      {
        bucketKey: "2026-07-28",
        counts: { success: 1, failure: 1, cancelled: 0, blocked: 0 },
        total: 2,
      },
      {
        bucketKey: "2026-07-29",
        counts: { success: 1, failure: 0, cancelled: 0, blocked: 0 },
        total: 1,
      },
    ]);
  });

  it("buckets weekly when requested", () => {
    const records = [
      makeSweepCompleted({ sweepId: "s1", emittedAt: "2026-07-27T00:00:00Z", result: "success" }),
      makeSweepCompleted({ sweepId: "s2", emittedAt: "2026-07-30T00:00:00Z", result: "blocked" }),
      makeSweepCompleted({ sweepId: "s3", emittedAt: "2026-08-03T00:00:00Z", result: "cancelled" }),
    ];
    const buckets = buildOutcomesOverTime(records, "weekly", "UTC");
    expect(buckets.map((b) => b.bucketKey)).toEqual(["2026-07-27", "2026-08-03"]);
    expect(buckets[0]?.total).toBe(2);
    expect(buckets[0]?.counts).toEqual({ success: 1, failure: 0, cancelled: 0, blocked: 1 });
    expect(buckets[1]?.counts).toEqual({ success: 0, failure: 0, cancelled: 1, blocked: 0 });
  });

  it("excludes sweeps with no known result yet", () => {
    // A sweep.outcome-only record with no result set at all (still running,
    // hypothetically) should not appear in any bucket.
    const inFlight = makeSweepCompleted({ sweepId: "s1", emittedAt: "2026-07-28T00:00:00Z", result: "success" });
    inFlight.record = { ...inFlight.record, result: undefined };
    const buckets = buildOutcomesOverTime([inFlight], "daily", "UTC");
    expect(buckets).toEqual([]);
  });

  it("returns an empty array for no records", () => {
    expect(buildOutcomesOverTime([], "daily", "UTC")).toEqual([]);
  });

  it("defaults to daily granularity", () => {
    const record = makeSweepCompleted({ sweepId: "s1", emittedAt: "2026-07-28T00:00:00Z", result: "success" });
    expect(buildOutcomesOverTime([record], "daily", "UTC")).toEqual([
      { bucketKey: "2026-07-28", counts: { success: 1, failure: 0, cancelled: 0, blocked: 0 }, total: 1 },
    ]);
  });
});
