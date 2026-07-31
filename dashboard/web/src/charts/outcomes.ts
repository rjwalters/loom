/**
 * Outcomes-over-time chart data (issue #4751, AC1): count of sweeps by
 * `result`, bucketed by time. Built on `correlateSweeps` — a sweep only
 * contributes to a bucket once it has a known `result` (i.e. once its
 * `sweep.completed` or `sweep.outcome` record has been observed); a sweep
 * still in flight (no terminal record yet) is simply absent, not counted as
 * any result.
 */

import type { HistoryRecord, SweepResult } from "../types.js";
import { correlateSweeps } from "./correlate.js";
import { type BucketGranularity, groupByTimeBucket } from "./timeBuckets.js";

export const SWEEP_RESULTS: readonly SweepResult[] = ["success", "failure", "cancelled", "blocked"] as const;

export interface OutcomeBucket {
  bucketKey: string;
  counts: Record<SweepResult, number>;
  total: number;
}

function emptyCounts(): Record<SweepResult, number> {
  return { success: 0, failure: 0, cancelled: 0, blocked: 0 };
}

/**
 * Build the outcomes-over-time series from a set of `HistoryRecord`s (any
 * mix of kinds; only `sweep.completed`/`sweep.outcome` contribute). Buckets
 * are returned in ascending (chronological) key order; a time range with no
 * completed sweeps produces an empty array, not a zero-filled range —
 * dense/zero-filled axes are a rendering-layer concern once real chart
 * components exist (blocked on the frontend scaffold, issue #4749).
 */
export function buildOutcomesOverTime(
  records: HistoryRecord[],
  granularity: BucketGranularity = "daily",
): OutcomeBucket[] {
  const sweeps = [...correlateSweeps(records).values()].filter((sweep) => sweep.result !== undefined);
  const buckets = groupByTimeBucket(sweeps, (sweep) => sweep.emittedAt, granularity);

  const result: OutcomeBucket[] = [];
  for (const [key, bucketSweeps] of buckets) {
    const counts = emptyCounts();
    for (const sweep of bucketSweeps) {
      // Guaranteed defined by the filter above; narrow for the type checker.
      const outcomeResult = sweep.result;
      if (outcomeResult) counts[outcomeResult] += 1;
    }
    result.push({ bucketKey: key, counts, total: bucketSweeps.length });
  }
  return result;
}
