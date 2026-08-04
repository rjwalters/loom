/**
 * Per-account burn curves: `usage_fraction` over time, reconstructed from
 * `tokens.snapshot` history (Epic #4702, Phase 3, issue #4752).
 *
 * ## Series identity is the `account`; `hostId` is provenance (#4898)
 *
 * This used to key on `(hostId, account)`, reasoning that two hosts could
 * legitimately hold same-named but distinct accounts, so pooling them would
 * interleave "two independent usage clocks". For a shared token pool that is
 * backwards: `usage_fraction` is the account's **server-side** consumption,
 * not the reporting host's contribution to it, so several hosts holding one
 * account are not two clocks — they are one clock read N times.
 *
 * Keying on the pair therefore rendered the same account once per host: on a
 * three-host fleet, 39 near-identical curves for 13 accounts, and one
 * exhaustion forecast per copy. Readings are now merged by `account`, and the
 * contributing hosts are carried as `hostIds` for provenance.
 *
 * The old concern is not dismissed, only relocated: nothing guarantees two
 * operators' `.loom/tokens/` use disjoint local names. That case is now
 * *detected* rather than assumed — see `divergentHosts` below.
 *
 * Merging is safe against ordering because `at` is the daemon's `captured_at`
 * (the probe instant), not the push time, so a merged series is still in true
 * chronological order and still monotonically non-decreasing within a window.
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
 *    float noise in a value the daemon computes as a ratio). While an account
 *    is healthy that reset tracks the same 5h window `usage_fraction` measures,
 *    so the two rollover signals agree; when an account goes `exhausted` the
 *    daemon switches it to the 7d instant (the one that says when the account
 *    comes back), which reads here as a jump to a later reset — a real regime
 *    change, and exactly where a new segment belongs.
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
 *
 * ## Pool-level curves (issue #4847)
 *
 * `buildPoolBurnCurves` (bottom of this file) is the public-surface
 * counterpart: one curve per `hostId` (no account key — the aggregate names
 * none), built from `/public/history`'s `mean_usage_fraction` /
 * `max_usage_fraction` / `exhausted_count` instead of a per-account
 * `usage_fraction`. Same segmentation rules, same "unknown != zero" contract,
 * different input shape (`PoolSample`, not `TokenSample`).
 */

import type { AccountReading, PoolSample, TokenSample } from "./types.js";

/**
 * How far usage must fall to count as a window rollover.
 *
 * Was `1e-9` — appropriate when a series came from one host, where the only
 * sub-threshold movement was float noise. A merged multi-host series (#4898)
 * also carries small legitimate wobble: hosts probe on independent schedules
 * and their clocks are not synchronised, so two readings of the same value can
 * land marginally out of order. At `1e-9` any such pair fabricated a
 * "window-reset" and shattered the curve into meaningless segments.
 *
 * A genuine rollover resets usage toward zero — a drop of most of the range,
 * never hundredths. `0.02` clears the observed cross-host spread (~0.01) with
 * room to spare while remaining far below any real reset. Rollovers from a
 * usage below this are not interesting: an account resetting from 0.01 to 0
 * has nothing to plot either way.
 */
const USAGE_DROP_EPSILON = 0.02;

/** Default telemetry-gap tolerance: samples more than 60 minutes apart start a
 * new segment. The daemon's snapshot cadence is minutes, so an hour-wide hole
 * is a reporting outage, not a slow poll. */
export const DEFAULT_MAX_SAMPLE_GAP_MS = 60 * 60 * 1000;

/**
 * Two readings closer together than this are "the same moment" for the
 * purpose of comparing hosts (`findDivergentHosts`). Comfortably under the
 * daemon's snapshot cadence, so it pairs cross-host observations without
 * pairing a host's own consecutive samples.
 */
const CONCURRENT_READING_WINDOW_MS = 90 * 1000;

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
  /** Every host that reported this account, first-seen order. Provenance,
   * not identity — the account is the thing with a quota (#4898). */
  hostIds: string[];
  /** Hosts whose readings disagreed materially with a near-simultaneous
   * reading from another host — i.e. the same local name plausibly refers to
   * different upstream accounts. Empty in the normal shared-pool case;
   * non-empty means the merge should not be trusted for this account. */
  divergentHosts: string[];
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
  account: string;
  /** Every host that reported this account, in first-seen order. */
  hostIds: string[];
  readings: Array<{ at: number; hostId: string; reading: AccountReading }>;
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
      let entry = series.get(reading.account);
      if (!entry) {
        entry = { account: reading.account, hostIds: [], readings: [] };
        series.set(reading.account, entry);
      }
      if (!entry.hostIds.includes(sample.hostId)) entry.hostIds.push(sample.hostId);
      entry.readings.push({ at: sample.at, hostId: sample.hostId, reading });
    }
  }

  const curves: AccountBurnCurve[] = [];
  for (const entry of series.values()) {
    // Stable across hosts: probe time, then host name, so two readings sharing
    // a `captured_at` second do not reorder between runs.
    entry.readings.sort((a, b) => a.at - b.at || a.hostId.localeCompare(b.hostId));
    curves.push(buildCurve(entry, maxSampleGapMs));
  }

  // Stable, operator-meaningful ordering: pool rank (the selector's own
  // preference order), then name. Rank-less accounts sort last. No longer
  // grouped by host — one curve now spans every host that reported it.
  curves.sort(
    (a, b) =>
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
    hostIds: entry.hostIds,
    divergentHosts: findDivergentHosts(entry),
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

/**
 * Hosts whose reading of this account contradicts another host's at nearly the
 * same instant.
 *
 * This is the safeguard for the case the old `(hostId, account)` keying
 * assumed was universal: two operators' pools can legitimately use one local
 * name for different upstream accounts, and merging those would average two
 * unrelated quotas into a line describing neither.
 *
 * Detected rather than assumed. For a genuinely shared account every host
 * observes the same server-side value, so near-simultaneous readings agree to
 * within probe timing. A disagreement wider than `USAGE_DROP_EPSILON` between
 * readings closer together than the probe cadence is not timing — it is two
 * different accounts.
 */
function findDivergentHosts(entry: Series): string[] {
  const divergent = new Set<string>();

  for (let i = 1; i < entry.readings.length; i += 1) {
    const previous = entry.readings[i - 1];
    const current = entry.readings[i];
    if (!previous || !current) continue;
    if (previous.hostId === current.hostId) continue;
    if (current.at - previous.at > CONCURRENT_READING_WINDOW_MS) continue;

    const a = previous.reading.usageFraction;
    const b = current.reading.usageFraction;
    if (a === undefined || b === undefined) continue;

    if (Math.abs(a - b) > USAGE_DROP_EPSILON) {
      divergent.add(previous.hostId);
      divergent.add(current.hostId);
    }
  }

  return [...divergent].sort();
}

function newestDefinedRank(entry: Series): number | undefined {
  for (let i = entry.readings.length - 1; i >= 0; i -= 1) {
    const rank = entry.readings[i]?.reading.rank;
    if (rank !== undefined) return rank;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Pool-level burn curves (`/public/history`'s aggregate — issue #4847)
// ---------------------------------------------------------------------------

/**
 * One reading of the pool aggregate, mirroring {@link BurnPoint} but keyed on
 * the whole pool rather than one account: `usage_fraction` becomes a pair
 * (mean/peak across every account that reported one) and there is an
 * `exhaustedCount`/`accountCount` instead of a boolean flag.
 */
export interface PoolBurnPoint {
  /** Epoch ms. */
  at: number;
  /** Mean/peak `usage_fraction` across the pool, clamped to `[0, 1]`. Absent
   * — never `0` — when no account reported one in this sample. */
  meanUsageFraction?: number;
  maxUsageFraction?: number;
  accountCount: number;
  exhaustedCount: number;
  /** Epoch ms of this reading's `next_limit_window_reset_at`, when known. */
  nextLimitWindowResetAt?: number;
}

export interface PoolBurnSegment {
  /** Chronological, at least one point. */
  points: PoolBurnPoint[];
  /** The `next_limit_window_reset_at` in force for this segment (the newest
   * one observed in it), when any reading reported one. */
  nextLimitWindowResetAt?: number;
  startedBy: "initial" | "window-reset" | "gap";
}

/** One host's token pool over time. There is one curve per `hostId` — unlike
 * {@link AccountBurnCurve} there is no per-account key, because the aggregate
 * carries no account identity to key on. */
export interface PoolBurnCurve {
  hostId: string;
  /** Every usable point across every segment, chronological. */
  points: PoolBurnPoint[];
  segments: PoolBurnSegment[];
  /** The live segment — the tail of `segments`, or `undefined` when no
   * sample in range reported a usage figure at all. */
  currentSegment?: PoolBurnSegment;
  /** Newest reading of any kind (including one with no usage figure). */
  latestAt: number;
  /** Pool size / exhausted count as of the newest reading. */
  accountCount: number;
  exhaustedCount: number;
}

interface PoolSeries {
  hostId: string;
  samples: PoolSample[];
}

/**
 * Build one pool burn curve per `hostId` from a chronological
 * `/public/history` page. Segmentation mirrors {@link buildBurnCurves}
 * exactly — a rollover (peak usage dropping, or the pool's next reset
 * advancing) or a telemetry gap starts a new segment — because the same
 * "sawtooth would wreck a trend read" problem applies to the pool's peak
 * usage as it does to any one account's.
 *
 * `samples` is expected oldest-first ({@link parsePoolSamples} guarantees
 * this); re-sorted defensively per host, same as {@link buildBurnCurves}.
 */
export function buildPoolBurnCurves(
  samples: readonly PoolSample[],
  options: BurnCurveOptions = {},
): PoolBurnCurve[] {
  const maxSampleGapMs = options.maxSampleGapMs ?? DEFAULT_MAX_SAMPLE_GAP_MS;

  const byHost = new Map<string, PoolSeries>();
  for (const sample of samples) {
    let entry = byHost.get(sample.hostId);
    if (!entry) {
      entry = { hostId: sample.hostId, samples: [] };
      byHost.set(sample.hostId, entry);
    }
    entry.samples.push(sample);
  }

  const curves: PoolBurnCurve[] = [];
  for (const entry of byHost.values()) {
    entry.samples.sort((a, b) => a.at - b.at);
    curves.push(buildPoolCurve(entry, maxSampleGapMs));
  }
  curves.sort((a, b) => a.hostId.localeCompare(b.hostId));
  return curves;
}

function buildPoolCurve(entry: PoolSeries, maxSampleGapMs: number): PoolBurnCurve {
  const segments: PoolBurnSegment[] = [];
  const points: PoolBurnPoint[] = [];
  let previous: PoolBurnPoint | undefined;

  for (const sample of entry.samples) {
    // A sample with no usage figure at all contributes no plottable point —
    // same "unknown != zero" rule as a per-account reading — but the loop
    // below still uses `entry.samples.at(-1)` (not `points.at(-1)`) for the
    // curve's latest account/exhausted counts, so that summary never depends
    // on whether the newest sample happened to carry a usage figure.
    if (sample.meanUsageFraction === undefined && sample.maxUsageFraction === undefined) continue;

    const point: PoolBurnPoint = {
      at: sample.at,
      meanUsageFraction: sample.meanUsageFraction === undefined ? undefined : clampFraction(sample.meanUsageFraction),
      maxUsageFraction: sample.maxUsageFraction === undefined ? undefined : clampFraction(sample.maxUsageFraction),
      accountCount: sample.accountCount,
      exhaustedCount: sample.exhaustedCount,
      nextLimitWindowResetAt: sample.nextLimitWindowResetAt,
    };
    points.push(point);

    const startedBy = poolSegmentBreak(previous, point, maxSampleGapMs);
    const current = segments.at(-1);
    if (startedBy === undefined && current) {
      current.points.push(point);
      if (point.nextLimitWindowResetAt !== undefined) current.nextLimitWindowResetAt = point.nextLimitWindowResetAt;
    } else {
      segments.push({
        points: [point],
        nextLimitWindowResetAt: point.nextLimitWindowResetAt,
        startedBy: startedBy ?? "initial",
      });
    }
    previous = point;
  }

  const lastSample = entry.samples.at(-1);
  return {
    hostId: entry.hostId,
    points,
    segments,
    currentSegment: segments.at(-1),
    latestAt: lastSample?.at ?? 0,
    accountCount: lastSample?.accountCount ?? 0,
    exhaustedCount: lastSample?.exhaustedCount ?? 0,
  };
}

/** `undefined` when `point` continues `previous`'s segment. Mirrors
 * {@link segmentBreak}, reading the pool's peak usage where that reads one
 * account's `usage_fraction`. */
function poolSegmentBreak(
  previous: PoolBurnPoint | undefined,
  point: PoolBurnPoint,
  maxSampleGapMs: number,
): PoolBurnSegment["startedBy"] | undefined {
  if (previous === undefined) return "initial";
  if (point.at - previous.at > maxSampleGapMs) return "gap";
  if (
    previous.maxUsageFraction !== undefined &&
    point.maxUsageFraction !== undefined &&
    previous.maxUsageFraction - point.maxUsageFraction > USAGE_DROP_EPSILON
  ) {
    return "window-reset";
  }
  if (
    previous.nextLimitWindowResetAt !== undefined &&
    point.nextLimitWindowResetAt !== undefined &&
    point.nextLimitWindowResetAt > previous.nextLimitWindowResetAt
  ) {
    return "window-reset";
  }
  return undefined;
}
