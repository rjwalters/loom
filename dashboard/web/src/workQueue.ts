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
 * that listed both would count it twice and call it blocked.
 * [`mergeFleetQueue`] folds rows by `repo#issue` under the fleet merge rule
 * (Issue #9310) — most advanced `plan_state` wins, then the shard owner,
 * then the lower in-host `position`, then the host id — and the other hosts'
 * views stay attached as `others`. Private rows on the public view have no
 * repo or issue to fold on and stay one item per row.
 *
 * The rule is **not** this module's own invention: it is a port of
 * `loom-daemon`'s `work_finder::dispatch_plan_merge::merge_plans`, and both
 * implementations are pinned to the same JSON fixture
 * (`dashboard/test/fixtures/dispatch-plan-merge.json`), so neither can drift
 * without failing the other's test. `defaults/docs/dispatch-plan.md`
 * documents it in prose. Two things it deliberately never compares across
 * hosts: `workspace_priority` (per-host and unsynced — each host's own
 * config numbers its own repos) and wall-clock age, both of which the hosts
 * already folded into their own `position`.
 */

import { STALE_AFTER_SEC, type HostView } from "./fleet";
import { secondsSince } from "./format";
import type { QueuePlanState, QueueRow, QueueRowState, QueueSnapshotRecord } from "./queueTypes";
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
  /** The reporting host's own shard, when it is sharded — the other half of
   * the `owning_shard` tie-break (Issue #9310). */
  hostShard?: number;
}

export interface FleetQueueItem {
  repo?: string;
  issue?: number;
  visibility: "public" | "private";
  state: QueueRowState;
  /** The primary's dispatch-plan state — what the fleet says this issue is
   * doing, and the first key the fleet order sorts on (Issue #9310). */
  planState: QueuePlanState;
  /** The observation the item is classified by. */
  primary: HostRow;
  /** Every other host's view of the same issue, best first. */
  others: HostRow[];
  /** The live sweep for this issue, when one is reporting phases. */
  sweep?: ActiveSweep;
}

/** Fleet-order rank of a plan state: most advanced first, `unknown` last so
 * a state this build does not know never displaces one it does. */
const PLAN_STATE_RANK: Readonly<Record<QueuePlanState, number>> = {
  running: 0,
  next: 1,
  queued: 2,
  blocked: 3,
  unknown: 4,
};

/** A row's plan state. A pre-#9288 row has none, so its coarse state stands
 * in: a `ready` row is waiting in the plan, which is `queued`. */
export function rowPlanState(row: QueueRow): QueuePlanState {
  if (row.plan_state !== undefined) return row.plan_state;
  switch (row.state) {
    case "running":
      return "running";
    case "ready":
      return "queued";
    case "blocked":
      return "blocked";
    default:
      return "unknown";
  }
}

/** A row's place in its own host's plan, for the in-host half of the merge.
 * A pre-#9288 row (no `plan_state` at all) falls back to `rank`, the only
 * dispatch order it reports; a #9288 row with a plan but no `position` is
 * genuinely outside that host's plan and sorts behind every positioned row.
 * Positions from two hosts are never compared — only ranked within a host,
 * then interleaved. */
function rowPosition(row: QueueRow): number {
  if (row.position !== undefined) return row.position;
  if (row.plan_state === undefined && row.rank > 0) return row.rank;
  return Infinity;
}

/** Whether this host is the one the row's workspace shards to. An unsharded
 * host owns nothing, so an unsharded fleet falls straight through to the
 * `position` tie-break. */
function owns(observation: HostRow): boolean {
  return observation.hostShard !== undefined && observation.hostShard === observation.row.owning_shard;
}

function compareText(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function compareNumbers(a: number, b: number): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/** Which of two hosts' observations of one issue is the primary: most
 * advanced `plan_state`, then the shard owner, then the lower in-host
 * position, then the lower host id. Negative when `a` wins. */
function comparePrimary(a: HostRow, b: HostRow): number {
  const byState = PLAN_STATE_RANK[rowPlanState(a.row)] - PLAN_STATE_RANK[rowPlanState(b.row)];
  if (byState !== 0) return byState;
  if (owns(a) !== owns(b)) return owns(a) ? -1 : 1;
  return compareNumbers(rowPosition(a.row), rowPosition(b.row)) || compareText(a.hostId, b.hostId);
}

/** Fold every non-offline host's rows into one item per `repo#issue`, in
 * fleet order (Issue #9310) — see the module header for the rule and the
 * fixture that pins it. */
export function mergeFleetQueue(
  summaries: readonly HostQueueSummary[],
  sweeps: readonly ActiveSweep[],
): FleetQueueItem[] {
  const byKey = new Map<string, FleetQueueItem>();
  const anonymous: FleetQueueItem[] = [];

  for (const summary of summaries) {
    if (!summary.queue || summary.health === "offline") continue;
    const hostShard = summary.queue.record.plan?.shard?.host_shard;
    for (const row of summary.queue.record.rows) {
      const observation: HostRow = { hostId: summary.hostId, row, observedAgeSec: summary.ageSec };
      if (hostShard !== undefined) observation.hostShard = hostShard;
      if (row.repo === undefined || row.issue === undefined) {
        // Nothing to fold on: a withheld private row is its own item.
        anonymous.push(newItem(observation));
        continue;
      }
      const key = `${row.repo}#${row.issue}`;
      const existing = byKey.get(key);
      if (existing) existing.others.push(observation);
      else byKey.set(key, newItem(observation));
    }
  }

  const items = [...byKey.values(), ...anonymous];
  for (const item of items) {
    resolvePrimary(item);
    item.sweep = sweeps.find((sweep) => sweep.repo === item.repo && sweep.issue === item.issue && item.issue !== undefined);
  }
  return orderFleetItems(items);
}

/** A one-observation item; [`resolvePrimary`] settles which observation is
 * primary once every host has contributed. */
function newItem(observation: HostRow): FleetQueueItem {
  const row = observation.row;
  const item: FleetQueueItem = {
    visibility: row.visibility,
    state: row.state,
    planState: rowPlanState(row),
    primary: observation,
    others: [],
  };
  if (row.repo !== undefined) item.repo = row.repo;
  if (row.issue !== undefined) item.issue = row.issue;
  return item;
}

/** Re-rank an item's observations under [`comparePrimary`], so the primary
 * is the best one and `others` runs best-first behind it. */
function resolvePrimary(item: FleetQueueItem): void {
  const [best, ...others] = [item.primary, ...item.others].sort(comparePrimary);
  if (best === undefined) return; // Unreachable: the primary is always there.
  item.primary = best;
  item.others = others;
  item.state = best.row.state;
  item.planState = rowPlanState(best.row);
  item.visibility = best.row.visibility;
}

/** Fleet order: `plan_state` band first, then a round-robin interleave of
 * the hosts' own plan orders. Within a band each host's items are taken in
 * its own `position` order and handed out round by round (hosts within a
 * round by host id), so one host's long backlog never buries another host's
 * first row — and two hosts' positions are never compared as one scale. */
function orderFleetItems(items: FleetQueueItem[]): FleetQueueItem[] {
  const keyed = items.map((item) => ({
    item,
    state: PLAN_STATE_RANK[item.planState],
    hostId: item.primary.hostId,
    position: rowPosition(item.primary.row),
    round: 0,
  }));
  type Keyed = (typeof keyed)[number];
  const withinHost = (a: Keyed, b: Keyed): number =>
    a.state - b.state ||
    compareText(a.hostId, b.hostId) ||
    compareNumbers(a.position, b.position) ||
    compareText(a.item.repo ?? "", b.item.repo ?? "") ||
    compareNumbers(a.item.issue ?? Infinity, b.item.issue ?? Infinity);

  keyed.sort(withinHost);
  const rounds = new Map<string, number>();
  for (const entry of keyed) {
    const band = `${entry.state} ${entry.hostId}`;
    const round = rounds.get(band) ?? 0;
    entry.round = round;
    rounds.set(band, round + 1);
  }
  keyed.sort((a, b) => a.state - b.state || a.round - b.round || withinHost(a, b));
  return keyed.map((entry) => entry.item);
}

/** A client-side filter over the already-merged fleet items (Issue #9032):
 * `repo` and `tier` are ANDed, and an unset field matches everything. Pure
 * and DOM-free, like the rest of this module, so the predicate is
 * unit-testable without a browser — filtering never re-fetches, it only
 * narrows the `FleetQueueItem[]` `mergeFleetQueue` already produced. */
export interface QueueFilter {
  repo?: string;
  tier?: string;
}

/** The distinct repo and tier values across `items`, each sorted for a
 * stable dropdown order. An item with no repo (a withheld private row) or no
 * `tier` label contributes to the unfiltered lists but not to these option
 * sets — there is no "no repo" or "no tier" filter value to select. */
export interface QueueFilterOptions {
  repos: string[];
  tiers: string[];
}

export function queueFilterOptions(items: readonly FleetQueueItem[]): QueueFilterOptions {
  const repos = new Set<string>();
  const tiers = new Set<string>();
  for (const item of items) {
    if (item.repo) repos.add(item.repo);
    if (item.primary.row.tier) tiers.add(item.primary.row.tier);
  }
  return { repos: [...repos].sort(), tiers: [...tiers].sort() };
}

export function filterQueueItems(items: readonly FleetQueueItem[], filter: QueueFilter): FleetQueueItem[] {
  return items.filter((item) => {
    if (filter.repo && item.repo !== filter.repo) return false;
    if (filter.tier && item.primary.row.tier !== filter.tier) return false;
    return true;
  });
}

/** The PR an `open_pr` row's `detail` names. The daemon writes it as
 * `"open PR #8906"`; a bare `"8906"` is accepted too. */
export function openPrNumber(row: QueueRow): number | undefined {
  if (row.disposition !== "open_pr" || row.detail === undefined) return undefined;
  const match = /(?:^|#)(\d+)\s*$/.exec(row.detail.trim());
  return match ? Number(match[1]) : undefined;
}

/** A row's dispatch position for display. Forge-side `labelled_blocked` rows
 * (#8957) are outside the dispatch order and carry rank 0, shown as a dash. */
export function rankText(row: QueueRow): string {
  return row.rank > 0 ? String(row.rank) : "–";
}

/** A row's place in its own host's dispatch plan (Issue #9288), verbatim:
 * `#3 next`, `#5 queued (repo cap)`, or `running` / `blocked` with no
 * position. A pre-#9288 row falls back to its coarse state. */
export function planText(row: QueueRow): string {
  if (row.plan_state === undefined) return row.state;
  const position = row.position === undefined ? "" : `#${row.position} `;
  const gate = row.gate === undefined ? "" : ` (${row.gate.replace(/_/g, " ")})`;
  return `${position}${row.plan_state}${gate}`;
}

/** The reason text with its structured specifics (park label, open PR)
 * appended. */
export function reasonText(row: QueueRow): string {
  const base = row.reason || row.disposition.replace(/_/g, " ");
  return row.detail === undefined ? base : `${base} (${row.detail})`;
}
