/**
 * `queue.snapshot` ingest, live state and redaction (Issue #8852, phase 3).
 *
 * Phase 2 (`loom-daemon/src/telemetry/queue_snapshot.rs`) made each daemon
 * push its work finder's ranked ready queue as a host-scoped `queue.snapshot`
 * record on the `host.health` cadence, and only when the work finder has
 * ticked since the last one. This module is the Worker side of that record:
 *
 *  - [`normalizeQueueSnapshot`] narrows the untrusted payload to the known
 *    field set. Only what is listed here is stored, so an unexpected field a
 *    newer daemon adds cannot reach any response until this module names it.
 *  - [`shouldReplaceQueue`] keeps the Durable Object's `queue:<hostId>` entry
 *    on the newest tick. A redelivered or out-of-order older snapshot never
 *    overwrites a newer one and never refreshes `updatedAt`, so a stalled work
 *    finder cannot look live because an old batch was retried.
 *  - [`classifyAndPruneQueues`] attaches the same `live`/`stale`/`offline`
 *    freshness `host.health` uses, from the backend's own ingest clock.
 *  - [`redactQueueSnapshot`] is the public projection: per-row, keyed on each
 *    row's own `visibility`.
 *
 * # Freshness: empty vs. stale vs. absent
 *
 * The record exists to tell three states apart that a bare count cannot:
 *
 *  - no `queue` entry at all: the host has never sent one (a daemon older than
 *    phase 2, or the work finder is disabled);
 *  - a `live` entry with `seen: 0`: the work finder ticked recently and there
 *    is genuinely nothing to do;
 *  - a `stale`/`offline` entry: the last tick is old, so the counts are
 *    last-known, not current.
 *
 * `listing_failed` is a fourth, orthogonal flag: the tick ran, but some repos
 * could not be listed, so the queue is incomplete rather than empty.
 *
 * # Anti-leak
 *
 * The daemon already drops local paths and free-form error text. What remains
 * repo-identifying is `repo`, `issue`, `created_at`, `tier` and `detail` (a
 * park label or an open PR number). For a row whose `visibility` is not
 * exactly `"public"` an unauthenticated viewer gets none of them, only the
 * row's rank, state, disposition and the daemon's fixed reason text — plus,
 * since #9288, its plan `position`, `plan_state` and `gate`, which are
 * positions and gate names, not repo detail. The row's comparator `keys`
 * (which repeat `created_at` and the issue number), `repo_cap`,
 * `owning_shard`, `in_slice` and `hot` are withheld. The `counts` and the
 * per-tick `plan` block are kept: like a private sweep's phase, an aggregate
 * count or slot figure names no repository.
 */

import { classifyFreshness, PRUNE_AFTER_MS, type FreshnessInfo } from "./fleetState";
import { decodeVisibility, type RepoVisibility } from "./telemetry";

/** Coarse row state, as the daemon's `QueueDisposition::state` emits it.
 * Anything else (a newer daemon's state) is kept as `unknown` rather than
 * guessed into one of the three. */
export type QueueRowState = "running" | "ready" | "blocked" | "unknown";

/** Where a row stands in the host's dispatch plan (Issue #9288). */
export type QueuePlanState = "running" | "next" | "queued" | "blocked" | "unknown";

/** One comparator key that placed a row, in comparator order. */
export interface QueuePlanKey {
  name: string;
  value: string | number | boolean | null;
}

/** The host's dispatch-plan block for one tick (Issue #9288). */
export interface QueuePlan {
  slots: {
    max_concurrent?: number;
    occupancy?: number;
    free?: number;
    max_admissions_per_tick?: number;
    saturation_held: boolean;
    any_halted: boolean;
    /** Whether the host's single `loom:operator-priority` overflow slot
     * (#9244) is unused: a starred issue can still start past the
     * configured cap. Absent from a daemon older than #9318. */
    overflow_free?: boolean;
  };
  tick_interval_secs?: number;
  shard: { configured: boolean; host_shard?: number; shard_count?: number };
  /** Labels the plan covers; `loom:curated` / `loom:triage` are unordered. */
  scope: string[];
  /** Comparator key names, in order. */
  ordering: string[];
  complete: boolean;
}

export interface QueueRow {
  rank: number;
  repo?: string;
  visibility: RepoVisibility;
  issue?: number;
  workspace_priority?: number;
  /** Deprecated (#9244): always false from a current daemon. */
  urgent: boolean;
  /** Starred (`loom:operator-priority`, #9244). Absent when not starred. */
  operator_priority?: boolean;
  /** When it was starred, when the daemon knows. */
  operator_priority_at?: string;
  created_at?: string;
  tier?: string;
  disposition: string;
  state: QueueRowState;
  reason: string;
  detail?: string;
  /** Issue #9288 plan fields. Absent from a pre-#9288 daemon. */
  position?: number;
  plan_state?: QueuePlanState;
  keys?: QueuePlanKey[];
  gate?: string;
  in_slice?: boolean;
  hot?: boolean;
  owning_shard?: number;
  repo_cap?: { cap?: number; occupancy: number };
}

export interface QueueRepoRef {
  repo?: string;
  visibility: RepoVisibility;
}

export interface QueueCounts {
  running: number;
  ready: number;
  blocked: number;
}

/** The normalized `queue.snapshot` payload the Durable Object stores. */
export interface QueueSnapshot {
  kind: "queue.snapshot";
  tick_at: string;
  max_concurrent?: number;
  seen: number;
  counts: QueueCounts;
  listing_failed: QueueRepoRef[];
  listing_failed_unresolved: number;
  rows: QueueRow[];
  unresolved_rows: number;
  rows_truncated: number;
  /** The tick's dispatch plan (Issue #9288). Absent from older daemons. */
  plan?: QueuePlan;
  /** Public view only: rows whose repo-identifying fields were withheld. */
  withheld_rows?: number;
}

/** The `queue:<hostId>` Durable Object entry. `updatedAt` is the backend's
 * ingest clock, the same liveness signal `health:`/`tokens:` entries use. */
export interface HostQueueEntry {
  record: QueueSnapshot;
  updatedAt: string;
  freshness?: FreshnessInfo;
}

/** Most rows stored per host. The daemon already caps at its own `MAX_ROWS`
 * (200); this is the backend's own bound so a misbehaving sender cannot grow
 * the Durable Object entry without limit. */
export const MAX_STORED_ROWS = 200;

const ROW_STATES: readonly QueueRowState[] = ["running", "ready", "blocked"];
const PLAN_STATES: readonly QueuePlanState[] = ["running", "next", "queued", "blocked"];

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function count(value: unknown): number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : 0;
}

function optCount(value: unknown): number | undefined {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : undefined;
}

function strList(value: unknown): string[] {
  return Array.isArray(value) ? value.map(str).filter((s): s is string => s !== undefined) : [];
}

function normalizeKeys(value: unknown): QueuePlanKey[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const keys: QueuePlanKey[] = [];
  for (const entry of value) {
    if (!isObject(entry)) continue;
    const name = str(entry.name);
    const v = entry.value;
    if (name === undefined) continue;
    if (v === null || typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v))) {
      keys.push({ name, value: v });
    }
  }
  return keys.length > 0 ? keys : undefined;
}

function normalizeRepoCap(value: unknown): QueueRow["repo_cap"] {
  if (!isObject(value)) return undefined;
  const cap = optCount(value.cap);
  return cap === undefined ? { occupancy: count(value.occupancy) } : { cap, occupancy: count(value.occupancy) };
}

function bool(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

/** Narrow the #9288 plan block, or `undefined` when absent or malformed. */
export function normalizePlan(value: unknown): QueuePlan | undefined {
  if (!isObject(value)) return undefined;
  const slots = isObject(value.slots) ? value.slots : {};
  const shard = isObject(value.shard) ? value.shard : {};
  const plan: QueuePlan = {
    slots: { saturation_held: slots.saturation_held === true, any_halted: slots.any_halted === true },
    shard: { configured: shard.configured === true },
    scope: strList(value.scope),
    ordering: strList(value.ordering),
    complete: value.complete === true,
  };
  for (const key of ["max_concurrent", "occupancy", "free", "max_admissions_per_tick"] as const) {
    const n = optCount(slots[key]);
    if (n !== undefined) plan.slots[key] = n;
  }
  const overflowFree = bool(slots.overflow_free);
  if (overflowFree !== undefined) plan.slots.overflow_free = overflowFree;
  const hostShard = optCount(shard.host_shard);
  if (hostShard !== undefined) plan.shard.host_shard = hostShard;
  const shardCount = optCount(shard.shard_count);
  if (shardCount !== undefined) plan.shard.shard_count = shardCount;
  const interval = optCount(value.tick_interval_secs);
  if (interval !== undefined) plan.tick_interval_secs = interval;
  return plan;
}

function normalizeRow(value: unknown): QueueRow | undefined {
  if (!isObject(value)) return undefined;
  const rank = optCount(value.rank);
  if (rank === undefined) return undefined;
  const state = ROW_STATES.find((known) => known === value.state) ?? "unknown";
  const row: QueueRow = {
    rank,
    repo: str(value.repo),
    visibility: decodeVisibility(value.visibility),
    issue: optCount(value.issue),
    workspace_priority: optCount(value.workspace_priority),
    urgent: value.urgent === true,
    operator_priority: value.operator_priority === true ? true : undefined,
    operator_priority_at: str(value.operator_priority_at),
    created_at: str(value.created_at),
    tier: str(value.tier),
    disposition: str(value.disposition) ?? "unknown",
    state,
    reason: str(value.reason) ?? "",
    detail: str(value.detail),
    position: optCount(value.position),
    plan_state:
      value.plan_state === undefined ? undefined : (PLAN_STATES.find((known) => known === value.plan_state) ?? "unknown"),
    keys: normalizeKeys(value.keys),
    gate: str(value.gate),
    in_slice: bool(value.in_slice),
    hot: bool(value.hot),
    owning_shard: optCount(value.owning_shard),
    repo_cap: normalizeRepoCap(value.repo_cap),
  };
  for (const key of Object.keys(row) as (keyof QueueRow)[]) {
    if (row[key] === undefined) delete row[key];
  }
  return row;
}

function normalizeRepoRef(value: unknown): QueueRepoRef | undefined {
  if (!isObject(value)) return undefined;
  const repo = str(value.repo);
  return repo === undefined
    ? { visibility: decodeVisibility(value.visibility) }
    : { repo, visibility: decodeVisibility(value.visibility) };
}

/** Narrow an ingested `queue.snapshot` payload, or `undefined` when it has no
 * parseable `tick_at` (the freshness stamp everything else hangs off). */
export function normalizeQueueSnapshot(record: Record<string, unknown>): QueueSnapshot | undefined {
  const tickAt = str(record.tick_at);
  if (tickAt === undefined || !Number.isFinite(Date.parse(tickAt))) return undefined;

  const rows = Array.isArray(record.rows)
    ? record.rows.map(normalizeRow).filter((row): row is QueueRow => row !== undefined)
    : [];
  const overflow = Math.max(0, rows.length - MAX_STORED_ROWS);
  const counts = isObject(record.counts) ? record.counts : {};
  const maxConcurrent = optCount(record.max_concurrent);
  const plan = normalizePlan(record.plan);

  return {
    kind: "queue.snapshot",
    tick_at: tickAt,
    ...(maxConcurrent !== undefined && { max_concurrent: maxConcurrent }),
    seen: count(record.seen),
    counts: { running: count(counts.running), ready: count(counts.ready), blocked: count(counts.blocked) },
    listing_failed: Array.isArray(record.listing_failed)
      ? record.listing_failed.map(normalizeRepoRef).filter((ref): ref is QueueRepoRef => ref !== undefined)
      : [],
    listing_failed_unresolved: count(record.listing_failed_unresolved),
    rows: rows.slice(0, MAX_STORED_ROWS),
    unresolved_rows: count(record.unresolved_rows),
    rows_truncated: count(record.rows_truncated) + overflow,
    ...(plan !== undefined && { plan }),
  };
}

/** Whether `incoming` should replace the stored entry: only a strictly newer
 * tick does. Equal means a redelivery of the same snapshot; older means an
 * out-of-order retry. Neither may refresh the entry's `updatedAt`. */
export function shouldReplaceQueue(existing: HostQueueEntry | undefined, incoming: QueueSnapshot): boolean {
  if (!existing) return true;
  const previous = Date.parse(existing.record.tick_at);
  if (!Number.isFinite(previous)) return true;
  return Date.parse(incoming.tick_at) > previous;
}

/** Pure core of the Durable Object's `queue:` pass: classify each entry's
 * freshness and list the keys old enough to prune ([`PRUNE_AFTER_MS`], the
 * same horizon `health:`/`tokens:` entries use). */
export function classifyAndPruneQueues(
  entries: ReadonlyMap<string, HostQueueEntry>,
  now: Date = new Date(),
): { queues: Record<string, HostQueueEntry>; pruneKeys: string[] } {
  const queues: Record<string, HostQueueEntry> = {};
  const pruneKeys: string[] = [];
  for (const [key, value] of entries) {
    if (now.getTime() - Date.parse(value.updatedAt) > PRUNE_AFTER_MS) {
      pruneKeys.push(key);
      continue;
    }
    queues[key.slice("queue:".length)] = { ...value, freshness: classifyFreshness(value.updatedAt, now) };
  }
  return { queues, pruneKeys };
}

/** One row as an unauthenticated viewer may see it: unchanged when public,
 * reduced to non-identifying fields when private. */
export function redactQueueRow(row: QueueRow): QueueRow {
  if (row.visibility === "public") return row;
  const redacted: QueueRow = {
    rank: row.rank,
    visibility: "private",
    urgent: row.urgent,
    operator_priority: row.operator_priority,
    disposition: row.disposition,
    state: row.state,
    reason: row.reason,
  };
  // Plan position, state and gate place the row without naming it (#9288).
  if (row.position !== undefined) redacted.position = row.position;
  if (row.plan_state !== undefined) redacted.plan_state = row.plan_state;
  if (row.gate !== undefined) redacted.gate = row.gate;
  return redacted;
}

/** The public projection of a whole snapshot. Counts, `seen` and the tick
 * stamp survive; private rows and private failed-listing repos lose every
 * repo-identifying field but keep their place, so totals still add up. */
export function redactQueueSnapshot(record: QueueSnapshot): QueueSnapshot {
  const withheld = record.rows.filter((row) => row.visibility !== "public").length;
  return {
    ...record,
    listing_failed: record.listing_failed.map((ref) =>
      ref.visibility === "public" ? ref : { visibility: "private" as const },
    ),
    rows: record.rows.map(redactQueueRow),
    withheld_rows: withheld,
  };
}

/** Public projection of a raw `queue.snapshot` payload, for the history and
 * live-tail routes (`redaction.ts`'s per-kind derivation). A payload that does
 * not normalize contributes nothing. */
export function publicQueuePayload(payload: Record<string, unknown>): Record<string, unknown> {
  const normalized = normalizeQueueSnapshot(payload);
  if (!normalized) return {};
  const redacted = redactQueueSnapshot(normalized);
  return {
    rows: redacted.rows,
    listing_failed: redacted.listing_failed,
    withheld_rows: redacted.withheld_rows,
    ...(redacted.plan !== undefined && { plan: redacted.plan }),
  };
}
