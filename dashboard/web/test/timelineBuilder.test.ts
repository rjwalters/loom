import { describe, expect, it } from "vitest";
import { SweepTimelineBuilder, buildSweepTimeline } from "../src/timelineBuilder.js";
import type {
  SweepCompletedRecord,
  SweepOutcomeRecord,
  SweepPhaseRecord,
  SweepStartedRecord,
} from "../src/types.js";

const SWEEP_ID = "sweep-issue-4703-0";

function phase(phaseName: string, enteredAt: string): SweepPhaseRecord {
  return {
    kind: "sweep.phase",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    phase: phaseName,
    entered_at: enteredAt,
  };
}

function started(startedAt: string, model = "opus", effort = "high"): SweepStartedRecord {
  return {
    kind: "sweep.started",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    started_at: startedAt,
    model,
    effort,
  };
}

function completed(completedAt: string, result: SweepCompletedRecord["result"] = "success"): SweepCompletedRecord {
  return {
    kind: "sweep.completed",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    completed_at: completedAt,
    result,
  };
}

function outcome(overrides: Partial<SweepOutcomeRecord> = {}): SweepOutcomeRecord {
  return {
    kind: "sweep.outcome",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    model: "opus",
    effort: "high",
    result: "success",
    ...overrides,
  };
}

describe("SweepTimelineBuilder", () => {
  it("computes per-phase durations from consecutive entered_at gaps", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      phase("curator", "2026-07-30T12:00:00Z"),
      phase("builder", "2026-07-30T12:00:12Z"),
      phase("judge", "2026-07-30T12:05:52Z"),
    ]);

    expect(timeline.phases).toEqual([
      { phase: "curator", enteredAt: "2026-07-30T12:00:00Z", durationSec: 12, ongoing: false },
      { phase: "builder", enteredAt: "2026-07-30T12:00:12Z", durationSec: 340, ongoing: false },
      { phase: "judge", enteredAt: "2026-07-30T12:05:52Z", durationSec: undefined, ongoing: true },
    ]);
  });

  it("sorts out-of-order phase records by entered_at before computing durations", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      phase("builder", "2026-07-30T12:00:12Z"),
      phase("curator", "2026-07-30T12:00:00Z"),
    ]);

    expect(timeline.phases.map((p) => p.phase)).toEqual(["curator", "builder"]);
    expect(timeline.phases[0]?.durationSec).toBe(12);
  });

  it("closes the last phase's duration using sweep.completed when present", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      phase("merge", "2026-07-30T12:08:00Z"),
      completed("2026-07-30T12:08:32Z", "success"),
    ]);

    expect(timeline.phases).toEqual([
      { phase: "merge", enteredAt: "2026-07-30T12:08:00Z", durationSec: 32, ongoing: false },
    ]);
    expect(timeline.result).toBe("success");
  });

  it("keeps two entries in lifecycle order for a phase that repeats (judge <-> doctor cycle)", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      phase("judge", "2026-07-30T12:01:00Z"),
      phase("doctor", "2026-07-30T12:02:00Z"),
      phase("judge", "2026-07-30T12:04:00Z"),
      phase("merge", "2026-07-30T12:05:00Z"),
    ]);

    expect(timeline.phases.map((p) => p.phase)).toEqual(["judge", "doctor", "judge", "merge"]);
    expect(timeline.phases[0]?.durationSec).toBe(60);
    expect(timeline.phases[2]?.durationSec).toBe(60);
  });

  it("prefers sweep.outcome's authoritative phase_durations over computed gaps", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      phase("curator", "2026-07-30T12:00:00Z"),
      phase("builder", "2026-07-30T12:00:12Z"),
      outcome({
        phase_durations: [
          { phase: "curator", duration_sec: 12 },
          { phase: "builder", duration_sec: 500 }, // authoritative value differs from any computed gap
        ],
        total_duration_sec: 512,
        pr_number: 4710,
        result: "success",
      }),
    ]);

    expect(timeline.phases[1]).toEqual({
      phase: "builder",
      enteredAt: "2026-07-30T12:00:12Z",
      durationSec: 500,
      ongoing: false,
    });
    expect(timeline.totalDurationSec).toBe(512);
    expect(timeline.prNumber).toBe(4710);
    expect(timeline.result).toBe("success");
  });

  it("reports result/prNumber/model/effort from sweep.outcome for a completed sweep", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [
      started("2026-07-30T12:00:00Z", "opus", "high"),
      phase("curator", "2026-07-30T12:00:00Z"),
      outcome({ result: "failure", pr_number: undefined }),
    ]);

    expect(timeline.result).toBe("failure");
    expect(timeline.prNumber).toBeUndefined();
    expect(timeline.model).toBe("opus");
    expect(timeline.effort).toBe("high");
  });

  it("marks the sweep's most recent phase as ongoing while in flight", () => {
    const timeline = buildSweepTimeline(SWEEP_ID, [phase("builder", "2026-07-30T12:00:00Z")]);
    expect(timeline.phases[0]?.ongoing).toBe(true);
    expect(timeline.result).toBeUndefined();
  });

  it("ignores records for a different sweep_id", () => {
    const builder = new SweepTimelineBuilder(SWEEP_ID);
    builder.addRecord(phase("curator", "2026-07-30T12:00:00Z"));
    builder.addRecord({ ...phase("builder", "2026-07-30T12:00:12Z"), sweep_id: "some-other-sweep" });

    const timeline = builder.getTimeline();
    expect(timeline.phases).toHaveLength(1);
    expect(timeline.phases[0]?.phase).toBe("curator");
  });

  it("supports incremental ingestion via addRecord, re-deriving on each call", () => {
    const builder = new SweepTimelineBuilder(SWEEP_ID);
    builder.addRecord(phase("curator", "2026-07-30T12:00:00Z"));
    expect(builder.getTimeline().phases[0]?.ongoing).toBe(true);

    builder.addRecord(phase("builder", "2026-07-30T12:00:12Z"));
    const midTimeline = builder.getTimeline();
    expect(midTimeline.phases[0]?.durationSec).toBe(12);
    expect(midTimeline.phases[1]?.ongoing).toBe(true);

    builder.addRecord(completed("2026-07-30T12:08:32Z", "success"));
    const finalTimeline = builder.getTimeline();
    expect(finalTimeline.phases[1]?.ongoing).toBe(false);
    expect(finalTimeline.result).toBe("success");
  });
});
