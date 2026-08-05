/**
 * Per-host `host.health` metric trend (issue #5355): one point per
 * `host.health` record for a single numeric field (`cpu_idle_fraction` or
 * `worktree_root_free_gb`), in chronological order.
 *
 * Each field is independently optional per the daemon's "unknown != zero"
 * contract (`.loom/docs/telemetry-schema.md`) — a probe that could not
 * measure a given field omits *that field*, while the record itself (and its
 * other fields) may still be present. A record missing the requested field
 * therefore contributes a `value: null` point (a gap) rather than being
 * dropped from the series entirely — `hostHealthTrendChartView.ts` renders a
 * gap for it, never a misleading zero.
 */

import type { HistoryRecord, TelemetryRecord } from "../types.js";

export type HealthTrendField = "cpu_idle_fraction" | "worktree_root_free_gb";

export interface HealthTrendPoint {
  emittedAt: string;
  /** `null` when this record's `field` was absent — a gap, never a zero. */
  value: number | null;
}

/** Mirrors `correlate.ts`'s `payloadOf`: `record.record` is verbatim ingested
 * JSON (an upper bound, not a guarantee — `/public/history` serves a
 * per-`kind` redaction subset), so every field is read defensively. */
function payloadOf(record: TelemetryRecord): Record<string, unknown> {
  return record as Record<string, unknown>;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * Build the trend series for `field` from a set of `HistoryRecord`s (any mix
 * of kinds; only `host.health` contributes). Returned in ascending
 * (chronological) `emittedAt` order — `fetchAllHistory` returns newest-first,
 * so callers passing its output straight through get it flipped here rather
 * than needing to remember to reverse it themselves.
 */
export function buildHealthMetricTrend(records: HistoryRecord[], field: HealthTrendField): HealthTrendPoint[] {
  const points: HealthTrendPoint[] = records
    .filter((record) => record.kind === "host.health")
    .map((record) => {
      const value = asNumber(payloadOf(record.record)[field]);
      return { emittedAt: record.emittedAt, value: value ?? null };
    });

  return points.sort((a, b) => (a.emittedAt < b.emittedAt ? -1 : a.emittedAt > b.emittedAt ? 1 : 0));
}
