/**
 * Rolling token-pool burn-rate ("throughput") derivation (issue #9029).
 *
 * `forecast.ts`'s `slopePerHour` already fits a burn rate, but only as an
 * input to an exhaustion ETA — it is never surfaced as its own time series,
 * and it fits the *whole* live segment at once (an ever-widening, effectively
 * unbounded window as a segment ages). This module answers a narrower,
 * complementary question: "how fast is the pool burning *right now*", as a
 * plottable series, from a small bounded trailing window rather than the
 * whole segment.
 *
 * ## Bounded, not accumulating
 *
 * For each point, the rate looks back at most {@link BurnRateOptions.windowHours}
 * (default {@link DEFAULT_BURN_RATE_WINDOW_HOURS}) within the *same burn
 * segment* for a reference point, and reports `(current - reference) /
 * hoursElapsed`. The window pointer only ever advances forward across a
 * curve's points (a classic sliding window), so computing the whole series is
 * O(n) with no unbounded in-memory accumulation — consistent with the
 * bounding pattern `historicalChartsPanel.ts` uses for its own window
 * (`DEFAULT_WINDOW_DAYS` / `CHART_PAGE_SIZE`).
 *
 * ## Why per-segment, never across one
 *
 * Processing `PoolBurnCurve.segments` independently — never flattening
 * `curve.points` and sliding across the whole thing — means a limit-window
 * rollover or a telemetry gap (`burn.ts`'s segmentation) can never appear as
 * a reference point for a rate computed after it. The first points of every
 * segment therefore report `null` (no rate — an unmeasured trend is
 * "unknown", not "0" or, worse, a fabricated cliff), exactly as intended: a
 * rollover reads as a gap in the rate line, never as a huge negative spike.
 */

import type { PoolBurnCurve, PoolBurnPoint } from "./burn.js";

/** Trailing window, in hours, a rate is computed over. Small enough to react
 * to a real change in burn behaviour within the hour, large enough that the
 * daemon's minutes-scale snapshot cadence (`burn.ts`'s
 * `DEFAULT_MAX_SAMPLE_GAP_MS` doc) gives it several points to average across
 * rather than reacting to one noisy pair. */
export const DEFAULT_BURN_RATE_WINDOW_HOURS = 1;

const MS_PER_HOUR = 60 * 60 * 1000;

export interface BurnRatePoint {
  /** Epoch ms. */
  at: number;
  /** Change in `meanUsageFraction` per hour over the trailing window ending
   * at `at`, as a fraction (multiply by 100 for `%/h` — see
   * `format.ts`'s `formatRatePerHour`). `null` when the window contains no
   * earlier point in this segment, or either endpoint's mean usage is
   * unmeasured — a rate is unknown there, never a fabricated `0`. */
  meanRatePerHour: number | null;
  /** Same, for `maxUsageFraction`. */
  maxRatePerHour: number | null;
}

export interface BurnRateOptions {
  /** See {@link DEFAULT_BURN_RATE_WINDOW_HOURS}. */
  windowHours?: number;
}

/**
 * Build the bounded rolling-window burn-rate series for one pool curve.
 * Returns one {@link BurnRatePoint} per point in `curve.points`, in the same
 * chronological order (segment boundaries do not remove points, only cap how
 * far back a rate may look).
 */
export function buildPoolBurnRateSeries(curve: PoolBurnCurve, options: BurnRateOptions = {}): BurnRatePoint[] {
  const windowMs = (options.windowHours ?? DEFAULT_BURN_RATE_WINDOW_HOURS) * MS_PER_HOUR;
  const out: BurnRatePoint[] = [];
  for (const segment of curve.segments) {
    out.push(...rateForSegment(segment.points, windowMs));
  }
  return out;
}

function rateForSegment(points: readonly PoolBurnPoint[], windowMs: number): BurnRatePoint[] {
  const out: BurnRatePoint[] = [];
  let refIndex = 0;
  for (let i = 0; i < points.length; i += 1) {
    const point = points[i]!;
    // Advance the window's left edge forward — never backward — past any
    // point older than the trailing window. Monotonic across the loop, so
    // the whole segment costs O(n) regardless of window size.
    while (points[refIndex]!.at < point.at - windowMs) refIndex += 1;

    const ref = points[refIndex]!;
    const hours = (point.at - ref.at) / MS_PER_HOUR;
    out.push({
      at: point.at,
      meanRatePerHour: rateBetween(ref.meanUsageFraction, point.meanUsageFraction, hours),
      maxRatePerHour: rateBetween(ref.maxUsageFraction, point.maxUsageFraction, hours),
    });
  }
  return out;
}

/** `null` (never `0` or `NaN`) when either endpoint is unmeasured, or when
 * `ref` and `point` are the same reading (no elapsed time to divide by — the
 * first point of a segment, where the window's only candidate reference is
 * itself). */
function rateBetween(from: number | undefined, to: number | undefined, hours: number): number | null {
  if (from === undefined || to === undefined || !(hours > 0)) return null;
  return (to - from) / hours;
}
