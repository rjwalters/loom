/**
 * Success-rate trend chart (issue #4751, AC2) — "derived from the same
 * data" as the outcomes-over-time chart, per the acceptance criteria: this
 * module takes `OutcomeBucket[]` (from `outcomes.ts`) rather than
 * re-querying/re-correlating raw records, so the two charts are always
 * consistent with each other by construction.
 */

import type { OutcomeBucket } from "./outcomes.js";

export interface SuccessRatePoint {
  bucketKey: string;
  /** `success / total`, in `[0, 1]`. `null` when the bucket has zero
   * completed sweeps — a rate is undefined for an empty bucket, and a chart
   * should render a gap there rather than a misleading `0`. */
  successRate: number | null;
  total: number;
}

export function buildSuccessRateTrend(buckets: OutcomeBucket[]): SuccessRatePoint[] {
  return buckets.map((bucket) => ({
    bucketKey: bucket.bucketKey,
    successRate: bucket.total > 0 ? bucket.counts.success / bucket.total : null,
    total: bucket.total,
  }));
}
