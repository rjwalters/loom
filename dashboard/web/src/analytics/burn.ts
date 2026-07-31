/**
 * Per-account burn curves: `usage_fraction` over time, reconstructed from
 * `tokens.snapshot` history (Epic #4702, Phase 3, issue #4752).
 *
 * ## Series identity is `hostId` + `account`, not `account` alone
 *
 * The token pool is a **per-host** resource (`.loom/tokens/` lives on the
 * machine running `loom-daemon`), and `tokens.snapshot` carries no repo but
 * does arrive under a `hostId` envelope. Two hosts can legitimately have
 * accounts with the same name — pooling them into one series would interleave
 * two independent usage clocks and produce a sawtooth that describes neither.
 * So one curve is one `(hostId, account)` pair.
 *
 * ## Limit windows, and why the curve is segmented
 *
 * `usage_fraction` is monotonically non-decreasing **within a limit window**
 * and drops back toward 0 when the window rolls over at
 * `limit_window_reset_at`. A single unsegmented series therefore contains
 * sawtooth discontinuities that would wreck any trend fit (`forecast.ts`) and
 * mislead the eye. `buildBurnCurves` splits each series into segments at:
 *
 * 1. **A window rollover** — `limit_window_reset_at` changed to a later
 *    instant, or usage fell (by more than `USAGE_DROP_EPSILON`, which absorbs
 *    float noise in a value the daemon computes as a ratio).
 * 2. **A telemetry gap** — consecutive samples further apart than
 *    `maxSampleGapMs`. The host was down, offline, or not reporting; whatever
 *    happened in between is unobserved, and joining across it would draw a
 *    straight line through data nobody collected.
 *
 * The **last** segment is the live one — the only one `forecast.ts` may
 * extrapolate from.
 *
 * ## Unknown is not zero
 *
 * A reading whose `usage_fraction` the daemon omitted contributes no point to
 * the curve (per the "unknown != zero" contract). It still contributes its
 * `exhausted` flag, which the schema documents as always present: an account
 * can be known-exhausted while its numeric usage is unknown, and that must
 * still light up the UI.
 */

import type { AccountReading, TokenSample } from "./types.js";

/** A usage drop smaller than this is float noise, not a window rollover. */
const USAGE_DROP_EPSILON = 1e-9;

/** Default telemetry-gap tolerance: samples more than 60 minutes apart start a
 * new segment. The daemon's snapshot cadence is minutes, so an hour-wide hole
 * is a reporting outage, not a slow poll. */
export const DEFAULT_MAX_SAMPLE_GAP_MS = 60 * 60 * 1000;

export interface BurnPoint {
  /** Epoch ms. */
  at: number;
  /** `usage_fraction` as reported, clamped to `[0, 1]`. */
  usageFraction: number;
  exhausted: boolean;
  /** Epoch ms of this reading's `limit_window_reset_at`, when known. */
  limitWindowResetAt?: number;
}

export interface BurnSegment {
  /** Chronological, at least one point. */
  points: BurnPoint[];
  /** The `limit_window_reset_at` in force for this segment (the newest one
   * observed in it), when any reading reported one. */
  limitWindowResetAt?: number;
  /** Why this segment starts where it does — `"initial"` for the first,
   * `"window-reset"` for a rollover, `"gap"` for a telemetry hole. */
  startedBy: "initial" | "window-reset" | "gap";
}

export interface AccountBurnCurve {
  hostId: string;
  account: string;
  /** Newest known pool rank (lower = preferred by the selector). */
  rank?: number;
  /** Every usable point across every segment, chronological. */
  points: BurnPoint[];
  segments: BurnSegment[];
  /** The live segment — the tail of `segments`, or `undefined` when the
   * account reported no numeric usage at all in the window queried. */
  currentSegment?: BurnSegment;
  /** Newest reading of any kind (including one with unknown usage). */
  latestAt: number;
  /** `exhausted` as of the newest reading. */
  exhausted: boolean;
  /** `exhausted` on any reading in range — an account that was exhausted and
   * has since rolled over is still worth flagging in a fleet view. */
  everExhausted: boolean;
}

export interface BurnCurveOptions {
  /** See `DEFAULT_MAX_SAMPLE_GAP_MS`. */
  maxSampleGapMs?: number;
}

function clampFraction(value: number): number {
  if (value < 0) return 0;
  if (value > 1) return 1;
  return value;
}

interface Series {
  hostId: string;
  account: string;
  readings: Array<{ at: number; reading: AccountReading }>;
}

/**
 * Build one burn curve per `(hostId, account)` pair from a chronological
 * `tokens.snapshot` history.
 *
 * `samples` is expected oldest-first (`parse.ts`'s `parseTokenSamples`
 * guarantees this); it is re-sorted defensively per series anyway so a caller
 * passing raw newest-first API order still gets correct curves rather than a
 * silently reversed one.
 */
export function buildBurnCurves(
  samples: readonly TokenSample[],
  options: BurnCurveOptions = {},
): AccountBurnCurve[] {
  const maxSampleGapMs = options.maxSampleGapMs ?? DEFAULT_MAX_SAMPLE_GAP_MS;

  const series = new Map<string, Series>();
  for (const sample of samples) {
    for (const reading of sample.accounts) {
      const key = `${sample.hostId}\u0000${reading.account}`;
      let entry = series.get(key);
      if (!entry) {
        entry = { hostId: sample.hostId, account: reading.account, readings: [] };
        series.set(key, entry);
      }
      entry.readings.push({ at: sample.at, reading });
    }
  }

  const curves: AccountBurnCurve[] = [];
  for (const entry of series.values()) {
    entry.readings.sort((a, b) => a.at - b.at);
    curves.push(buildCurve(entry, maxSampleGapMs));
  }

  // Stable, operator-meaningful ordering: host, then pool rank (the selector's
  // own preference order), then name. Rank-less accounts sort last.
  curves.sort(
    (a, b) =>
      a.hostId.localeCompare(b.hostId) ||
      (a.rank ?? Number.MAX_SAFE_INTEGER) - (b.rank ?? Number.MAX_SAFE_INTEGER) ||
      a.account.localeCompare(b.account),
  );
  return curves;
}

function buildCurve(entry: Series, maxSampleGapMs: number): AccountBurnCurve {
  const segments: BurnSegment[] = [];
  const points: BurnPoint[] = [];
  let previous: BurnPoint | undefined;
  let everExhausted = false;

  for (const { at, reading } of entry.readings) {
    if (reading.exhausted) everExhausted = true;
    if (reading.usageFraction === undefined) continue;

    const point: BurnPoint = {
      at,
      usageFraction: clampFraction(reading.usageFraction),
      exhausted: reading.exhausted,
      limitWindowResetAt: reading.limitWindowResetAt,
    };
    points.push(point);

    const startedBy = segmentBreak(previous, point, maxSampleGapMs);
    const current = segments.at(-1);
    if (startedBy === undefined && current) {
      current.points.push(point);
      if (point.limitWindowResetAt !== undefined) current.limitWindowResetAt = point.limitWindowResetAt;
    } else {
      segments.push({
        points: [point],
        limitWindowResetAt: point.limitWindowResetAt,
        startedBy: startedBy ?? "initial",
      });
    }
    previous = point;
  }

  const lastReading = entry.readings.at(-1);
  return {
    hostId: entry.hostId,
    account: entry.account,
    rank: newestDefinedRank(entry),
    points,
    segments,
    currentSegment: segments.at(-1),
    latestAt: lastReading?.at ?? 0,
    exhausted: lastReading?.reading.exhausted ?? false,
    everExhausted,
  };
}

/** `undefined` when `point` continues `previous`'s segment. */
function segmentBreak(
  previous: BurnPoint | undefined,
  point: BurnPoint,
  maxSampleGapMs: number,
): BurnSegment["startedBy"] | undefined {
  if (previous === undefined) return "initial";
  if (point.at - previous.at > maxSampleGapMs) return "gap";
  if (previous.usageFraction - point.usageFraction > USAGE_DROP_EPSILON) return "window-reset";
  if (
    previous.limitWindowResetAt !== undefined &&
    point.limitWindowResetAt !== undefined &&
    point.limitWindowResetAt > previous.limitWindowResetAt
  ) {
    return "window-reset";
  }
  return undefined;
}

function newestDefinedRank(entry: Series): number | undefined {
  for (let i = entry.readings.length - 1; i >= 0; i -= 1) {
    const rank = entry.readings[i]?.reading.rank;
    if (rank !== undefined) return rank;
  }
  return undefined;
}
