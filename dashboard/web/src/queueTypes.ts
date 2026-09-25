/**
 * The per-host work queue as `GET /api/fleet-state` returns it (Issue #8852,
 * phase 3): `hosts[<id>].queue`, the Durable Object's normalized copy of that
 * host's newest `queue.snapshot`. Source of truth is
 * `../../src/queueState.ts`; re-declared here for the same reason `types.ts`
 * re-declares the rest of the snapshot (no Workers types in a browser bundle).
 *
 * On `/public/fleet-state` a private row keeps only `rank`, `visibility`,
 * `urgent`, `disposition`, `state` and `reason`, so every repo-identifying
 * field here is optional.
 */

export type QueueRowState = "running" | "ready" | "blocked" | "unknown";

export interface QueueRow {
  /** 1-based dispatch order on the host. Gaps mean the daemon dropped rows
   * (unresolvable repo slug). */
  rank: number;
  /** Forge `owner/repo`. Absent for a private row on the public view. */
  repo?: string;
  visibility: "public" | "private";
  issue?: number;
  workspace_priority?: number;
  urgent: boolean;
  /** The issue's own `createdAt` — the only age the daemon knows. */
  created_at?: string;
  /** Informational `tier:*` label; the daemon does not order by it. */
  tier?: string;
  /** The daemon's snake_case `QueueDisposition` (`dispatched`, `parked`, …). */
  disposition: string;
  state: QueueRowState;
  /** The daemon's fixed human-readable reason for the disposition. */
  reason: string;
  /** The park label (`parked`) or the open PR number (`open_pr`). */
  detail?: string;
}

export interface QueueRepoRef {
  repo?: string;
  visibility: "public" | "private";
}

export interface QueueSnapshotRecord {
  /** When the work finder tick this describes completed (daemon clock). */
  tick_at: string;
  max_concurrent?: number;
  /** Ready issues the tick listed — the host's backlog. */
  seen: number;
  /** True totals over every row, including dropped ones. */
  counts: { running: number; ready: number; blocked: number };
  /** Repos whose listing failed this tick: the queue is incomplete. */
  listing_failed: QueueRepoRef[];
  listing_failed_unresolved: number;
  rows: QueueRow[];
  unresolved_rows: number;
  rows_truncated: number;
  /** Public view only: how many rows had their repo detail withheld. */
  withheld_rows?: number;
}
