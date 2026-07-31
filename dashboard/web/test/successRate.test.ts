import { describe, expect, it } from "vitest";
import { buildSuccessRateTrend } from "../src/charts/successRate.js";
import type { OutcomeBucket } from "../src/charts/outcomes.js";

describe("buildSuccessRateTrend", () => {
  it("computes success rate per bucket from outcome counts", () => {
    const buckets: OutcomeBucket[] = [
      { bucketKey: "2026-07-28", counts: { success: 3, failure: 1, cancelled: 0, blocked: 0 }, total: 4 },
      { bucketKey: "2026-07-29", counts: { success: 0, failure: 2, cancelled: 0, blocked: 0 }, total: 2 },
    ];
    expect(buildSuccessRateTrend(buckets)).toEqual([
      { bucketKey: "2026-07-28", successRate: 0.75, total: 4 },
      { bucketKey: "2026-07-29", successRate: 0, total: 2 },
    ]);
  });

  it("uses null (not 0) for an empty bucket, so a chart can render a gap", () => {
    const buckets: OutcomeBucket[] = [
      { bucketKey: "2026-07-28", counts: { success: 0, failure: 0, cancelled: 0, blocked: 0 }, total: 0 },
    ];
    expect(buildSuccessRateTrend(buckets)).toEqual([{ bucketKey: "2026-07-28", successRate: null, total: 0 }]);
  });

  it("returns an empty array for no buckets", () => {
    expect(buildSuccessRateTrend([])).toEqual([]);
  });
});
