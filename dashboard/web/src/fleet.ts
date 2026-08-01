/**
 * Snapshot → view model.
 *
 * The `/api/fleet-state` payload is two loosely-coupled collections (`hosts`
 * keyed by id, `activeSweeps` as a flat list carrying `hostId`). Every view
 * wants them joined per host, so the join lives here — once, pure, and
 * testable without a DOM.
 *
 * Two joins that are easy to get wrong, and are pinned by tests:
 *
 * - **The host set is the union of both collections, not `hosts`' keys.** A
 *   host whose first pushed record was a `sweep.started` has live sweeps and
 *   no `hosts` entry at all (the Durable Object only creates one on
 *   `host.health`/`tokens.snapshot` — see `../../src/fleetState.ts`). Keying
 *   off `hosts` alone would silently hide a busy host.
 * - **Zero sweeps is a normal state, not an empty state.** An idle host is
 *   healthy and must still render its health/token panel.
 */

import { secondsSince } from "./format";
import type { ActiveSweep, FleetSnapshot, HostEntry, TokenAccount } from "./types";

/**
 * How old a `host.health` / `tokens.snapshot` may be before it is shown as
 * stale. The daemon samples both every ~5 minutes
 * (`docs/deploy-runbook.md` §10), so 15 minutes is three missed samples — long
 * enough that a single dropped push or a batching delay is not an alarm, short
 * enough to notice a host that stopped reporting.
 */
export const STALE_AFTER_SEC = 15 * 60;

export type HostStatus =
  /** Reporting recently, and the token pool has healthy capacity left. Some
   * accounts being exhausted is normal rotation, not a fault — see
   * `isTokenPoolDegraded` below. */
  | "ok"
  /** Reporting recently, but the token pool is at or near exhaustion. */
  | "degraded"
  /** Last report is older than `STALE_AFTER_SEC`. */
  | "stale"
  /** Known only from `activeSweeps`, or from a `hosts` entry with neither
   * `health` nor `tokens` yet — nothing to assess. */
  | "unknown";

export interface TokenSummary {
  /** The per-account rows, or `[]` for a public viewer, who is sent an
   * aggregate instead. `total` is the pool size either way — check
   * `hasAccountDetail` rather than `accounts.length` to tell "no accounts"
   * from "accounts withheld". */
  accounts: TokenAccount[];
  total: number;
  exhausted: number;
  /** Highest known `usage_fraction`, or `undefined` when no account reports
   * one. Deliberately not `0` — see `format.ts`'s unknown-is-not-zero rule. */
  peakUsage: number | undefined;
  /** False when this summary came from the public aggregate, so the
   * per-account table has nothing to render and should say why. */
  hasAccountDetail: boolean;
}

export interface HostView {
  hostId: string;
  entry: HostEntry;
  sweeps: ActiveSweep[];
  tokens: TokenSummary;
  status: HostStatus;
  /** Most recent of the health/tokens `updatedAt`s — the host's liveness
   * signal. `undefined` when it has never reported either. */
  lastReportAt: string | undefined;
  /** Seconds since `lastReportAt`, or `undefined`. */
  lastReportAgeSec: number | undefined;
}

export interface FleetView {
  hosts: HostView[];
  totalSweeps: number;
  /** Hosts in `stale` or `degraded` — the count the overview headline shows. */
  needsAttention: number;
}

/**
 * Normalize either `tokens.snapshot` shape into one `TokenSummary`.
 *
 * An authenticated viewer's record carries `accounts` and the totals are
 * derived from it. A public viewer's record carries the server-computed
 * aggregate instead (`../../src/redaction.ts`'s `deriveTokenPoolAggregate`)
 * and the same totals are read straight off it. Views get identical fields
 * either way — only the per-account table needs to know the difference, via
 * `hasAccountDetail`.
 */
export function summarizeTokens(entry: HostEntry): TokenSummary {
  const record = entry.tokens?.record;
  const accounts = record?.accounts;

  if (accounts) {
    let peakUsage: number | undefined;
    let exhausted = 0;
    for (const account of accounts) {
      if (account.exhausted) exhausted += 1;
      if (account.usage_fraction !== undefined) {
        peakUsage = peakUsage === undefined ? account.usage_fraction : Math.max(peakUsage, account.usage_fraction);
      }
    }
    return { accounts, total: accounts.length, exhausted, peakUsage, hasAccountDetail: true };
  }

  return {
    accounts: [],
    total: record?.account_count ?? 0,
    exhausted: record?.exhausted_count ?? 0,
    peakUsage: record?.max_usage_fraction ?? undefined,
    // A host that has simply never sent a tokens.snapshot has no record at
    // all; a public viewer's record exists but withholds the rows.
    hasAccountDetail: record === undefined,
  };
}

/**
 * A pool is treated as "close to the edge" — worth a `degraded` badge — once
 * one account or fewer is left to rotate onto, or three quarters of the pool
 * is spent. Below that line, some accounts being exhausted is the pool
 * working exactly as designed (the selector rotates away from them), not a
 * fault: see #4864.
 */
const LOW_AVAILABILITY_THRESHOLD = 1;
const HIGH_EXHAUSTION_FRACTION = 0.75;

/** `true` when the token pool is empty of capacity or nearly so. A pool with
 * no reported accounts (`total === 0`) is not degraded by this check —
 * that host simply has not sent a `tokens.snapshot` yet. */
export function isTokenPoolDegraded(tokens: TokenSummary): boolean {
  if (tokens.total === 0) return false;
  const available = tokens.total - tokens.exhausted;
  return available <= LOW_AVAILABILITY_THRESHOLD || tokens.exhausted / tokens.total >= HIGH_EXHAUSTION_FRACTION;
}

/** Newest of the two `updatedAt`s. String compare is safe here *only* because
 * both are backend-generated `new Date().toISOString()` values — fixed-width
 * UTC, so lexicographic order is chronological order. */
function latestReport(entry: HostEntry): string | undefined {
  const candidates = [entry.health?.updatedAt, entry.tokens?.updatedAt].filter(
    (value): value is string => typeof value === "string" && value.length > 0,
  );
  if (candidates.length === 0) return undefined;
  return candidates.sort()[candidates.length - 1];
}

export function buildHostView(
  hostId: string,
  entry: HostEntry,
  sweeps: ActiveSweep[],
  now: Date = new Date(),
): HostView {
  const tokens = summarizeTokens(entry);
  const lastReportAt = latestReport(entry);
  const lastReportAgeSec = secondsSince(lastReportAt, now);

  let status: HostStatus;
  if (lastReportAgeSec === undefined) {
    status = "unknown";
  } else if (lastReportAgeSec > STALE_AFTER_SEC) {
    status = "stale";
  } else if (isTokenPoolDegraded(tokens)) {
    status = "degraded";
  } else {
    status = "ok";
  }

  return { hostId, entry, sweeps, tokens, status, lastReportAt, lastReportAgeSec };
}

/** Sort: hosts needing attention first, then busiest, then by id so the list
 * does not reshuffle between polls when nothing changed. */
const STATUS_ORDER: Record<HostStatus, number> = { stale: 0, degraded: 1, unknown: 2, ok: 3 };

export function buildFleetView(snapshot: FleetSnapshot, now: Date = new Date()): FleetView {
  const sweepsByHost = new Map<string, ActiveSweep[]>();
  for (const sweep of snapshot.activeSweeps) {
    const list = sweepsByHost.get(sweep.hostId);
    if (list) list.push(sweep);
    else sweepsByHost.set(sweep.hostId, [sweep]);
  }

  const hostIds = new Set<string>([...Object.keys(snapshot.hosts), ...sweepsByHost.keys()]);

  const hosts = [...hostIds]
    .map((hostId) =>
      buildHostView(hostId, snapshot.hosts[hostId] ?? {}, sortSweeps(sweepsByHost.get(hostId) ?? []), now),
    )
    .sort(
      (a, b) =>
        STATUS_ORDER[a.status] - STATUS_ORDER[b.status] ||
        b.sweeps.length - a.sweeps.length ||
        a.hostId.localeCompare(b.hostId),
    );

  return {
    hosts,
    totalSweeps: snapshot.activeSweeps.length,
    needsAttention: hosts.filter((host) => host.status === "stale" || host.status === "degraded").length,
  };
}

/** Longest-running first (a sweep with no `startedAt` sorts last), then by
 * `sweepId` for a stable order across polls. */
export function sortSweeps(sweeps: ActiveSweep[]): ActiveSweep[] {
  return [...sweeps].sort((a, b) => {
    const aStart = a.startedAt ? Date.parse(a.startedAt) : Number.POSITIVE_INFINITY;
    const bStart = b.startedAt ? Date.parse(b.startedAt) : Number.POSITIVE_INFINITY;
    const aKey = Number.isNaN(aStart) ? Number.POSITIVE_INFINITY : aStart;
    const bKey = Number.isNaN(bStart) ? Number.POSITIVE_INFINITY : bStart;
    return aKey - bKey || a.sweepId.localeCompare(b.sweepId);
  });
}

export function findHost(view: FleetView, hostId: string): HostView | undefined {
  return view.hosts.find((host) => host.hostId === hostId);
}
