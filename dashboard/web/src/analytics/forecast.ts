/**
 * Limit-window forecasting: will this account burn through its quota before
 * its window resets? (Epic #4702, Phase 3, issue #4752.)
 *
 * ## Method: ordinary least squares over the live segment only
 *
 * The forecast fits a straight line to `usage_fraction` vs. time across the
 * points of the **current** burn segment (`burn.ts` — the run since the last
 * window rollover or telemetry gap) and extrapolates it to `usage_fraction =
 * 1`. Deliberately the simplest defensible model:
 *
 * - **Least squares, not last-two-points.** A single noisy sample would
 *   otherwise swing the projection wildly; the fit averages the run.
 * - **Current segment only.** Fitting across a rollover would average a
 *   descending sawtooth edge into the trend and under-predict every time.
 * - **No smoothing/decay parameters.** Nothing here is tuned; there is no
 *   hidden constant an operator would have to learn about to read the number.
 *   If the trend is not linear, the honest output is a rough ETA, not a
 *   confident wrong one — which is why the UI renders this as an ETA band,
 *   never as a promise.
 *
 * ## The verdict
 *
 * The projected exhaustion instant is compared against the window's own
 * `limit_window_reset_at`. Whichever comes first decides the status:
 * `resets-first` (the quota survives the window — the healthy case) or
 * `projected-exhaustion` (the account runs dry first, and `marginSec` says by
 * how much). An account the daemon already reports as `exhausted` short-
 * circuits to `exhausted` — a measured fact always outranks a projection.
 *
 * ## Unknowns stay unknown
 *
 * Fewer than two points in the live segment yields `insufficient-data`, not a
 * guess. A non-positive slope yields `flat` — no burn is observable, so no
 * exhaustion is projected — rather than an "infinity" ETA. A missing
 * `limit_window_reset_at` yields a projection with an undefined margin rather
 * than an invented window length.
 */

import type { AccountBurnCurve, BurnPoint, PoolBurnCurve } from "./burn.js";

export type ForecastStatus =
  /** The daemon reports the account as exhausted right now. */
  | "exhausted"
  /** Projected to hit `usage_fraction = 1` before the window resets. */
  | "projected-exhaustion"
  /** The limit window resets before the projection reaches exhaustion. */
  | "resets-first"
  /** No measurable burn (flat or decreasing trend). */
  | "flat"
  /** Fewer than two usable points in the live segment. */
  | "insufficient-data";

export interface AccountForecast {
  /** Hosts that reported the account this forecast covers (#4898). */
  hostIds: string[];
  account: string;
  status: ForecastStatus;
  /** Points that fed the fit (live segment only). */
  sampleCount: number;
  /** Fitted burn rate, `usage_fraction` per hour. Present whenever the fit ran
   * (including a non-positive slope, so the UI can show "0.00/h"). */
  slopePerHour?: number;
  /** Newest observed `usage_fraction` in the live segment. */
  latestUsageFraction?: number;
  /** Epoch ms at which the fitted line reaches `usage_fraction = 1`. Never in
   * the past: a line already past 1 is reported as "now". */
  projectedExhaustionAt?: number;
  /** Seconds from `now` to `projectedExhaustionAt`. */
  secondsUntilExhaustion?: number;
  /** Epoch ms of the window rollover this account is racing. */
  limitWindowResetAt?: number;
  /** Seconds from `now` to `limitWindowResetAt`. Negative when the reported
   * reset instant is already in the past (a stale snapshot). */
  secondsUntilReset?: number;
  /** `limitWindowResetAt - projectedExhaustionAt`, in seconds. Positive means
   * the window resets with room to spare; negative means the account runs dry
   * that many seconds before relief arrives. */
  marginSec?: number;
}

export interface ForecastOptions {
  /** Reference instant (epoch ms). Injectable so tests are deterministic. */
  now?: number;
}

const MS_PER_HOUR = 60 * 60 * 1000;

/** Forecast one account from its burn curve. */
export function forecastAccount(curve: AccountBurnCurve, options: ForecastOptions = {}): AccountForecast {
  const now = options.now ?? Date.now();
  const segment = curve.currentSegment;
  const points = segment?.points ?? [];
  const latest: BurnPoint | undefined = points[points.length - 1];
  const limitWindowResetAt = segment?.limitWindowResetAt ?? latest?.limitWindowResetAt;

  const base: AccountForecast = {
    hostIds: curve.hostIds,
    account: curve.account,
    status: "insufficient-data",
    sampleCount: points.length,
    latestUsageFraction: latest?.usageFraction,
    limitWindowResetAt,
    secondsUntilReset:
      limitWindowResetAt === undefined ? undefined : Math.round((limitWindowResetAt - now) / 1000),
  };

  // A measured fact outranks any projection: if the pool says the account is
  // exhausted, that is the status regardless of what the trend suggests.
  if (curve.exhausted) return { ...base, status: "exhausted" };

  if (points.length < 2) return base;

  const fit = leastSquares(points);
  const slopePerHour = fit.slopePerMs * MS_PER_HOUR;
  const withFit: AccountForecast = { ...base, slopePerHour };

  if (!(fit.slopePerMs > 0)) return { ...withFit, status: "flat" };

  // Extrapolate the fitted line — not the raw last reading — to
  // `usage_fraction = 1`. For a two-point segment the fit passes exactly
  // through both points, so this degenerates to the obvious answer.
  const rawExhaustionAt = (1 - fit.intercept) / fit.slopePerMs;
  const projectedExhaustionAt = Math.max(Math.round(rawExhaustionAt), now);
  const secondsUntilExhaustion = Math.round((projectedExhaustionAt - now) / 1000);
  const marginSec =
    limitWindowResetAt === undefined
      ? undefined
      : Math.round((limitWindowResetAt - projectedExhaustionAt) / 1000);

  const status: ForecastStatus =
    limitWindowResetAt !== undefined && limitWindowResetAt <= projectedExhaustionAt
      ? "resets-first"
      : "projected-exhaustion";

  return { ...withFit, status, projectedExhaustionAt, secondsUntilExhaustion, marginSec };
}

/** Forecast every curve, preserving `buildBurnCurves`' ordering. */
export function forecastAccounts(
  curves: readonly AccountBurnCurve[],
  options: ForecastOptions = {},
): AccountForecast[] {
  return curves.map((curve) => forecastAccount(curve, options));
}

interface LinearFit {
  /** `usage_fraction` per millisecond. */
  slopePerMs: number;
  /** `usage_fraction` at epoch 0, in the same units the caller passes in. */
  intercept: number;
}

/**
 * Ordinary least squares of `usageFraction` on `at`.
 *
 * The fit is computed against time **relative to the first point** and the
 * intercept translated back to absolute epoch afterwards. Fitting raw epoch
 * milliseconds directly would square numbers around 1.7e12 — enough to lose
 * most of a float64's mantissa to cancellation and produce a visibly wrong
 * slope for a short, shallow segment.
 */
function leastSquares(points: readonly BurnPoint[]): LinearFit {
  // `forecastAccount` returns early below two points, so index 0 always
  // exists; `?? 0` is only here to satisfy `noUncheckedIndexedAccess`.
  const t0 = points[0]?.at ?? 0;
  const n = points.length;
  let sumX = 0;
  let sumY = 0;
  for (const point of points) {
    sumX += point.at - t0;
    sumY += point.usageFraction;
  }
  const meanX = sumX / n;
  const meanY = sumY / n;

  let sxx = 0;
  let sxy = 0;
  for (const point of points) {
    const dx = point.at - t0 - meanX;
    sxx += dx * dx;
    sxy += dx * (point.usageFraction - meanY);
  }

  // All points share one instant (a duplicated snapshot): no slope is
  // derivable, report flat rather than dividing by zero.
  const slopePerMs = sxx === 0 ? 0 : sxy / sxx;
  const relativeIntercept = meanY - slopePerMs * meanX;
  return { slopePerMs, intercept: relativeIntercept - slopePerMs * t0 };
}

// ---------------------------------------------------------------------------
// Pool-level "forecast": a deliberate decision not to project (issue #4847)
// ---------------------------------------------------------------------------

/**
 * The public surface gets a pool-wide `tokens.snapshot` aggregate
 * (`mean_usage_fraction` / `max_usage_fraction` / `exhausted_count`), not a
 * per-account series — see `../../docs/token-analytics.md`. Fitting the same
 * least-squares projection above to `mean_usage_fraction` was considered and
 * rejected: a pool-wide mean can sit comfortably mid-range while one account
 * in the pool is a single sample away from exhaustion, so a trend line
 * through the mean would read as reassuring exactly when it should not.
 * Averaging the input away is not a smaller version of the per-account
 * forecast, it is a different, misleading question — "will the average
 * account run dry", when the actual risk is "will *any* account run dry".
 *
 * So there is no `projectedExhaustionAt` here. Instead this reports the
 * pool's exhaustion state *as measured*, right now: how many of its accounts
 * are exhausted, and how loaded its busiest one is — both `exhausted_count`
 * and `max_usage_fraction` are already the honest, non-averaged answer to
 * "how much trouble is this pool in" that a mean-based ETA would only
 * obscure. `render.ts` renders this instead of a chart, and says why the
 * chart is missing rather than leaving a silent gap.
 */
export interface PoolHealthSummary {
  hostId: string;
  /** Pool size / exhausted count as of the newest sample. */
  accountCount: number;
  exhaustedCount: number;
  /** `exhaustedCount / accountCount`, or `undefined` when the pool is empty
   * (never a misleading `0`). */
  exhaustedFraction?: number;
  /** The newest sample's peak/mean usage across the pool, when reported. */
  maxUsageFraction?: number;
  meanUsageFraction?: number;
  /** Epoch ms of the earliest limit-window reset across the pool, when
   * known — "capacity returns at" names no account, unlike a per-account
   * `limitWindowResetAt`, so it is safe to show alongside this summary even
   * though no ETA is projected from it. */
  nextLimitWindowResetAt?: number;
}

/** Summarize one host's pool curve. Reads the curve's own latest-sample
 * fields for account/exhausted counts (present even when no usage figure was
 * reported that sample) and the newest plottable point for the usage
 * figures (absent when none was). */
export function summarizePoolHealth(curve: PoolBurnCurve): PoolHealthSummary {
  const latest = curve.points[curve.points.length - 1];
  return {
    hostId: curve.hostId,
    accountCount: curve.accountCount,
    exhaustedCount: curve.exhaustedCount,
    exhaustedFraction: curve.accountCount > 0 ? curve.exhaustedCount / curve.accountCount : undefined,
    maxUsageFraction: latest?.maxUsageFraction,
    meanUsageFraction: latest?.meanUsageFraction,
    nextLimitWindowResetAt: latest?.nextLimitWindowResetAt,
  };
}

/** Summarize every pool curve, preserving `buildPoolBurnCurves`' ordering. */
export function summarizePoolHealths(curves: readonly PoolBurnCurve[]): PoolHealthSummary[] {
  return curves.map(summarizePoolHealth);
}
