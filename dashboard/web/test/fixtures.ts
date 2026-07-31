import type { HistoryRecord } from "../src/types.js";

let nextId = 1;

/** Reset the fixture id counter between tests that care about exact `id`
 * values (most don't). */
export function resetFixtureIds(): void {
  nextId = 1;
}

export function makeSweepCompleted(overrides: {
  sweepId: string;
  emittedAt: string;
  result: string;
  repo?: string;
  hostId?: string;
  id?: number;
}): HistoryRecord {
  return {
    id: overrides.id ?? nextId++,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: overrides.hostId ?? "host-a",
    kind: "sweep.completed",
    repo: overrides.repo ?? "rjwalters/loom",
    visibility: "public",
    issue: 1000,
    sweepId: overrides.sweepId,
    ingestedAt: overrides.emittedAt,
    record: {
      kind: "sweep.completed",
      repo: overrides.repo ?? "rjwalters/loom",
      visibility: "public",
      issue: 1000,
      sweep_id: overrides.sweepId,
      completed_at: overrides.emittedAt,
      result: overrides.result,
    },
  };
}

export function makeSweepOutcome(overrides: {
  sweepId: string;
  emittedAt: string;
  result?: string;
  model?: string;
  totalDurationSec?: number;
  phaseDurations?: { phase: string; duration_sec: number }[];
  repo?: string;
  hostId?: string;
  id?: number;
}): HistoryRecord {
  return {
    id: overrides.id ?? nextId++,
    schemaVersion: 1,
    emittedAt: overrides.emittedAt,
    hostId: overrides.hostId ?? "host-a",
    kind: "sweep.outcome",
    repo: overrides.repo ?? "rjwalters/loom",
    visibility: "public",
    issue: 1000,
    sweepId: overrides.sweepId,
    ingestedAt: overrides.emittedAt,
    record: {
      kind: "sweep.outcome",
      repo: overrides.repo ?? "rjwalters/loom",
      visibility: "public",
      issue: 1000,
      sweep_id: overrides.sweepId,
      model: overrides.model,
      phase_durations: overrides.phaseDurations ?? [],
      total_duration_sec: overrides.totalDurationSec,
      result: overrides.result,
    },
  };
}

/** A full "sweep" fixture: paired `sweep.completed` + `sweep.outcome`
 * records for one sweepId, the common case charts correlate. */
export function makeCompletedSweepPair(args: {
  sweepId: string;
  emittedAt: string;
  result: string;
  model: string;
  totalDurationSec: number;
  phaseDurations: { phase: string; duration_sec: number }[];
  repo?: string;
  hostId?: string;
}): HistoryRecord[] {
  return [
    makeSweepOutcome({
      sweepId: args.sweepId,
      emittedAt: args.emittedAt,
      result: args.result,
      model: args.model,
      totalDurationSec: args.totalDurationSec,
      phaseDurations: args.phaseDurations,
      repo: args.repo,
      hostId: args.hostId,
    }),
    makeSweepCompleted({
      sweepId: args.sweepId,
      emittedAt: args.emittedAt,
      result: args.result,
      repo: args.repo,
      hostId: args.hostId,
    }),
  ];
}
