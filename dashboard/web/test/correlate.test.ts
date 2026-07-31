import { beforeEach, describe, expect, it } from "vitest";
import { correlateSweeps } from "../src/charts/correlate.js";
import { makeCompletedSweepPair, makeSweepCompleted, makeSweepOutcome, resetFixtureIds } from "./fixtures.js";

beforeEach(() => {
  resetFixtureIds();
});

describe("correlateSweeps", () => {
  it("merges a sweep.completed + sweep.outcome pair by sweepId", () => {
    const records = makeCompletedSweepPair({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T12:08:32Z",
      result: "success",
      model: "opus",
      totalDurationSec: 512,
      phaseDurations: [
        { phase: "curator", duration_sec: 12 },
        { phase: "builder", duration_sec: 340 },
      ],
    });

    const correlated = correlateSweeps(records);
    expect(correlated.size).toBe(1);
    const sweep = correlated.get("sweep-1");
    expect(sweep).toEqual({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T12:08:32Z",
      result: "success",
      model: "opus",
      totalDurationSec: 512,
      phaseDurations: [
        { phase: "curator", durationSec: 12 },
        { phase: "builder", durationSec: 340 },
      ],
    });
  });

  it("is order-independent (outcome before completed, or vice versa)", () => {
    const pairA = makeCompletedSweepPair({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T12:08:32Z",
      result: "failure",
      model: "sonnet",
      totalDurationSec: 100,
      phaseDurations: [{ phase: "builder", duration_sec: 100 }],
    });
    const reversed = [...pairA].reverse();

    const forward = correlateSweeps(pairA);
    const backward = correlateSweeps(reversed);
    expect(backward.get("sweep-1")).toEqual(forward.get("sweep-1"));
  });

  it("handles a sweep.completed record with no matching sweep.outcome (still in-flight telemetry)", () => {
    const record = makeSweepCompleted({
      sweepId: "sweep-only-completed",
      emittedAt: "2026-07-30T12:00:00Z",
      result: "cancelled",
    });
    const correlated = correlateSweeps([record]);
    const sweep = correlated.get("sweep-only-completed");
    expect(sweep?.result).toBe("cancelled");
    expect(sweep?.totalDurationSec).toBeUndefined();
    expect(sweep?.phaseDurations).toEqual([]);
  });

  it("handles a sweep.outcome record with no matching sweep.completed", () => {
    const record = makeSweepOutcome({
      sweepId: "sweep-only-outcome",
      emittedAt: "2026-07-30T12:00:00Z",
      result: "success",
      model: "opus",
      totalDurationSec: 42,
      phaseDurations: [{ phase: "judge", duration_sec: 42 }],
    });
    const correlated = correlateSweeps([record]);
    const sweep = correlated.get("sweep-only-outcome");
    expect(sweep?.result).toBe("success");
    expect(sweep?.totalDurationSec).toBe(42);
    expect(sweep?.emittedAt).toBe("2026-07-30T12:00:00Z");
  });

  it("prefers sweep.completed's emittedAt/result over sweep.outcome's when both are present", () => {
    const outcome = makeSweepOutcome({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T11:55:00Z",
      result: "success",
      model: "opus",
      totalDurationSec: 200,
    });
    const completed = makeSweepCompleted({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T12:00:00Z",
      result: "success",
    });
    const correlated = correlateSweeps([outcome, completed]);
    expect(correlated.get("sweep-1")?.emittedAt).toBe("2026-07-30T12:00:00Z");
  });

  it("ignores unrelated record kinds", () => {
    const unrelated = makeSweepCompleted({ sweepId: "sweep-1", emittedAt: "2026-07-30T12:00:00Z", result: "success" });
    unrelated.kind = "sweep.started";
    const correlated = correlateSweeps([unrelated]);
    expect(correlated.size).toBe(0);
  });

  it("skips records with a null sweepId (fully redacted private rows)", () => {
    const record = makeSweepCompleted({ sweepId: "sweep-1", emittedAt: "2026-07-30T12:00:00Z", result: "success" });
    record.sweepId = null;
    const correlated = correlateSweeps([record]);
    expect(correlated.size).toBe(0);
  });

  it("is idempotent when the same record appears twice (e.g. an overlapping page boundary)", () => {
    const records = makeCompletedSweepPair({
      sweepId: "sweep-1",
      emittedAt: "2026-07-30T12:00:00Z",
      result: "success",
      model: "opus",
      totalDurationSec: 10,
      phaseDurations: [{ phase: "builder", duration_sec: 10 }],
    });
    const duplicated = [...records, ...records];
    const correlated = correlateSweeps(duplicated);
    expect(correlated.size).toBe(1);
    expect(correlated.get("sweep-1")?.phaseDurations).toEqual([{ phase: "builder", durationSec: 10 }]);
  });
});
