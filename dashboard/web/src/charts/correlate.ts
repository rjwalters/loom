/**
 * Correlates `sweep.completed` and `sweep.outcome` records by `sweepId` into
 * one merged view per sweep — the join issue #4751's implementation
 * guidance calls for: `result` is authoritative on `sweep.completed` (the
 * terminal-state event), while `phase_durations`/`total_duration_sec`/
 * `model` only ever appear on `sweep.outcome` (see
 * `.loom/docs/telemetry-schema.md`'s two record shapes). Both charts below
 * (outcomes-over-time/success-rate and duration percentiles) build on this
 * single merged representation rather than re-deriving the join themselves.
 */

import type { HistoryRecord, SweepResult, TelemetryRecord } from "../types.js";
import { isSweepResult } from "../types.js";

/** One phase's duration on a correlated sweep — the camelCased chart-layer
 * counterpart of the wire-shape `SweepPhaseDuration` in `types.ts` (whose
 * `duration_sec` is the verbatim ingested field name). */
export interface PhaseDuration {
  phase: string;
  durationSec: number;
}

export interface CorrelatedSweep {
  sweepId: string;
  /** The canonical event time for this sweep, used for time-bucketing:
   * `sweep.completed`'s `emittedAt` when present (the terminal-state
   * moment), falling back to `sweep.outcome`'s otherwise. */
  emittedAt: string;
  result?: SweepResult;
  model?: string;
  totalDurationSec?: number;
  phaseDurations: PhaseDuration[];
}

interface MutableCorrelatedSweep extends CorrelatedSweep {
  /** Tracks whether `emittedAt` was set from a `sweep.completed` record yet
   * (so a later `sweep.outcome` record for the same sweep, processed after
   * the `sweep.completed`, never clobbers it — see `mergeSweepRecord`). */
  emittedAtFromCompleted: boolean;
}

/**
 * Read a record's `record` payload as an opaque key/value bag.
 *
 * `HistoryRecord.record` is typed as the `TelemetryRecord` discriminated
 * union, but `/public/history` serves *redacted* payloads whose fields are a
 * per-`kind` allowlist subset (see `dashboard/src/redaction.ts`), so the
 * declared shape is an upper bound, not a guarantee. Every field below is
 * therefore read defensively through `asString`/`asNumber` rather than off
 * the union — which is also why this correlation works identically against
 * `/api/history` and `/public/history`.
 */
function payloadOf(record: TelemetryRecord): Record<string, unknown> {
  return record as Record<string, unknown>;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function parsePhaseDurations(value: unknown): PhaseDuration[] {
  if (!Array.isArray(value)) return [];
  const result: PhaseDuration[] = [];
  for (const entry of value) {
    if (entry && typeof entry === "object") {
      const phase = asString((entry as Record<string, unknown>).phase);
      const durationSec = asNumber((entry as Record<string, unknown>).duration_sec);
      if (phase !== undefined && durationSec !== undefined) {
        result.push({ phase, durationSec });
      }
    }
  }
  return result;
}

/**
 * Merge a batch of `HistoryRecord`s (any mix of kinds — non-sweep records
 * are ignored) into one `CorrelatedSweep` per distinct `sweepId`.
 *
 * Idempotent and order-independent: calling this once over the full
 * paginated set (from `fetchAllHistory`) or incrementally over successive
 * pages and merging the resulting maps produces the same result, since each
 * field is only ever set once matching data for it is seen and later,
 * duplicate observations of the same field are no-ops (see
 * `mergeInto`) — records missing `sweepId` (should not happen for these two
 * kinds per the schema, but redaction never nulls it since `sweepId` is only
 * omitted for `visibility: "private"` records, which drop the whole
 * correlatable identity) are skipped.
 */
export function correlateSweeps(records: HistoryRecord[]): Map<string, CorrelatedSweep> {
  const bySweepId = new Map<string, MutableCorrelatedSweep>();

  for (const record of records) {
    if (record.kind !== "sweep.completed" && record.kind !== "sweep.outcome") continue;
    if (!record.sweepId) continue;

    let entry = bySweepId.get(record.sweepId);
    if (!entry) {
      entry = {
        sweepId: record.sweepId,
        emittedAt: record.emittedAt,
        phaseDurations: [],
        emittedAtFromCompleted: false,
      };
      bySweepId.set(record.sweepId, entry);
    }

    mergeInto(entry, record);
  }

  // Strip the internal-only tracking field before handing back to callers.
  const result = new Map<string, CorrelatedSweep>();
  for (const [sweepId, entry] of bySweepId) {
    const { emittedAtFromCompleted: _unused, ...rest } = entry;
    result.set(sweepId, rest);
  }
  return result;
}

function mergeInto(entry: MutableCorrelatedSweep, record: HistoryRecord): void {
  const payload = payloadOf(record.record);

  if (record.kind === "sweep.completed") {
    // sweep.completed is the terminal-state moment; always trust its
    // emittedAt/result as authoritative once seen.
    entry.emittedAt = record.emittedAt;
    entry.emittedAtFromCompleted = true;
    const result = asString(payload.result);
    if (isSweepResult(result)) entry.result = result;
  } else {
    // sweep.outcome: only use its emittedAt as a fallback (never overwrite
    // an already-seen sweep.completed timestamp), but its model/duration
    // fields have no other source.
    if (!entry.emittedAtFromCompleted) entry.emittedAt = record.emittedAt;
    if (entry.result === undefined) {
      const result = asString(payload.result);
      if (isSweepResult(result)) entry.result = result;
    }
    const model = asString(payload.model);
    if (model !== undefined) entry.model = model;
    const totalDurationSec = asNumber(payload.total_duration_sec);
    if (totalDurationSec !== undefined) entry.totalDurationSec = totalDurationSec;
    const phaseDurations = parsePhaseDurations(payload.phase_durations);
    if (phaseDurations.length > 0) entry.phaseDurations = phaseDurations;
  }
}
