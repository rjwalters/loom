/**
 * Per-sweep timeline aggregation: turns a sequence of telemetry records for
 * one `sweep_id` into the phase-progression view the timeline component
 * renders (issue #4750, Epic #4702 Phase 3).
 *
 * Two data sources feed a timeline, and either is sufficient on its own
 * (per the issue's "Implementation Guidance"):
 *
 * - `sweep.phase` records streamed live (or backfilled via
 *   `GET /api/history?...`), from which per-phase durations are computed
 *   client-side as the gap between consecutive `entered_at` timestamps.
 * - `sweep.outcome`'s `phase_durations` — authoritative, server-computed
 *   durations recorded once a sweep finishes (see
 *   `.loom/docs/telemetry-schema.md`). When present, these supersede the
 *   client-computed durations for phases that already ran.
 */

import type {
  SweepCompletedRecord,
  SweepOutcomeRecord,
  SweepPhaseRecord,
  SweepResult,
  SweepStartedRecord,
  TelemetryRecord,
} from "./types.js";

export interface TimelinePhaseEntry {
  phase: string;
  enteredAt: string;
  /** Seconds spent in this phase, or `undefined` while still ongoing. */
  durationSec?: number;
  /** True only for the most recent phase of a sweep that hasn't completed. */
  ongoing: boolean;
}

export interface SweepTimeline {
  sweepId: string;
  phases: TimelinePhaseEntry[];
  result?: SweepResult;
  prNumber?: number;
  totalDurationSec?: number;
  model?: string;
  effort?: string;
}

function toEpochMs(rfc3339: string): number {
  return new Date(rfc3339).getTime();
}

/**
 * Accumulates records for a single sweep and produces its timeline view.
 * Feed it records in any order (it sorts `sweep.phase` by `entered_at`);
 * call `getTimeline()` at any point for the current best-known view.
 */
export class SweepTimelineBuilder {
  readonly sweepId: string;
  private readonly phaseRecords: SweepPhaseRecord[] = [];
  private started: SweepStartedRecord | undefined;
  private completed: SweepCompletedRecord | undefined;
  private outcome: SweepOutcomeRecord | undefined;

  constructor(sweepId: string) {
    this.sweepId = sweepId;
  }

  /** Ignores records for a different `sweep_id` (or with none at all). */
  addRecord(record: TelemetryRecord): void {
    const sweepId = (record as { sweep_id?: unknown }).sweep_id;
    if (sweepId !== this.sweepId) return;

    switch (record.kind) {
      case "sweep.started":
        this.started = record as SweepStartedRecord;
        break;
      case "sweep.phase":
        this.phaseRecords.push(record as SweepPhaseRecord);
        break;
      case "sweep.completed":
        this.completed = record as SweepCompletedRecord;
        break;
      case "sweep.outcome":
        this.outcome = record as SweepOutcomeRecord;
        break;
      default:
        break;
    }
  }

  addRecords(records: Iterable<TelemetryRecord>): void {
    for (const record of records) this.addRecord(record);
  }

  /** Phase records sorted by `entered_at`, oldest first (stable on ties). */
  private sortedPhases(): SweepPhaseRecord[] {
    return [...this.phaseRecords].sort(
      (a, b) => toEpochMs(a.entered_at) - toEpochMs(b.entered_at),
    );
  }

  getTimeline(): SweepTimeline {
    const phases = this.sortedPhases();

    // Authoritative post-hoc durations, when available, take precedence —
    // matched to the live-streamed phase entries positionally (a phase can
    // legitimately repeat, e.g. the judge<->doctor cycle, so entries are
    // matched by order, not deduped by name).
    const outcomeDurations = this.outcome?.phase_durations;

    const entries: TimelinePhaseEntry[] = phases.map((phaseRecord, index) => {
      const nextPhase = phases[index + 1];
      const isLast = index === phases.length - 1;

      let durationSec: number | undefined;
      const outcomeEntry = outcomeDurations?.[index];
      if (outcomeEntry) {
        durationSec = outcomeEntry.duration_sec;
      } else if (nextPhase) {
        durationSec = (toEpochMs(nextPhase.entered_at) - toEpochMs(phaseRecord.entered_at)) / 1000;
      } else if (this.completed) {
        durationSec =
          (toEpochMs(this.completed.completed_at) - toEpochMs(phaseRecord.entered_at)) / 1000;
      } else if (this.outcome) {
        // Sweep finished (we have an outcome) but no per-phase duration was
        // recorded for the trailing segment — leave it undefined rather
        // than fabricating a value (mirrors the daemon's own "unattributed
        // trailing segment" behavior documented in the telemetry schema).
        durationSec = undefined;
      }

      return {
        phase: phaseRecord.phase,
        enteredAt: phaseRecord.entered_at,
        durationSec,
        ongoing: isLast && durationSec === undefined && !this.completed && !this.outcome,
      };
    });

    const result = this.outcome?.result ?? this.completed?.result;

    return {
      sweepId: this.sweepId,
      phases: entries,
      result,
      prNumber: this.outcome?.pr_number,
      totalDurationSec: this.outcome?.total_duration_sec,
      model: this.outcome?.model ?? this.started?.model,
      effort: this.outcome?.effort ?? this.started?.effort,
    };
  }
}

/** Convenience: build a timeline from a flat, unordered record list. */
export function buildSweepTimeline(sweepId: string, records: TelemetryRecord[]): SweepTimeline {
  const builder = new SweepTimelineBuilder(sweepId);
  builder.addRecords(records);
  return builder.getTimeline();
}
