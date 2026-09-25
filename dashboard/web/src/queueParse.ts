/**
 * Wire JSON → `QueueSnapshotRecord` (Issue #8852, phase 3). Same rules as
 * `parse.ts`: never throw, drop wrong-typed fields, and treat anything but
 * the exact string `"public"` as private.
 */

import type { QueueRepoRef, QueueRow, QueueRowState, QueueSnapshotRecord } from "./queueTypes";

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function nat(value: unknown): number | undefined {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 ? value : undefined;
}

function visibility(value: unknown): "public" | "private" {
  return typeof value === "string" && value.toLowerCase() === "public" ? "public" : "private";
}

const STATES: readonly QueueRowState[] = ["running", "ready", "blocked"];

export function parseQueueRow(value: unknown): QueueRow | undefined {
  if (!isObject(value)) return undefined;
  const rank = nat(value.rank);
  if (rank === undefined) return undefined;
  const row: QueueRow = {
    rank,
    visibility: visibility(value.visibility),
    urgent: value.urgent === true,
    disposition: str(value.disposition) ?? "unknown",
    state: STATES.find((known) => known === value.state) ?? "unknown",
    reason: str(value.reason) ?? "",
  };
  const repo = str(value.repo);
  if (repo !== undefined) row.repo = repo;
  const issue = nat(value.issue);
  if (issue !== undefined) row.issue = issue;
  const priority = nat(value.workspace_priority);
  if (priority !== undefined) row.workspace_priority = priority;
  const createdAt = str(value.created_at);
  if (createdAt !== undefined) row.created_at = createdAt;
  const tier = str(value.tier);
  if (tier !== undefined) row.tier = tier;
  const detail = str(value.detail);
  if (detail !== undefined) row.detail = detail;
  return row;
}

function parseRepoRef(value: unknown): QueueRepoRef | undefined {
  if (!isObject(value)) return undefined;
  const repo = str(value.repo);
  return repo === undefined ? { visibility: visibility(value.visibility) } : { repo, visibility: visibility(value.visibility) };
}

/** `undefined` when there is no parseable `tick_at` — without it the record
 * cannot say how current it is, so it is not rendered as if it were. */
export function parseQueueSnapshot(value: unknown): QueueSnapshotRecord | undefined {
  if (!isObject(value)) return undefined;
  const tickAt = str(value.tick_at);
  if (tickAt === undefined || !Number.isFinite(Date.parse(tickAt))) return undefined;
  const counts = isObject(value.counts) ? value.counts : {};
  const record: QueueSnapshotRecord = {
    tick_at: tickAt,
    seen: nat(value.seen) ?? 0,
    counts: {
      running: nat(counts.running) ?? 0,
      ready: nat(counts.ready) ?? 0,
      blocked: nat(counts.blocked) ?? 0,
    },
    listing_failed: Array.isArray(value.listing_failed)
      ? value.listing_failed.map(parseRepoRef).filter((ref): ref is QueueRepoRef => ref !== undefined)
      : [],
    listing_failed_unresolved: nat(value.listing_failed_unresolved) ?? 0,
    rows: Array.isArray(value.rows)
      ? value.rows.map(parseQueueRow).filter((row): row is QueueRow => row !== undefined)
      : [],
    unresolved_rows: nat(value.unresolved_rows) ?? 0,
    rows_truncated: nat(value.rows_truncated) ?? 0,
  };
  const maxConcurrent = nat(value.max_concurrent);
  if (maxConcurrent !== undefined) record.max_concurrent = maxConcurrent;
  const withheld = nat(value.withheld_rows);
  if (withheld !== undefined) record.withheld_rows = withheld;
  return record;
}
