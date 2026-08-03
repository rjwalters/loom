/**
 * The `GET /api/fleet-state` response contract, as consumed by this UI.
 *
 * **Source of truth is the backend, not this file**: the Durable Object that
 * produces this shape is `../../src/fleetState.ts` (`FleetSnapshot` /
 * `ActiveSweepState`), and it is documented in `../../docs/query-api.md`. The
 * types are re-declared here rather than imported across the package boundary
 * because the backend module is compiled against `@cloudflare/workers-types`
 * (it references the `DurableObject` / `DurableObjectState` globals), which a
 * browser bundle has no business depending on.
 *
 * Two deliberate divergences from the backend declarations:
 *
 * 1. **Everything the daemon may omit is optional here, and index signatures
 *    are open.** The telemetry schema (`.loom/docs/telemetry-schema.md`) is
 *    explicitly additive/forward-compatible: a host running an older daemon
 *    omits fields a newer one sends, and a newer daemon may send fields this
 *    build has never heard of. `parse.ts` narrows the wire JSON into these
 *    types field by field and drops anything wrong-typed, so a partial or
 *    future record degrades to "unknown" instead of throwing.
 *
 * 2. **`record` payloads are typed.** The backend stores them as opaque
 *    `Record<string, unknown>` (it does not interpret them); the UI does, so
 *    the two host-level record kinds it renders get real field types drawn
 *    from the schema doc's `host.health` / `tokens.snapshot` sections.
 */

/** One repository in a host's `managed_repos` roster (#4976). `slug` is
 * present for a public repo, or a private one on the authenticated view; the
 * public (unauthenticated) view drops `slug` for a private entry — the
 * backend's redaction keeps the entry (so its count still shows) but strips
 * the identifying field, mirroring `PublicActiveSweep`'s repo-nulling for a
 * private sweep (`dashboard/src/redaction.ts`). */
export interface ManagedRepoEntry {
  slug?: string;
  visibility?: "public" | "private";
}

/** One `(root, role)` pair's persistent tick-failure detail inside a host's
 * `roles` role-tick health summary (#5022). `root` is a filesystem workspace
 * path (not a forge repo slug) — it names no repository. */
export interface RoleTickFailure {
  root?: string;
  role?: string;
  failures?: number;
  last_at?: string;
  detail?: string;
}

/** `host.health`'s role-tick health summary (#5022) — the same
 * transient-vs-persistent classification `loom-daemon health`'s `roles`
 * section already computes, carried through the telemetry pipeline so a role
 * dying on one host is visible fleet-wide, not only to an operator who
 * happens to run `loom-daemon health` locally on that host.
 *
 * `total: 0` means "no role ticks sampled" (the role runner idle or
 * disabled) — a normal state, not an error — same as an empty `persistent`
 * list with `total > 0` means "every tick ok". Absent entirely on a record
 * from a pre-#5022 daemon. */
export interface RoleTickHealth {
  total?: number;
  ok?: number;
  persistent?: RoleTickFailure[];
}

/** `host.health` — CPU/disk headroom, daemon version, uptime.
 *
 * Every measured field is optional by design: the daemon's "unknown != zero"
 * contract says an unmeasurable probe is *absent*, never a fake `0`. The UI
 * must render an absent field as unknown (`—`), never as zero/full — see
 * `format.ts` and the tests that pin this. */
export interface HostHealthRecord {
  kind?: string;
  captured_at?: string;
  daemon_version?: string;
  /** Short git commit the emitting binary was built from (#4956). Absent on
   * records from a pre-#4956 daemon; `"unknown"` when the build host had no
   * git. The version alone only moves once per release, so this is what makes
   * two same-version builds distinguishable. */
  build_commit?: string;
  /** RFC-3339 instant the emitting binary was compiled (#4956). Absent when
   * the build stamp was unavailable — never a fabricated instant. */
  built_at?: string;
  uptime_sec?: number;
  logical_cpus?: number;
  cpu_idle_fraction?: number;
  load_per_core?: number;
  worktree_root_free_gb?: number;
  /** Whether this host's own dispatch is currently halted for a non-idle
   * reason — the host-distress breaker tripped `Open`/`CoolDown`, the
   * saturation hold, or the rate-limit breaker (Issue #4975). Absent on a
   * record from a pre-#4975 daemon, which is indistinguishable from `false`
   * — neither means "known to be healthy", just "not known to be halted". */
  dispatch_halted?: boolean;
  /** Human-readable reason for the halt (e.g. `"load-per-core 4.24 >= 2.50
   * sustained for 3 consecutive tick(s)"`), naming the specific breaker/gate
   * that tripped. Absent when `dispatch_halted` is absent/`false`. */
  halt_reason?: string;
  /** This host's managed-repository roster (#4976) — sourced from the
   * daemon's workspace registry, not inferred from in-flight sweeps, so an
   * idle-but-registered repo still appears. Absent entirely on a host
   * running a pre-#4976 daemon, or one with no registered workspaces. */
  managed_repos?: ManagedRepoEntry[];
  /** This host's role-tick health (#5022). Absent on a record from a
   * pre-#5022 daemon, which is indistinguishable from "nothing sampled yet"
   * — never render its absence as either healthy or degraded. */
  roles?: RoleTickHealth;
}

/** One account inside a `tokens.snapshot`. Only `exhausted` is always sent;
 * `rank` / `usage_fraction` / `limit_window_reset_at` are omitted when the
 * daemon does not know them.
 *
 * `limit_window_reset_at` is the instant the window *currently gating* the
 * account rolls over — the 7d window once it is `exhausted`, the 5h window
 * (the one `usage_fraction` measures) otherwise. The daemon resolves which,
 * so this side reads it as one thing: when the account's constraint lifts. */
export interface TokenAccount {
  account?: string;
  rank?: number;
  usage_fraction?: number;
  limit_window_reset_at?: string;
  exhausted?: boolean;
}

/** `tokens.snapshot` — point-in-time view of the multi-account token pool.
 *
 * **Two shapes, by audience.** An authenticated viewer gets `accounts`: the
 * per-account rows. A public viewer gets the `*_count` / `*_usage_fraction`
 * aggregate instead, and no `accounts` at all — the account identifiers and
 * their individual burn stay behind the Access gate (`../../src/redaction.ts`,
 * `deriveTokenPoolAggregate`). Both shapes are optional here because either
 * audience sees only one of them; `summarizeTokens` in `fleet.ts` normalizes
 * the two into one `TokenSummary` so views do not branch on audience. */
export interface TokensSnapshotRecord {
  kind?: string;
  captured_at?: string;
  accounts?: TokenAccount[];
  account_count?: number;
  exhausted_count?: number;
  mean_usage_fraction?: number | null;
  max_usage_fraction?: number | null;
  next_limit_window_reset_at?: string | null;
}

/** A record plus when the Durable Object last applied it. `updatedAt` is
 * backend-clock ingest time, distinct from the record's own `captured_at`
 * (daemon clock) — staleness is computed from `updatedAt` so a host with a
 * skewed clock cannot look fresh. */
export interface Timestamped<T> {
  record: T;
  updatedAt: string;
}

export interface HostEntry {
  health?: Timestamped<HostHealthRecord>;
  tokens?: Timestamped<TokensSnapshotRecord>;
}

/** One in-flight sweep. Mirrors `ActiveSweepState` in `../../src/fleetState.ts`.
 *
 * `hostId`/`sweepId`/`updatedAt` are always present there; the rest depend on
 * which lifecycle records have arrived so far (a sweep that has emitted
 * `sweep.started` but no `sweep.phase` yet has no `phase`/`enteredPhaseAt`). */
export interface ActiveSweep {
  hostId: string;
  sweepId: string;
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  phase?: string;
  startedAt?: string;
  enteredPhaseAt?: string;
  model?: string;
  effort?: string;
  updatedAt?: string;
}

export interface FleetSnapshot {
  hosts: Record<string, HostEntry>;
  activeSweeps: ActiveSweep[];
}

// ---------------------------------------------------------------------------
// Live-tail / history wire types (`GET /api/events`, `GET /api/history`)
// ---------------------------------------------------------------------------
//
// The types above describe the *aggregated* `GET /api/fleet-state` snapshot the
// overview and host-detail views render. The types below describe the *raw
// telemetry record* stream that the live event feed and the per-sweep timeline
// consume (#4750): the envelope + record kinds documented in
// `.loom/docs/telemetry-schema.md` (Rust source of truth) and the SSE frame
// shape documented in `../../docs/query-api.md`'s `GET /api/events` section.
//
// They are kept intentionally loose (`Record<string, unknown>` plus known
// fields) since `record` is the verbatim ingested JSON payload and new fields
// are additive across schema versions. The two sets are deliberately disjoint —
// one file, two contracts, no shared names.

/** The terminal results `sweep.completed`/`sweep.outcome`'s `result` field
 * takes on (`dashboard/docs/query-api.md`'s `result` query-param table). */
export type SweepResult = "success" | "failure" | "cancelled" | "blocked";

/** Runtime narrowing for `SweepResult`, used by transforms that read `result`
 * out of a `record` payload (which is verbatim ingested JSON, so its `result`
 * arrives as an unvalidated `unknown`). */
export function isSweepResult(value: unknown): value is SweepResult {
  return value === "success" || value === "failure" || value === "cancelled" || value === "blocked";
}

export type SweepPhaseName = "curator" | "builder" | "judge" | "doctor" | "merge";

/** The record kinds this frontend cares about (a subset of the full schema). */
export type RecordKind =
  | "sweep.started"
  | "sweep.phase"
  | "sweep.completed"
  | "sweep.outcome"
  | "tokens.snapshot"
  | "host.health";

export interface SweepStartedRecord {
  kind: "sweep.started";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  started_at: string;
  model?: string;
  effort?: string;
}

export interface SweepPhaseRecord {
  kind: "sweep.phase";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  phase: SweepPhaseName | string;
  entered_at: string;
}

export interface SweepCompletedRecord {
  kind: "sweep.completed";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  completed_at: string;
  result: SweepResult;
}

export interface SweepPhaseDuration {
  phase: SweepPhaseName | string;
  duration_sec: number;
}

export interface SweepOutcomeRecord {
  kind: "sweep.outcome";
  repo?: string;
  visibility?: "public" | "private";
  issue?: number;
  sweep_id: string;
  model?: string;
  effort?: string;
  config?: Record<string, string>;
  phase_durations?: SweepPhaseDuration[];
  total_duration_sec?: number;
  result: SweepResult;
  pr_number?: number;
}

/** Any record kind not modeled above — passed through opaquely. */
export interface OtherRecord {
  kind: string;
  [key: string]: unknown;
}

export type TelemetryRecord =
  | SweepStartedRecord
  | SweepPhaseRecord
  | SweepCompletedRecord
  | SweepOutcomeRecord
  | OtherRecord;

/** The `event` object nested inside every `GET /api/events` SSE frame. */
export interface LiveTailEvent {
  hostId: string;
  emittedAt: string;
  schemaVersion: number;
  record: TelemetryRecord;
}

/** One parsed `data:` frame from the live-tail SSE stream. */
export interface LiveTailFrame {
  topic: string;
  event: LiveTailEvent;
}

/**
 * A single row from `GET /api/history`'s (or `GET /public/history`'s)
 * `records` array, camelCased exactly as the API returns it.
 *
 * On `/public/history`, a `visibility: "private"` row has `repo`/`issue`/
 * `sweepId` nulled and `record` reduced to a per-`kind` field allowlist —
 * see `dashboard/src/redaction.ts`. Transforms over these rows must
 * therefore tolerate missing fields rather than trust `record`'s declared
 * shape, so that the same code works against either route with no
 * redaction-awareness of its own.
 */
export interface HistoryRecord {
  id: number;
  schemaVersion: number;
  emittedAt: string;
  hostId: string;
  kind: string;
  repo?: string | null;
  visibility?: "public" | "private" | null;
  issue?: number | null;
  sweepId?: string | null;
  ingestedAt: string;
  record: TelemetryRecord;
}

/** One page of `GET /api/history` / `GET /public/history` — mirrors
 * `dashboard/src/query.ts`'s `HistoryQueryResult`. */
export interface HistoryQueryResult {
  records: HistoryRecord[];
  /** `id` of the last record on this page, or `null` at the end of the
   * matching result set. Pass back as `?cursor=` to fetch the next page. */
  nextCursor: number | null;
}
