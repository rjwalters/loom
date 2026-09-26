/**
 * The fleet work-queue view model (Issue #8852, phase 3): pure functions over
 * the per-host `queue` entries (`queueTypes.ts`) that `views/workQueue.ts`
 * renders. No DOM here, so every decision is unit-testable on plain data.
 *
 * ## Freshness is per host, and "empty" is not "unknown"
 *
 * [`queueHealth`] separates the states a bare count would merge:
 *
 *  - `absent`  — the host has never sent a queue (a daemon older than
 *    #8852 phase 2, or its work finder is off). Nothing is known.
 *  - `idle`    — a recent tick listed nothing. The queue is really empty.
 *  - `active`  — a recent tick listed work.
 *  - `stale`   — the last queue arrived more than [`QUEUE_STALE_AFTER_SEC`]
 *    ago. The daemon sends one only after a new work-finder tick, so this
 *    means the work finder (or the host's telemetry) has stopped. Counts are
 *    last-known and are left out of the fleet totals.
 *  - `offline` — older than [`QUEUE_OFFLINE_AFTER_SEC`]. Its rows are dropped
 *    from the fleet lists entirely.
 *
 * Ages are measured from `updatedAt`, the backend's ingest clock, so a host
 * with a skewed clock cannot look fresh; `tick_at` (the daemon's own clock)
 * is displayed alongside.
 *
 * ## One issue, several hosts
 *
 * Hosts that manage the same repos each list the same ready issue: host A
 * runs it (`in_flight`) while host B skips it (`peer_claim`). A fleet view
 * that listed both would count it twice and call it blocked. [`mergeFleetQueue`]
 * folds rows by `repo#issue` and keeps the most advanced state (running,
 * then ready, then blocked); the other hosts' views stay attached as
 * `others`. Private rows on the public view have no repo or issue to fold on
 * and stay one item per row.
 */

import { STALE_AFTER_SEC, type HostView } from "./fleet";
import { secondsSince } from "./format";
import type { QueueRow, QueueRowState, QueueSnapshotRecord } from "./queueTypes";
import type { ActiveSweep, Timestamped } from "./types";

/** Same bound the host status badge uses: ~3 missed 5-minute samples. */
export const QUEUE_STALE_AFTER_SEC = STALE_AFTER_SEC;
/** Same bound as the backend's `OFFLINE_AFTER_SEC`. */
export const QUEUE_OFFLINE_AFTER_SEC = 4 * 60 * 60;

export type QueueHealth = "absent" | "idle" | "active" | "stale" | "offline";

export interface HostQueueSummary {
  hostId: string;
  health: QueueHealth;
  /** Seconds since the backend last accepted a newer tick. */
  ageSec: number | undefined;
  queue: Timestamped<QueueSnapshotRecord> | undefined;
  /** Some repos could not be listed on the last tick. */
  incomplete: boolean;
}

export function queueHealth(queue: Timestamped<QueueSnapshotRecord> | undefined, now: Date): QueueHealth {
  if (!queue) return "absent";
  const age = secondsSince(queue.updatedAt, now);
  if (age === undefined || age > QUEUE_OFFLINE_AFTER_SEC) return "offline";
  if (age > QUEUE_STALE_AFTER_SEC) return "stale";
  const { counts, seen } = queue.record;
  return seen === 0 && counts.running + counts.ready + counts.blocked === 0 ? "idle" : "active";
}

export function summarizeHostQueue(host: HostView, now: Date): HostQueueSummary {
  const queue = host.entry.queue;
  return {
    hostId: host.hostId,
    health: queueHealth(queue, now),
    ageSec: queue ? secondsSince(queue.updatedAt, now) : undefined,
    queue,
    incomplete: queue ? queue.record.listing_failed.length + queue.record.listing_failed_unresolved > 0 : false,
  };
}

/** Whether a host's counts describe the present (and so may be totalled). */
export function isCurrent(health: QueueHealth): boolean {
  return health === "idle" || health === "active";
}

export interface FleetQueueTotals {
  backlog: number;
  running: number;
  ready: number;
  blocked: number;
  /** Hosts whose counts went into the totals. */
  currentHosts: number;
  /** Hosts with a queue that is too old to total. */
  staleHosts: number;
}

/** Per-state totals over hosts with a current queue. Summing across hosts
 * double-counts an issue two hosts both list; it is the "work each host
 * sees" total, which is what the per-host table beside it adds up to. */
export function fleetQueueTotals(summaries: readonly HostQueueSummary[]): FleetQueueTotals {
  const totals: FleetQueueTotals = { backlog: 0, running: 0, ready: 0, blocked: 0, currentHosts: 0, staleHosts: 0 };
  for (const summary of summaries) {
    if (!summary.queue) continue;
    if (!isCurrent(summary.health)) {
      totals.staleHosts += 1;
      continue;
    }
    const { seen, counts } = summary.queue.record;
    totals.currentHosts += 1;
    totals.backlog += seen;
    totals.running += counts.running;
    totals.ready += counts.ready;
    totals.blocked += counts.blocked;
  }
  return totals;
}

export interface HostRow {
  hostId: string;
  row: QueueRow;
  /** Seconds since the host's queue arrived — how old this observation is. */
  observedAgeSec: number | undefined;
}

export interface FleetQueueItem {
  repo?: string;
  issue?: number;
  visibility: "public" | "private";
  state: QueueRowState;
  /** The observation the item is classified by. */
  primary: HostRow;
  /** Every other host's view of the same issue. */
  others: HostRow[];
  /** The live sweep for this issue, when one is reporting phases. */
  sweep?: ActiveSweep;
}

const STATE_RANK: Readonly<Record<QueueRowState, number>> = { running: 0, ready: 1, blocked: 2, unknown: 3 };

function better(a: HostRow, b: HostRow): boolean {
  const byState = STATE_RANK[a.row.state] - STATE_RANK[b.row.state];
  if (byState !== 0) return byState < 0;
  return (a.observedAgeSec ?? Infinity) < (b.observedAgeSec ?? Infinity);
}

/** Fold every non-offline host's rows into one item per issue, ordered by
 * state, then `loom:urgent`, then oldest issue first — the same tiebreaks the
 * daemon's own comparator uses after workspace priority. */
export function mergeFleetQueue(
  summaries: readonly HostQueueSummary[],
  sweeps: readonly ActiveSweep[],
): FleetQueueItem[] {
  const byKey = new Map<string, FleetQueueItem>();
  const anonymous: FleetQueueItem[] = [];

  for (const summary of summaries) {
    if (!summary.queue || summary.health === "offline") continue;
    for (const row of summary.queue.record.rows) {
      const observation: HostRow = { hostId: summary.hostId, row, observedAgeSec: summary.ageSec };
      if (row.repo === undefined || row.issue === undefined) {
        anonymous.push({ visibility: row.visibility, state: row.state, primary: observation, others: [] });
        continue;
      }
      const key = `${row.repo}#${row.issue}`;
      const existing = byKey.get(key);
      if (!existing) {
        byKey.set(key, {
          repo: row.repo,
          issue: row.issue,
          visibility: row.visibility,
          state: row.state,
          primary: observation,
          others: [],
        });
      } else if (better(observation, existing.primary)) {
        existing.others.push(existing.primary);
        existing.primary = observation;
        existing.state = row.state;
      } else {
        existing.others.push(observation);
      }
    }
  }

  const items = [...byKey.values(), ...anonymous];
  for (const item of items) {
    item.sweep = sweeps.find((sweep) => sweep.repo === item.repo && sweep.issue === item.issue && item.issue !== undefined);
  }
  return items.sort(compareItems);
}

function compareItems(a: FleetQueueItem, b: FleetQueueItem): number {
  const byState = STATE_RANK[a.state] - STATE_RANK[b.state];
  if (byState !== 0) return byState;
  if (a.primary.row.urgent !== b.primary.row.urgent) return a.primary.row.urgent ? -1 : 1;
  const aCreated = Date.parse(a.primary.row.created_at ?? "");
  const bCreated = Date.parse(b.primary.row.created_at ?? "");
  const aKey = Number.isFinite(aCreated) ? aCreated : Infinity;
  const bKey = Number.isFinite(bCreated) ? bCreated : Infinity;
  if (aKey !== bKey) return aKey - bKey;
  return (a.issue ?? Infinity) - (b.issue ?? Infinity);
}

/** The PR an `open_pr` row's `detail` names. The daemon writes it as
 * `"open PR #8906"`; a bare `"8906"` is accepted too. */
export function openPrNumber(row: QueueRow): number | undefined {
  if (row.disposition !== "open_pr" || row.detail === undefined) return undefined;
  const match = /(?:^|#)(\d+)\s*$/.exec(row.detail.trim());
  return match ? Number(match[1]) : undefined;
}

/** The reason text with its structured specifics (park label, open PR)
 * appended. */
export function reasonText(row: QueueRow): string {
  const base = row.reason || row.disposition.replace(/_/g, " ");
  return row.detail === undefined ? base : `${base} (${row.detail})`;
}
