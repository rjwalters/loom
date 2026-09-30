/**
 * The per-host work queue as `GET /api/fleet-state` returns it (Issue #8852,
 * phase 3): `hosts[<id>].queue`, the Durable Object's normalized copy of that
 * host's newest `queue.snapshot`. Source of truth is
 * `../../src/queueState.ts`; re-declared here for the same reason `types.ts`
 * re-declares the rest of the snapshot (no Workers types in a browser bundle).
 *
 * On `/public/fleet-state` a private row keeps only `rank`, `visibility`,
 * `urgent`, `operator_priority`, `disposition`, `state` and `reason` (plus
 * the #9288 plan `position`, `plan_state` and `gate`), so every
 * repo-identifying field here is optional.
 */

export type QueueRowState = "running" | "ready" | "blocked" | "unknown";

/** Where a row stands in its host's dispatch plan (Issue #9288). */
export type QueuePlanState = "running" | "next" | "queued" | "blocked" | "unknown";

export interface QueueRow {
  /** 1-based dispatch order on the host. Gaps mean the daemon dropped rows
   * (unresolvable repo slug). */
  rank: number;
  /** Forge `owner/repo`. Absent for a private row on the public view. */
  repo?: string;
  visibility: "public" | "private";
  issue?: number;
  workspace_priority?: number;
  /** Deprecated (#9244): always false. `loom:urgent` no longer orders work. */
  urgent: boolean;
  /** Starred (`loom:operator-priority`, #9244): dispatched ahead of all other
   * work. Absent when not starred or from an older daemon. */
  operator_priority?: boolean;
  /** When it was starred, when the daemon knows. */
  operator_priority_at?: string;
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
  /** 1-based position in the host's shaped dispatch plan (Issue #9288).
   * Per host: positions from two hosts are not comparable. Absent when the
   * row is not dispatchable on that host this tick, or from older daemons. */
  position?: number;
  plan_state?: QueuePlanState;
  /** Which admission gate holds a deferred row (`capacity`, `ramp`, …). */
  gate?: string;
  /** The shard that owns this row's workspace, when the fleet is sharded.
   * Compared against the reporting host's own `plan.shard.host_shard` by the
   * fleet merge (Issue #9310). Withheld from the public view. */
  owning_shard?: number;
}

/** The reporting host's shard posture for the tick (Issue #9288). */
export interface QueuePlanShard {
  configured: boolean;
  host_shard?: number;
  shard_count?: number;
}

/** The host's per-tick dispatch-plan block, narrowed to what the UI reads.
 * The daemon also sends `slots`, `ordering`, `scope` and `complete`; only
 * `shard` is re-declared here, because the fleet merge's `owning_shard`
 * tie-break is the one thing the browser needs it for (Issue #9310). */
export interface QueuePlanView {
  shard?: QueuePlanShard;
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
  /** This tick's dispatch plan. Absent from a pre-#9288 daemon. */
  plan?: QueuePlanView;
  /** Public view only: how many rows had their repo detail withheld. */
  withheld_rows?: number;
}
