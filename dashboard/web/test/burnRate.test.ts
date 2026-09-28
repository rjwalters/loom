import { beforeEach, describe, expect, it } from "vitest";

import { DEFAULT_BURN_RATE_WINDOW_HOURS, buildPoolBurnRateSeries } from "../src/analytics/burnRate.js";
import { buildPoolBurnCurves } from "../src/analytics/burn.js";
import type { PoolBurnCurve, PoolBurnPoint, PoolBurnSegment } from "../src/analytics/burn.js";
import { parsePoolSamples } from "../src/analytics/parse.js";
import { HOUR, MINUTE, T0, poolTokensSnapshot, resetIds } from "./analyticsFixtures.js";

beforeEach(resetIds);

/** A `PoolBurnPoint` with only the fields `buildPoolBurnRateSeries` reads. */
function point(at: number, mean: number | undefined, max: number | undefined): PoolBurnPoint {
  return { at, meanUsageFraction: mean, maxUsageFraction: max, accountCount: 1, exhaustedCount: 0 };
}

/** Assemble a `PoolBurnCurve` directly from segment point lists, so a test can
 * control segmentation precisely without going through the parser/curve
 * builder. The first segment is `"initial"`, every later one `"window-reset"`
 * — the boundary kind does not matter to `buildPoolBurnRateSeries`, only that
 * it IS a boundary. */
function makeCurve(segmentPoints: PoolBurnPoint[][]): PoolBurnCurve {
  const segments: PoolBurnSegment[] = segmentPoints.map((points, index) => ({
    points,
    startedBy: index === 0 ? "initial" : "window-reset",
  }));
  const points = segments.flatMap((segment) => segment.points);
  return {
    hostId: "host-a",
    points,
    segments,
    currentSegment: segments[segments.length - 1],
    latestAt: points[points.length - 1]?.at ?? 0,
    accountCount: 1,
    exhaustedCount: 0,
  };
}

describe("buildPoolBurnRateSeries", () => {
  it("defaults to a 1-hour trailing window", () => {
    expect(DEFAULT_BURN_RATE_WINDOW_HOURS).toBe(1);
  });

  it("reports null for the first point of a segment — no earlier point to compare against", () => {
    const curve = makeCurve([[point(T0, 0.1, 0.1), point(T0 + 30 * MINUTE, 0.2, 0.3)]]);
    const series = buildPoolBurnRateSeries(curve);
    expect(series[0]).toEqual({ at: T0, meanRatePerHour: null, maxRatePerHour: null });
  });

  it("computes %/h from the earliest point still inside the trailing window", () => {
    const curve = makeCurve([[point(T0, 0.1, 0.1), point(T0 + 30 * MINUTE, 0.2, 0.3)]]);
    const series = buildPoolBurnRateSeries(curve);
    // 0.1 -> 0.2 over 0.5h = 0.2/h; 0.1 -> 0.3 over 0.5h = 0.4/h.
    expect(series[1]?.meanRatePerHour).toBeCloseTo(0.2);
    expect(series[1]?.maxRatePerHour).toBeCloseTo(0.4);
  });

  it("advances the reference point forward once an older one falls outside the window", () => {
    const curve = makeCurve([
      [
        point(T0, 0.1, 0.1),
        point(T0 + 40 * MINUTE, 0.2, 0.2),
        // T0 is 80 min before this point — outside the default 1h window, so
        // the reference must be the 40-min-old point instead.
        point(T0 + 80 * MINUTE, 0.3, 0.3),
      ],
    ]);
    const series = buildPoolBurnRateSeries(curve);
    const hours = 40 / 60;
    expect(series[2]?.meanRatePerHour).toBeCloseTo((0.3 - 0.2) / hours);
  });

  it("honours a caller-supplied window, reaching further back than the default", () => {
    const curve = makeCurve([
      [point(T0, 0.1, 0.1), point(T0 + 40 * MINUTE, 0.2, 0.2), point(T0 + 80 * MINUTE, 0.3, 0.3)],
    ]);
    const series = buildPoolBurnRateSeries(curve, { windowHours: 2 });
    const hours = 80 / 60;
    // With a 2h window the oldest point (T0) is still in range for the third.
    expect(series[2]?.meanRatePerHour).toBeCloseTo((0.3 - 0.1) / hours);
  });

  it("reports null for one series without affecting the other when only one metric is measured", () => {
    const curve = makeCurve([[point(T0, 0.1, undefined), point(T0 + 20 * MINUTE, 0.2, 0.5)]]);
    const series = buildPoolBurnRateSeries(curve);
    expect(series[1]?.meanRatePerHour).not.toBeNull();
    expect(series[1]?.maxRatePerHour).toBeNull();
  });

  it("returns an empty array for a curve with no points", () => {
    expect(buildPoolBurnRateSeries(makeCurve([]))).toEqual([]);
  });

  describe("segment boundaries — a rollover must never fabricate a large negative rate", () => {
    it("reports null, not a spurious negative rate, for the first point after a limit-window rollover", () => {
      const curves = buildPoolBurnCurves(
        parsePoolSamples([
          poolTokensSnapshot(T0, { accountCount: 2, maxUsage: 0.9, meanUsage: 0.85 }),
          poolTokensSnapshot(T0 + 10 * MINUTE, { accountCount: 2, maxUsage: 0.95, meanUsage: 0.9 }),
          // Rollover: usage falls back toward zero.
          poolTokensSnapshot(T0 + 20 * MINUTE, { accountCount: 2, maxUsage: 0.05, meanUsage: 0.05 }),
        ]),
      );
      const curve = curves[0]!;
      expect(curve.segments.map((segment) => segment.startedBy)).toEqual(["initial", "window-reset"]);

      const series = buildPoolBurnRateSeries(curve);
      // Index-aligned with curve.points: [T0, +10m, +20m (rollover)].
      expect(series).toHaveLength(3);
      const rolloverPoint = series[2]!;
      expect(rolloverPoint.meanRatePerHour).toBeNull();
      expect(rolloverPoint.maxRatePerHour).toBeNull();

      // No rate anywhere in the series reads as a huge negative cliff.
      for (const rate of series) {
        if (rate.meanRatePerHour !== null) expect(rate.meanRatePerHour).toBeGreaterThan(-1);
        if (rate.maxRatePerHour !== null) expect(rate.maxRatePerHour).toBeGreaterThan(-1);
      }
    });

    it("still computes a real rate within the segment that follows a rollover", () => {
      const curves = buildPoolBurnCurves(
        parsePoolSamples([
          poolTokensSnapshot(T0, { accountCount: 2, maxUsage: 0.9 }),
          poolTokensSnapshot(T0 + 20 * MINUTE, { accountCount: 2, maxUsage: 0.05 }), // rollover
          poolTokensSnapshot(T0 + 40 * MINUTE, { accountCount: 2, maxUsage: 0.15 }),
        ]),
      );
      const series = buildPoolBurnRateSeries(curves[0]!);
      const hours = 20 / 60;
      expect(series[2]?.maxRatePerHour).toBeCloseTo((0.15 - 0.05) / hours);
    });

    it("never looks back across a telemetry gap either", () => {
      const curves = buildPoolBurnCurves(
        parsePoolSamples([
          poolTokensSnapshot(T0, { accountCount: 2, maxUsage: 0.1 }),
          poolTokensSnapshot(T0 + 5 * HOUR, { accountCount: 2, maxUsage: 0.6 }), // gap (> 1h default)
        ]),
      );
      const curve = curves[0]!;
      expect(curve.segments.map((segment) => segment.startedBy)).toEqual(["initial", "gap"]);
      const series = buildPoolBurnRateSeries(curve);
      expect(series[1]?.maxRatePerHour).toBeNull();
    });
  });
});
