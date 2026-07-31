import { beforeEach, describe, expect, it } from "vitest";
import { buildDurationPercentiles, computePercentiles } from "../src/charts/durations.js";
import { makeCompletedSweepPair, makeSweepCompleted, resetFixtureIds } from "./fixtures.js";

beforeEach(() => {
  resetFixtureIds();
});

describe("computePercentiles", () => {
  it("computes nearest-rank percentiles over a known 10-element set", () => {
    // 10 values, 10..100 step 10. Nearest-rank: index = ceil(rank/100*10)-1.
    const values = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
    const result = computePercentiles(values, [50, 90, 99]);
    // p50 -> ceil(5)-1 = 4 -> values[4] = 50
    expect(result[50]).toBe(50);
    // p90 -> ceil(9)-1 = 8 -> values[8] = 90
    expect(result[90]).toBe(90);
    // p99 -> ceil(9.9)-1 = 9 -> values[9] = 100
    expect(result[99]).toBe(100);
  });

  it("is insensitive to input order", () => {
    const sorted = [1, 2, 3, 4, 5];
    const shuffled = [5, 1, 4, 2, 3];
    expect(computePercentiles(shuffled)).toEqual(computePercentiles(sorted));
  });

  it("handles a single-element set (all percentiles equal the one value)", () => {
    const result = computePercentiles([42]);
    expect(result).toEqual({ 50: 42, 90: 42, 99: 42 });
  });

  it("throws on an empty set", () => {
    expect(() => computePercentiles([])).toThrow(/non-empty/);
  });
});

describe("buildDurationPercentiles", () => {
  it("computes overall total-duration percentiles and a per-phase breakdown", () => {
    const records = [
      ...makeCompletedSweepPair({
        sweepId: "s1",
        emittedAt: "2026-07-28T00:00:00Z",
        result: "success",
        model: "opus",
        totalDurationSec: 100,
        phaseDurations: [
          { phase: "curator", duration_sec: 10 },
          { phase: "builder", duration_sec: 90 },
        ],
      }),
      ...makeCompletedSweepPair({
        sweepId: "s2",
        emittedAt: "2026-07-28T01:00:00Z",
        result: "failure",
        model: "opus",
        totalDurationSec: 200,
        phaseDurations: [
          { phase: "curator", duration_sec: 20 },
          { phase: "builder", duration_sec: 180 },
        ],
      }),
    ];

    const result = buildDurationPercentiles(records);
    expect(result.overall).toEqual(computePercentiles([100, 200]));
    expect(result.byPhase.curator).toEqual(computePercentiles([10, 20]));
    expect(result.byPhase.builder).toEqual(computePercentiles([90, 180]));
  });

  it("omits `overall` when no sweep has a known total duration", () => {
    const record = makeSweepCompleted({ sweepId: "s1", emittedAt: "2026-07-28T00:00:00Z", result: "success" });
    const result = buildDurationPercentiles([record]);
    expect(result.overall).toBeUndefined();
    expect(result.byPhase).toEqual({});
  });

  it("only includes a phase that appears in at least one sweep", () => {
    const records = makeCompletedSweepPair({
      sweepId: "s1",
      emittedAt: "2026-07-28T00:00:00Z",
      result: "success",
      model: "opus",
      totalDurationSec: 10,
      phaseDurations: [{ phase: "judge", duration_sec: 10 }],
    });
    const result = buildDurationPercentiles(records);
    expect(Object.keys(result.byPhase)).toEqual(["judge"]);
  });

  it("returns an empty result for no records", () => {
    const result = buildDurationPercentiles([]);
    expect(result.overall).toBeUndefined();
    expect(result.byPhase).toEqual({});
  });
});
