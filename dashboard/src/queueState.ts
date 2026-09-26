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
 * row's rank, state, disposition and the daemon's fixed reason text. The
 * `counts` are true totals and are kept: like a private sweep's phase, an
 * aggregate count names no repository.
 */

import { classifyFreshness, PRUNE_AFTER_MS, type FreshnessInfo } from "./fleetState";
import { decodeVisibility, type RepoVisibility } from "./telemetry";

/** Coarse row state, as the daemon's `QueueDisposition::state` emits it.
 * Anything else (a newer daemon's state) is kept as `unknown` rather than
 * guessed into one of the three. */
export type QueueRowState = "running" | "ready" | "blocked" | "unknown";

export interface QueueRow {
  rank: number;
  repo?: string;
  visibility: RepoVisibility;
  issue?: number;
  workspace_priority?: number;
  urgent: boolean;
  created_at?: string;
  tier?: string;
  disposition: string;
  state: QueueRowState;
  reason: string;
  detail?: string;
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
    created_at: str(value.created_at),
    tier: str(value.tier),
    disposition: str(value.disposition) ?? "unknown",
    state,
    reason: str(value.reason) ?? "",
    detail: str(value.detail),
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
  return {
    rank: row.rank,
    visibility: "private",
    urgent: row.urgent,
    disposition: row.disposition,
    state: row.state,
    reason: row.reason,
  };
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
  return { rows: redacted.rows, listing_failed: redacted.listing_failed, withheld_rows: redacted.withheld_rows };
}
