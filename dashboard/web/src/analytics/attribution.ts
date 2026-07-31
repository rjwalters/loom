/**
 * Per-repo attribution of fleet-wide token usage (Epic #4702, Phase 3,
 * issue #4752).
 *
 * ## The problem this module exists to solve
 *
 * `tokens.snapshot` has **no `repo` field** — it is fleet-wide token-pool
 * state (`.loom/docs/telemetry-schema.md`: "host-level — no `repo` /
 * `visibility`"). Nothing on the wire ever says "this account spent these
 * tokens on that repository". The only records that carry `repo` are the
 * `sweep.*` family. Attribution is therefore a **join**, and the join has to
 * be stated explicitly rather than implied by a chart.
 *
 * ## The join, precisely
 *
 * **Join key: `hostId` + time overlap. Nothing else.**
 *
 * - `hostId` is a hard key: a token pool is a per-host resource, so usage
 *   observed on host A can only have been spent by sweeps on host A.
 * - Time overlap is the soft key, and it is applied to *intervals*, never to
 *   instants. Each consecutive pair of `tokens.snapshot` samples on a host
 *   defines a **usage interval** `[prev.captured_at, next.captured_at)` and,
 *   per account, a **usage delta** `next.usage_fraction -
 *   prev.usage_fraction`. That delta is the fleet's measured consumption over
 *   exactly that interval — attributing it is the whole job.
 * - Every sweep window on the same host that overlaps the interval is a
 *   candidate consumer. Usage is assumed uniform across the interval, so:
 *   - The **union** of all overlapping sweep windows (clipped to the interval)
 *     decides how much of the delta is attributable at all. An interval only
 *     10% covered by sweeps yields 10% attributable usage; the other 90% is
 *     unattributed, because nothing was running for it.
 *   - That attributable portion is then split across the overlapping sweeps
 *     **in proportion to each sweep's overlap duration** (sweep-seconds) and
 *     summed per repo. Two sweeps running the whole interval split it 50/50.
 *
 * **`account` and `model` are dimensions, not join keys.** The issue's
 * original sketch proposed joining "per account/model". That is not derivable
 * from the telemetry: `tokens.snapshot` accounts have no `model`, and
 * `sweep.started`'s `model` (`opus`/`sonnet`) has no account field — no record
 * anywhere links a pool account to a sweep. Pretending otherwise would invent
 * a correlation. So both are carried through as *breakdowns* of an
 * already-computed attribution (`byAccount` is exact — the delta genuinely
 * came from that account; `byModel` inherits the proportional split of the
 * sweeps that claimed it) rather than as keys that narrow the join.
 *
 * ## Tolerances (all configurable, all defaulted conservatively)
 *
 * | Knob | Default | Why |
 * |---|---|---|
 * | `edgeToleranceMs` | 60 s | Sweep windows are widened by this on both ends before overlap is tested. `captured_at` (pool sampler clock) and `started_at` (sweep clock) are independent daemon timestamps; without slack, a sweep that starts moments after a snapshot loses its first interval outright. |
 * | `maxSampleGapMs` | 60 min | A pair of snapshots further apart than this is **dropped entirely** (counted in `droppedIntervals`). The usage in between is real but unobserved — smearing an hours-wide delta over whatever sweeps happen to overlap it would be fiction. |
 * | `openSweepMaxDurationMs` | 6 h | A sweep with `sweep.started` but no terminal record is capped at this past its start (and at `now`), so one crashed sweep that never emitted `sweep.completed` cannot absorb the fleet's usage forever. |
 *
 * ## What is deliberately *not* claimed
 *
 * - **A negative delta is a limit-window rollover, not negative usage.** That
 *   interval is dropped for that account (the pre-rollover portion of the burn
 *   is unobservable), never attributed and never subtracted.
 * - **Usage with no overlapping sweep goes to `unattributed`** — including the
 *   uncovered *portion* of a partially-covered interval. Support roles
 *   (Judge/Champion/Curator crons), manual sessions, and any consumer that
 *   does not emit `sweep.*` telemetry all burn real tokens. Silently
 *   redistributing that onto the repos that *did* run sweeps would inflate
 *   every number on the page. The unattributed share is reported as a
 *   first-class row, and a large one means the attribution is not to be
 *   trusted.
 * - **Units are account-window fractions**, not dollars or tokens:
 *   `1.0` = one account's entire limit window. `tokens.snapshot` reports a
 *   fraction of a quota, and no cost or absolute token count exists anywhere
 *   in the telemetry — so no cost figure is synthesized here.
 */

import type { SweepWindow, TokenSample } from "./types.js";

export const DEFAULT_EDGE_TOLERANCE_MS = 60 * 1000;
export const DEFAULT_MAX_SAMPLE_GAP_MS = 60 * 60 * 1000;
export const DEFAULT_OPEN_SWEEP_MAX_DURATION_MS = 6 * 60 * 60 * 1000;

export interface AttributionOptions {
  /** Reference instant (epoch ms) used to cap still-open sweep windows.
   * Injectable so tests are deterministic. */
  now?: number;
  /** See the tolerance table in the module doc. */
  edgeToleranceMs?: number;
  maxSampleGapMs?: number;
  openSweepMaxDurationMs?: number;
}

export interface NamedUsage {
  name: string;
  usage: number;
}

export interface RepoAttribution {
  repo: string;
  /** Attributed usage in account-window fractions. */
  usage: number;
  /** Fraction of `AttributionResult.totalUsage` (0..1). */
  share: number;
  /** Distinct sweeps that contributed. */
  sweepCount: number;
  /** Exact: the accounts whose deltas were attributed here. */
  byAccount: NamedUsage[];
  /** Inherited from the claiming sweeps' `model` (`"unknown"` when a sweep
   * reported none). */
  byModel: NamedUsage[];
}

export interface AttributionResult {
  repos: RepoAttribution[];
  /** Observed usage in intervals no sweep overlapped — crons, manual
   * sessions, and any non-sweep consumer. */
  unattributedUsage: number;
  /** `sum(repos.usage) + unattributedUsage`. */
  totalUsage: number;
  /** Consecutive-snapshot pairs skipped because they straddled a telemetry
   * gap wider than `maxSampleGapMs`. A high count means thin coverage. */
  droppedIntervals: number;
  /** Consecutive-snapshot/account pairs skipped because usage fell (a limit-
   * window rollover). */
  rolloverIntervals: number;
  /** Intervals that carried usage and were fully attributed to sweeps. */
  attributedIntervals: number;
  /** Bounds of the analyzed history (epoch ms), when any sample was seen. */
  rangeStart?: number;
  rangeEnd?: number;
}

interface Accumulator {
  usage: number;
  sweeps: Set<string>;
  byAccount: Map<string, number>;
  byModel: Map<string, number>;
}

/** Effective window of a sweep, with open windows capped and edges widened. */
interface EffectiveWindow {
  window: SweepWindow;
  start: number;
  end: number;
}

/**
 * Join `tokens.snapshot` history against sweep windows to produce per-repo
 * usage attribution. See the module doc for the exact join and its tolerances.
 */
export function attributeUsageToRepos(
  samples: readonly TokenSample[],
  sweeps: readonly SweepWindow[],
  options: AttributionOptions = {},
): AttributionResult {
  const now = options.now ?? Date.now();
  const edgeToleranceMs = options.edgeToleranceMs ?? DEFAULT_EDGE_TOLERANCE_MS;
  const maxSampleGapMs = options.maxSampleGapMs ?? DEFAULT_MAX_SAMPLE_GAP_MS;
  const openSweepMaxDurationMs = options.openSweepMaxDurationMs ?? DEFAULT_OPEN_SWEEP_MAX_DURATION_MS;

  const windowsByHost = new Map<string, EffectiveWindow[]>();
  for (const sweep of sweeps) {
    const end = sweep.endedAt ?? Math.min(now, sweep.startedAt + openSweepMaxDurationMs);
    const list = windowsByHost.get(sweep.hostId) ?? [];
    list.push({
      window: sweep,
      start: sweep.startedAt - edgeToleranceMs,
      end: Math.max(end, sweep.startedAt) + edgeToleranceMs,
    });
    windowsByHost.set(sweep.hostId, list);
  }

  const samplesByHost = new Map<string, TokenSample[]>();
  for (const sample of samples) {
    const list = samplesByHost.get(sample.hostId) ?? [];
    list.push(sample);
    samplesByHost.set(sample.hostId, list);
  }

  const byRepo = new Map<string, Accumulator>();
  let unattributedUsage = 0;
  let droppedIntervals = 0;
  let rolloverIntervals = 0;
  let attributedIntervals = 0;
  let rangeStart: number | undefined;
  let rangeEnd: number | undefined;

  for (const [hostId, hostSamples] of samplesByHost) {
    hostSamples.sort((a, b) => a.at - b.at);
    const hostWindows = windowsByHost.get(hostId) ?? [];

    for (const sample of hostSamples) {
      rangeStart = rangeStart === undefined ? sample.at : Math.min(rangeStart, sample.at);
      rangeEnd = rangeEnd === undefined ? sample.at : Math.max(rangeEnd, sample.at);
    }

    for (let i = 1; i < hostSamples.length; i += 1) {
      const previous = hostSamples[i - 1];
      const current = hostSamples[i];
      /* c8 ignore next -- both indices are in range by the loop bounds; the
         guard exists only to satisfy `noUncheckedIndexedAccess`. */
      if (!previous || !current) continue;
      const intervalStart = previous.at;
      const intervalEnd = current.at;
      if (intervalEnd <= intervalStart) continue; // duplicate snapshot instant
      if (intervalEnd - intervalStart > maxSampleGapMs) {
        droppedIntervals += 1;
        continue;
      }

      const deltas = accountDeltas(previous, current);
      if (deltas.rollovers > 0) rolloverIntervals += deltas.rollovers;
      if (deltas.byAccount.size === 0) continue;

      const overlaps = overlappingSweeps(hostWindows, intervalStart, intervalEnd);
      const totalOverlap = overlaps.reduce((sum, entry) => sum + entry.overlapMs, 0);
      // Usage is assumed uniform across the interval, so the share that is
      // attributable at all is the share of the interval covered by *some*
      // sweep — the union, not the sum (two concurrent sweeps cover the same
      // seconds once). The remainder is genuinely unattributable: no sweep was
      // running for it.
      const coveredMs = unionLength(overlaps);
      const coveredFraction = Math.min(coveredMs / (intervalEnd - intervalStart), 1);

      if (totalOverlap <= 0 || coveredFraction <= 0) {
        for (const usage of deltas.byAccount.values()) unattributedUsage += usage;
        continue;
      }

      attributedIntervals += 1;
      for (const [account, usage] of deltas.byAccount) {
        const attributable = usage * coveredFraction;
        unattributedUsage += usage - attributable;
        for (const { window, overlapMs } of overlaps) {
          const attributed = (attributable * overlapMs) / totalOverlap;
          if (attributed <= 0) continue;
          const bucket = accumulatorFor(byRepo, window.repo);
          bucket.usage += attributed;
          bucket.sweeps.add(`${window.hostId} ${window.sweepId}`);
          addTo(bucket.byAccount, account, attributed);
          addTo(bucket.byModel, window.model ?? "unknown", attributed);
        }
      }
    }
  }

  const attributedTotal = [...byRepo.values()].reduce((sum, bucket) => sum + bucket.usage, 0);
  const totalUsage = attributedTotal + unattributedUsage;

  const repos: RepoAttribution[] = [...byRepo.entries()]
    .map(([repo, bucket]) => ({
      repo,
      usage: bucket.usage,
      share: totalUsage > 0 ? bucket.usage / totalUsage : 0,
      sweepCount: bucket.sweeps.size,
      byAccount: toSortedUsage(bucket.byAccount),
      byModel: toSortedUsage(bucket.byModel),
    }))
    .sort((a, b) => b.usage - a.usage || a.repo.localeCompare(b.repo));

  return {
    repos,
    unattributedUsage,
    totalUsage,
    droppedIntervals,
    rolloverIntervals,
    attributedIntervals,
    rangeStart,
    rangeEnd,
  };
}

/** Positive per-account usage deltas between two consecutive snapshots.
 * Accounts absent from either snapshot, or with unknown usage in either, are
 * skipped — there is no delta to compute. A negative delta is a limit-window
 * rollover: counted, never attributed. */
function accountDeltas(
  previous: TokenSample,
  current: TokenSample,
): { byAccount: Map<string, number>; rollovers: number } {
  const previousUsage = new Map<string, number>();
  for (const reading of previous.accounts) {
    if (reading.usageFraction !== undefined) previousUsage.set(reading.account, reading.usageFraction);
  }

  const byAccount = new Map<string, number>();
  let rollovers = 0;
  for (const reading of current.accounts) {
    if (reading.usageFraction === undefined) continue;
    const before = previousUsage.get(reading.account);
    if (before === undefined) continue;
    const delta = reading.usageFraction - before;
    if (delta < 0) {
      rollovers += 1;
      continue;
    }
    if (delta === 0) continue;
    byAccount.set(reading.account, delta);
  }
  return { byAccount, rollovers };
}

interface Overlap {
  window: SweepWindow;
  overlapMs: number;
  /** The overlapping span itself, clipped to the interval — needed for the
   * union, which the per-sweep durations alone cannot reconstruct. */
  start: number;
  end: number;
}

function overlappingSweeps(
  windows: readonly EffectiveWindow[],
  intervalStart: number,
  intervalEnd: number,
): Overlap[] {
  const overlaps: Overlap[] = [];
  for (const candidate of windows) {
    const start = Math.max(candidate.start, intervalStart);
    const end = Math.min(candidate.end, intervalEnd);
    if (end > start) overlaps.push({ window: candidate.window, overlapMs: end - start, start, end });
  }
  return overlaps;
}

/** Total length of the union of a set of spans (overlapping spans counted
 * once). Classic sweep-line: sort by start, merge as you go. */
function unionLength(spans: readonly Overlap[]): number {
  const sorted = [...spans].sort((a, b) => a.start - b.start);
  let total = 0;
  let currentStart: number | undefined;
  let currentEnd = 0;
  for (const span of sorted) {
    if (currentStart === undefined) {
      currentStart = span.start;
      currentEnd = span.end;
    } else if (span.start > currentEnd) {
      total += currentEnd - currentStart;
      currentStart = span.start;
      currentEnd = span.end;
    } else if (span.end > currentEnd) {
      currentEnd = span.end;
    }
  }
  if (currentStart === undefined) return 0; // no spans at all
  return total + (currentEnd - currentStart);
}

function accumulatorFor(byRepo: Map<string, Accumulator>, repo: string): Accumulator {
  let bucket = byRepo.get(repo);
  if (!bucket) {
    bucket = { usage: 0, sweeps: new Set(), byAccount: new Map(), byModel: new Map() };
    byRepo.set(repo, bucket);
  }
  return bucket;
}

function addTo(map: Map<string, number>, key: string, amount: number): void {
  map.set(key, (map.get(key) ?? 0) + amount);
}

function toSortedUsage(map: ReadonlyMap<string, number>): NamedUsage[] {
  return [...map.entries()]
    .map(([name, usage]) => ({ name, usage }))
    .sort((a, b) => b.usage - a.usage || a.name.localeCompare(b.name));
}
