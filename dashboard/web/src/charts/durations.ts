/**
 * Duration percentile chart (issue #4751, AC3): p50/p90/p99 for
 * `total_duration_sec`, plus a breakdown by phase from `phase_durations`.
 * Built on `correlateSweeps` (`total_duration_sec`/`phase_durations` only
 * ever appear on `sweep.outcome` records — see that module's doc).
 */

import type { HistoryRecord } from "../types.js";
import { correlateSweeps } from "./correlate.js";

export type PercentileRank = 50 | 90 | 99;

export const DEFAULT_PERCENTILES: readonly PercentileRank[] = [50, 90, 99] as const;

/** One entry per requested percentile rank, e.g. `{50: 12.5, 90: 40, 99:
 * 88}`. */
export type PercentileResult = Partial<Record<PercentileRank, number>>;

/**
 * Nearest-rank percentile over a (not-necessarily-sorted) set of values.
 * `values` must be non-empty; callers should skip percentile computation
 * entirely for an empty set rather than call this (see
 * `buildDurationPercentiles`, which does exactly that).
 *
 * Nearest-rank (as opposed to linear interpolation) is used because it
 * always returns an actually-observed duration value — useful for a metric
 * like sweep duration, where "the 90th-percentile sweep took this long" is a
 * more meaningful statement than an interpolated value between two
 * durations that never happened.
 */
export function computePercentiles(values: number[], ranks: readonly PercentileRank[] = DEFAULT_PERCENTILES): PercentileResult {
  if (values.length === 0) {
    throw new Error("computePercentiles: values must be non-empty");
  }
  const sorted = [...values].sort((a, b) => a - b);
  const result: PercentileResult = {};
  for (const rank of ranks) {
    // Nearest-rank: index = ceil(rank/100 * N) - 1, clamped into range.
    const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil((rank / 100) * sorted.length) - 1));
    result[rank] = sorted[index];
  }
  return result;
}

export interface DurationPercentiles {
  /** Percentiles over `total_duration_sec` across every correlated sweep
   * that has one. `undefined` when no sweep in the input has a known total
   * duration. */
  overall: PercentileResult | undefined;
  /** Percentiles over each phase's `duration_sec`, across every sweep that
   * recorded that phase — e.g. `{curator: {50: 12, ...}, builder: {50: 340,
   * ...}}`. A phase absent from every sweep in the input is simply absent
   * from this map. */
  byPhase: Record<string, PercentileResult>;
}

export function buildDurationPercentiles(
  records: HistoryRecord[],
  ranks: readonly PercentileRank[] = DEFAULT_PERCENTILES,
): DurationPercentiles {
  const sweeps = [...correlateSweeps(records).values()];

  const totalDurations = sweeps
    .map((sweep) => sweep.totalDurationSec)
    .filter((value): value is number => value !== undefined);

  const byPhaseDurations = new Map<string, number[]>();
  for (const sweep of sweeps) {
    for (const phaseDuration of sweep.phaseDurations) {
      const existing = byPhaseDurations.get(phaseDuration.phase);
      if (existing) {
        existing.push(phaseDuration.durationSec);
      } else {
        byPhaseDurations.set(phaseDuration.phase, [phaseDuration.durationSec]);
      }
    }
  }

  const byPhase: Record<string, PercentileResult> = {};
  for (const [phase, durations] of byPhaseDurations) {
    byPhase[phase] = computePercentiles(durations, ranks);
  }

  return {
    overall: totalDurations.length > 0 ? computePercentiles(totalDurations, ranks) : undefined,
    byPhase,
  };
}
