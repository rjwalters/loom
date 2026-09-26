/**
 * `FleetState` Durable Object — the live "what is running right now across
 * every host" snapshot (Epic #4702, Phase 2 AC: "Durable Object holds
 * current live fleet state (updated per ingested record), independent of
 * D1 history"). Analogous to what `loom-daemon serve`'s in-process state
 * provides for a single host today, but aggregated across the whole fleet.
 *
 * A single global instance is used (see `FLEET_STATE_ID` in `src/index.ts`)
 * — the live-state working set (per-host health/tokens plus currently
 * in-flight sweeps) is small enough that one Durable Object's storage is
 * more than sufficient, and a singleton keeps "read the current fleet
 * state" a single object lookup rather than a fan-out across N per-host
 * objects.
 *
 * Storage layout (five key prefixes, iterated via `list({ prefix })` to
 * build a snapshot):
 *   `health:<hostId>`  → latest `host.health` record + when it was applied.
 *   `tokens:<hostId>`  → latest `tokens.snapshot` record + when applied.
 *   `queue:<hostId>`   → newest `queue.snapshot` (the work finder's ranked
 *                        ready queue, Issue #8852) — see `./queueState.ts`.
 *   `sweep:<sweepId>`  → the in-flight sweep's current known state; removed
 *                        entirely on `sweep.completed` (a finished sweep is
 *                        not "live" — its full history lives in D1).
 *   `compute:<jobId>`  → a currently-running ephemeral compute job (Issue
 *                        #8305); removed on that job's completion record.
 *
 * This D1-vs-DO split is deliberate: D1 answers "what happened", the DO
 * answers "what is happening right now", and the DO is never treated as a
 * source of truth for history — see `src/index.ts`'s ingest handler, which
 * always writes D1 first and treats a DO update failure as best-effort.
 *
 * # Leaked `sweep:` entries (Issue #4955)
 *
 * A `sweep:` entry is removed only on a `sweep.completed` record — but that
 * record can be lost (most commonly across a daemon restart whose reaper-
 * driven terminal transition does not get re-exported), leaving a phantom
 * "still in flight" entry forever. Two independent, additive backstops:
 *
 *  1. **Staleness bound** ([`STALE_SWEEP_MS`]): `buildSnapshot` excludes (and
 *     lazily deletes) any `sweep:` entry whose `updatedAt` is older than the
 *     bound. `sweep.phase` records flow regularly during a real sweep, so a
 *     silent multi-hour-old entry is dead. Self-contained — needs nothing
 *     from the daemon side — but only self-heals after the bound elapses.
 *  2. **Per-host reconciliation**: `applyUpdate`'s `host.health` case reads
 *     the record's `active_sweep_ids` (Issue #4955, `HostHealthRecord` on the
 *     daemon side) and deletes every `sweep:` entry for that `hostId` NOT in
 *     the set — ground truth wins within one health cadence, covering even a
 *     crash-lost completion immediately. **Only** applied when the field is
 *     present and non-empty: an absent field (a pre-#4955 daemon) or an
 *     empty one (the daemon's registry is not yet authoritative, e.g. still
 *     rebuilding right after its own restart) must never be read as "zero
 *     sweeps running" — that would wipe every legitimately-live entry for
 *     the host instead of fixing the leak.
 *
 * # Live `compute:` entries (Issue #8305, Phase 2 of #8257)
 *
 * 2am's elastic EDA batch runner emits **two** `ephemeral_compute` records
 * per job — a launch-time one and, separately, a completion-time one for the
 * same `job_id` (see `migrations/0003_ephemeral_compute.sql`'s "No per-job
 * dedup index" section) — which is exactly the `sweep.started` /
 * `sweep.completed` shape above, so it is handled the same way: the launch
 * record creates a `compute:<jobId>` entry, the completion record deletes it.
 * Both records share `kind: "ephemeral_compute"` (there is no `.started` /
 * `.completed` sub-kind), so the two are told apart by the presence of
 * `ended_at` on the payload.
 *
 * Leak handling differs from `sweep:` in two deliberate ways:
 *
 *  - **A much wider bound** ([`STALE_COMPUTE_MS`], 24h vs. `STALE_SWEEP_MS`'s
 *    4h). Nothing refreshes a `compute:` entry between its launch and its
 *    completion — there is no `sweep.phase` analogue — so its `updatedAt` is
 *    effectively the job's launch time, and the bound has to clear the
 *    longest plausible job wall-clock rather than the longest plausible gap
 *    between heartbeats.
 *  - **Flag, then prune** rather than prune-on-sight. A crossed bound marks
 *    the entry `leaked: true` in the snapshot (parent AC 4) and keeps
 *    returning it — a leaked job must be *distinguishable* from one that
 *    closed normally, and a normally-closed job is already gone from the
 *    snapshot entirely. Only after [`PRUNE_COMPUTE_AFTER_MS`] is the entry
 *    deleted, so the DO's working set still stays bounded.
 */

import {
  classifyAndPruneQueues,
  normalizeQueueSnapshot,
  shouldReplaceQueue,
  type HostQueueEntry,
} from "./queueState";

export interface ActiveSweepState {
  hostId: string;
  sweepId: string;
  repo?: string;
  visibility: "public" | "private";
  issue?: number;
  phase?: string;
  startedAt?: string;
  enteredPhaseAt?: string;
  model?: string;
  effort?: string;
  /** Runtime adapter the sweep was dispatched on (`claude`, `codex`, …),
   * from `sweep.started`'s `runtime`. Absent for a pre-runtime daemon. */
  runtime?: string;
  /** Resolved launch provider, distinct from the runtime adapter. */
  provider?: string;
  updatedAt: string;
}

/**
 * One currently-running ephemeral compute job (Issue #8305) — the
 * `compute:<jobId>` live-state entry, built from an `ephemeral_compute`
 * launch record.
 *
 * Field names are the camelCase projection of the payload's snake_case ones
 * (`job_id` → `jobId`, …), matching how [`ActiveSweepState`] projects a
 * `sweep.started` record. The cost/duration fields (`wall_clock_sec`,
 * `estimated_cost_usd`) are deliberately absent: they only exist on the
 * *completion* record, which removes this entry rather than updating it —
 * D1 is where a finished job's cost is read from.
 */
export interface ActiveComputeState {
  /** The host whose emitter reported the job — not necessarily where the
   * instance itself runs (that is `region`/`instanceId`). */
  hostId: string;
  jobId: string;
  /** The sweep that submitted this job, when the emitter stamped one
   * (Issue #8835). This — never `hostId` — is what the dashboard joins on to
   * nest a job under the sweep paying for it: `hostId` is the *submitter's*
   * ingest identity (one synthetic id for a whole hostless elastic fleet),
   * which need not match the sweep's own host at all.
   *
   * Absent for a job submitted outside any sweep, and for one submitted by an
   * emitter that predates this field — both of which must keep rendering in
   * the flat "running compute" list rather than being dropped. */
  sweepId?: string;
  instanceId?: string;
  region?: string;
  instanceType?: string;
  spot?: boolean;
  ami?: string;
  /** The payload's own `started_at`, i.e. the emitter's clock. Kept for
   * display only — never used for leak detection, which uses `updatedAt`
   * (this backend's own ingest clock) so a skewed host clock cannot fake
   * liveness. */
  startedAt?: string;
  /** When this backend first applied a launch record for `jobId`. A re-sent
   * launch record deliberately does NOT refresh it — see `applyUpdate`. */
  updatedAt: string;
  /** Derived at snapshot time by [`classifyComputeEntries`], never stored:
   * `true` once the entry has gone [`STALE_COMPUTE_MS`] without a completion
   * record. */
  leaked?: boolean;
}

/**
 * Staleness classification for a `health:`/`tokens:` entry, derived purely
 * from how long ago the Durable Object applied it (`updatedAt` — the
 * backend's own ingest clock, not the daemon's `captured_at`, so a
 * skewed host clock can never fake liveness).
 *
 * Boundaries (issue #4957 — "dashboard renders last-known host state as
 * current forever"):
 *
 *   - `live`    — within [`LIVE_AFTER_SEC`], roughly 2x the daemon's
 *     ~5-minute `host.health`/`tokens.snapshot` sampling cadence
 *     (`SNAPSHOT_INTERVAL` in `loom-daemon/src/observability/mod.rs`,
 *     documented at `dashboard/docs/deploy-runbook.md` §10) — long enough
 *     that ordinary batching/flush jitter never flickers a healthy host to
 *     STALE, short enough to notice a genuinely stalled daemon quickly.
 *     Matches the "3 missed samples" reasoning `dashboard/web/src/fleet.ts`'s
 *     own (finer-grained) `STALE_AFTER_SEC` badge already uses.
 *   - `stale`   — up to [`OFFLINE_AFTER_SEC`]: no longer "current", but a
 *     single dropped push, a host asleep overnight, or a brief network blip
 *     is not yet "gone".
 *   - `offline` — beyond that: the daemon has very likely stopped, the host
 *     is asleep/powered off, or it lost its tailnet — its last-known
 *     numbers must never be presented as current (see `publicPage.ts`).
 */
export type HostFreshness = "live" | "stale" | "offline";

/** How old an entry may be and still read as `live`. */
export const LIVE_AFTER_SEC = 15 * 60;
/** Beyond this, an entry reads as `offline` rather than merely `stale`. */
export const OFFLINE_AFTER_SEC = 4 * 60 * 60;
/** Entries older than this are pruned from the Durable Object entirely on
 * the next [`FleetState.buildSnapshot`] — long enough that a host asleep
 * over a long weekend does not vanish, short enough that a decommissioned
 * host does not linger forever (issue #4957 AC: "long-gone hosts age out of
 * the DO entirely"). */
export const PRUNE_AFTER_MS = 7 * 24 * 60 * 60 * 1000;

export interface FreshnessInfo {
  status: HostFreshness;
  /** Seconds since `updatedAt`, floored at `0`. `Number.POSITIVE_INFINITY`
   * for an unparseable `updatedAt` (never treated as fresh). */
  ageSeconds: number;
}

/** Classify one `updatedAt` timestamp's freshness as of `now`. Exported so
 * both this module's `buildSnapshot` and `publicPage.ts`'s rendering share
 * exactly one cadence/boundary policy — see the module doc above. */
export function classifyFreshness(updatedAt: string, now: Date = new Date()): FreshnessInfo {
  const ageMs = now.getTime() - Date.parse(updatedAt);
  if (!Number.isFinite(ageMs)) {
    return { status: "offline", ageSeconds: Number.POSITIVE_INFINITY };
  }
  const ageSeconds = Math.max(0, Math.round(ageMs / 1000));
  const status: HostFreshness =
    ageSeconds <= LIVE_AFTER_SEC ? "live" : ageSeconds <= OFFLINE_AFTER_SEC ? "stale" : "offline";
  return { status, ageSeconds };
}

/** `true` once an entry is old enough to be pruned from the Durable Object
 * entirely — see [`PRUNE_AFTER_MS`]. An unparseable `updatedAt` is never
 * pruned by this check (a `NaN` age fails every numeric comparison), which
 * is the fail-safe direction: a malformed timestamp should surface as
 * `offline` via [`classifyFreshness`], not silently vanish. */
function isPruneable(updatedAt: string, now: Date): boolean {
  return now.getTime() - Date.parse(updatedAt) > PRUNE_AFTER_MS;
}

type TimestampedEntry = { record: Record<string, unknown>; updatedAt: string };

/** Pure core of [`FleetState.buildSnapshot`]'s host-classification/pruning
 * step, split out so it is unit-testable without spinning up a Durable
 * Object (issue #4957's test plan: "unit test buildSnapshot() classifies a
 * health/tokens entry as LIVE/STALE/OFFLINE correctly at boundary ages").
 * Takes the raw `storage.list()` results for both prefixes and returns the
 * classified `hosts` map plus the full storage keys (`health:<hostId>` /
 * `tokens:<hostId>`) that are old enough to prune — the instance method
 * below is the only thing that actually touches `this.state.storage`.
 */
export function classifyAndPruneHosts(
  healthEntries: ReadonlyMap<string, TimestampedEntry>,
  tokenEntries: ReadonlyMap<string, TimestampedEntry>,
  now: Date = new Date(),
): { hosts: FleetSnapshot["hosts"]; pruneKeys: string[] } {
  const hosts: FleetSnapshot["hosts"] = {};
  const pruneKeys: string[] = [];

  for (const [key, value] of healthEntries) {
    if (isPruneable(value.updatedAt, now)) {
      pruneKeys.push(key);
      continue;
    }
    const hostId = key.slice("health:".length);
    hosts[hostId] ??= {};
    hosts[hostId].health = { ...value, freshness: classifyFreshness(value.updatedAt, now) };
  }
  for (const [key, value] of tokenEntries) {
    if (isPruneable(value.updatedAt, now)) {
      pruneKeys.push(key);
      continue;
    }
    const hostId = key.slice("tokens:".length);
    hosts[hostId] ??= {};
    hosts[hostId].tokens = { ...value, freshness: classifyFreshness(value.updatedAt, now) };
  }

  return { hosts, pruneKeys };
}

export interface FleetSnapshot {
  hosts: Record<
    string,
    {
      // `freshness` is optional on the *type* (older callers/fixtures that
      // predate issue #4957 construct a bare `{ record, updatedAt }`) even
      // though `FleetState.buildSnapshot` always populates it today —
      // `publicPage.ts` recomputes it from `updatedAt` at render time via
      // `classifyFreshness` regardless, rather than trusting this field, so
      // its absence never silently hides a stale sample's age.
      health?: { record: Record<string, unknown>; updatedAt: string; freshness?: FreshnessInfo };
      tokens?: { record: Record<string, unknown>; updatedAt: string; freshness?: FreshnessInfo };
      /** The host's work-finder ready queue (Issue #8852) — see `./queueState.ts`. */
      queue?: HostQueueEntry;
    }
  >;
  activeSweeps: ActiveSweepState[];
  /** Currently-running ephemeral compute jobs (Issue #8305). Each entry
   * carries a `leaked` flag set by [`classifyComputeEntries`]. */
  activeCompute: ActiveComputeState[];
  /** Roster-expected hosts with no `health` entry at all (Issue #8792) —
   * see [`diffExpectedRoster`]. Never produced by the Durable Object itself
   * (it has no notion of an expected roster); the Worker attaches it in
   * `src/index.ts`'s `fetchLiveFleetSnapshot`. Optional on the type for the
   * same reason `freshness` is: fixtures/callers that predate it, and a
   * deployment with no `EXPECTED_HOSTS` configured, omit it. */
  missingHosts?: MissingHost[];
}

// ---------------------------------------------------------------------------
// Expected-host roster (Issue #8792)
// ---------------------------------------------------------------------------

/**
 * Why a roster-expected host has no `health` entry to render.
 *
 *   - `missing`       — the backend holds an active (non-revoked) ingest key
 *     for it, yet the Durable Object has no `health:` entry: the host was
 *     provisioned and has never reported (e.g. a daemon that predates
 *     telemetry export, #5083, or a misconfigured `[observability]` block),
 *     or it went silent long enough ago that [`PRUNE_AFTER_MS`] aged its
 *     last entry out. Either way it is an *incident*: something that should
 *     be reporting is not.
 *   - `unprovisioned` — no active ingest key exists for it (no `hosts` row in
 *     D1, or only a revoked one): the host was added to the roster but has
 *     not been enrolled yet (`POST /admin/hosts`), so it *cannot* report.
 *     A to-do, not an outage — rendered distinctly from `missing` so a
 *     freshly-planned host is never mistaken for one that was live and
 *     stopped.
 *
 * A host that reported and then went quiet (within the prune horizon) is
 * neither: it still has a `health` entry and reads as `stale`/`offline` via
 * [`classifyFreshness`].
 */
export type MissingHostState = "missing" | "unprovisioned";

export interface MissingHost {
  hostId: string;
  state: MissingHostState;
}

/**
 * Parse the operator-supplied expected-host roster (the `EXPECTED_HOSTS`
 * Worker var — see `src/index.ts`'s `Env`). Accepts host IDs separated by
 * commas and/or any whitespace (so a YAML-folded or one-per-line list pastes
 * in unchanged); blanks are dropped, duplicates collapsed, and the result is
 * sorted for stable rendering. Unset/empty yields `[]` — "no roster
 * configured", under which the dashboard behaves exactly as it did before
 * this knob existed.
 */
export function parseExpectedHostRoster(raw: string | undefined): string[] {
  if (!raw) return [];
  const ids = new Set(
    raw
      .split(/[\s,]+/)
      .map((id) => id.trim())
      .filter((id) => id.length > 0),
  );
  return Array.from(ids).sort();
}

/**
 * The roster-vs-reporting diff (Issue #8792): every roster host with no
 * `health` entry in `hosts`, classified as `missing` or `unprovisioned` (see
 * [`MissingHostState`]).
 *
 * `activeKeyHostIds` is the subset of `roster` holding a **non-revoked**
 * ingest key in D1 — passed in rather than queried here so this stays a pure,
 * directly-testable function (same split as [`classifyAndPruneHosts`]).
 *
 * Deliberately one-directional: the roster only ever *adds* rows, never
 * hides one. A reporting host absent from the roster still renders normally,
 * and a host *removed* from the roster simply stops being diffed — so
 * decommissioning a host by dropping it from the roster reads as intentional
 * rather than as a new, permanent `missing` incident.
 */
export function diffExpectedRoster(
  hosts: FleetSnapshot["hosts"],
  roster: readonly string[],
  activeKeyHostIds: ReadonlySet<string>,
): MissingHost[] {
  const missing: MissingHost[] = [];
  for (const hostId of roster) {
    if (hosts[hostId]?.health) continue;
    missing.push({ hostId, state: activeKeyHostIds.has(hostId) ? "missing" : "unprovisioned" });
  }
  return missing;
}

/**
 * Drop any `hosts` entry belonging to a hostId D1 has recorded as revoked
 * (Issue #5078, mechanism 2 — "a host known only from a stale DO record").
 *
 * `handleRevokeHost` (`src/index.ts`) writes D1's `revoked_at` unconditionally,
 * then best-effort clears this DO's `health:`/`tokens:` entries for that host
 * — that cleanup fetch is wrapped in try/catch specifically so a transient DO
 * failure can't fail the revoke itself (see `FleetState.removeHost`'s doc
 * comment), which means a failed cleanup can leave those entries in the DO
 * indefinitely. Rather than rely on that best-effort cleanup having
 * succeeded, `src/index.ts` treats D1's `revoked_at` as authoritative at
 * *read* time too: a snapshot never renders a revoked host as live, whether
 * or not its DO-side cleanup actually ran.
 *
 * Deliberately does **not** touch `activeSweeps` — a sweep still reporting
 * against a host revoked mid-run is a real anomaly (`removeHost`'s own doc
 * comment: "an in-flight sweep on a host being revoked mid-run is a real
 * anomaly worth surfacing"), not something this filter should hide; it stays
 * visible (as an unattributed sweep, once the host's `hosts` entry above is
 * gone — see `publicPage.ts`'s `renderFleetOverview`). `activeCompute`
 * (Issue #8305) is passed through untouched for the same reason, and with an
 * extra one of its own: a still-running cloud instance costs money whether or
 * not its reporting host was revoked, so hiding it is exactly backwards.
 */
export function filterRevokedHosts(
  snapshot: FleetSnapshot,
  revokedHostIds: ReadonlySet<string>,
): FleetSnapshot {
  if (revokedHostIds.size === 0) return snapshot;
  const hosts: FleetSnapshot["hosts"] = {};
  for (const [hostId, entry] of Object.entries(snapshot.hosts)) {
    if (!revokedHostIds.has(hostId)) {
      hosts[hostId] = entry;
    }
  }
  return {
    hosts,
    activeSweeps: snapshot.activeSweeps,
    activeCompute: snapshot.activeCompute,
    ...(snapshot.missingHosts && { missingHosts: snapshot.missingHosts }),
  };
}

/** Body accepted by the internal `POST /update` route — one record's worth
 * of live-state effect, already authenticated/validated by the Worker
 * before it reaches the Durable Object. */
export interface FleetStateUpdate {
  hostId: string;
  record: Record<string, unknown>;
}

/** A `sweep:` entry whose `updatedAt` is older than this is considered dead
 * (Issue #4955, fix layer 1) — comfortably above any realistic sweep
 * duration (a full Curator→Builder→Judge→Doctor→Merge lifecycle is
 * typically well under an hour) so a genuinely long-running Builder phase is
 * never prematurely dropped, while still bounding how long a lost
 * `sweep.completed` can leak a phantom "in flight" entry. */
export const STALE_SWEEP_MS = 4 * 60 * 60 * 1000; // 4 hours

/** Fix layer 1's pure decision (Issue #4955), split out of `buildSnapshot`
 * so it is directly unit-testable without a Durable Object / storage layer
 * — and so `buildSnapshot` itself only has to reason about doing the I/O
 * (list once, act on the result), not the staleness math.
 *
 * An unparseable `updatedAt` decodes to `NaN` from `Date.parse`, which is
 * treated as NOT stale (fail open, kept in the snapshot) — matches the
 * "unknown is not evidence of staleness" posture the rest of the pipeline
 * uses for unmeasurable/malformed data.
 */
export function isSweepEntryStale(
  entry: ActiveSweepState,
  nowMs: number,
  staleMs: number = STALE_SWEEP_MS,
): boolean {
  const updatedAtMs = Date.parse(entry.updatedAt);
  return Number.isFinite(updatedAtMs) && nowMs - updatedAtMs > staleMs;
}

/** Fix layer 1: the storage keys of every stale entry in `entries`, given
 * `nowMs` (injected rather than read internally so this stays a pure
 * function of its inputs — see [`isSweepEntryStale`]'s doc comment). */
export function selectStaleSweepKeys(
  entries: Iterable<[string, ActiveSweepState]>,
  nowMs: number,
  staleMs: number = STALE_SWEEP_MS,
): string[] {
  const staleKeys: string[] = [];
  for (const [key, entry] of entries) {
    if (isSweepEntryStale(entry, nowMs, staleMs)) {
      staleKeys.push(key);
    }
  }
  return staleKeys;
}

/** Fix layer 2's pure decision (Issue #4955): the storage keys of every
 * `entries` entry that belongs to `hostId` but is NOT in `rawActiveSweepIds`
 * — i.e. every `sweep:` entry this reconciliation pass should delete.
 *
 * `rawActiveSweepIds` is `unknown` because it is a field lifted straight out
 * of the untyped `record` the Worker forwards from `/ingest` — never assumed
 * to be well-formed. Returns `[]` (a no-op — never deletes anything) unless
 * it decodes to a genuinely **non-empty** array of strings: an absent field
 * (a pre-#4955 daemon), an empty array (the daemon's own registry is not yet
 * authoritative, e.g. still rebuilding right after its own restart), or an
 * array with no valid string elements must never be read as "this host has
 * zero sweeps running" — that would wipe every legitimately-live entry for
 * the host instead of fixing the leak. Only entries whose `hostId` matches
 * are ever considered — a health record from one host must never reconcile
 * away another host's live sweeps. */
export function selectReconciledAwaySweepKeys(
  entries: Iterable<[string, ActiveSweepState]>,
  hostId: string,
  rawActiveSweepIds: unknown,
): string[] {
  if (!Array.isArray(rawActiveSweepIds) || rawActiveSweepIds.length === 0) {
    return [];
  }
  const activeSweepIds = new Set(
    rawActiveSweepIds.filter((id): id is string => typeof id === "string"),
  );
  if (activeSweepIds.size === 0) {
    return [];
  }
  const staleKeys: string[] = [];
  for (const [key, entry] of entries) {
    if (entry.hostId === hostId && !activeSweepIds.has(entry.sweepId)) {
      staleKeys.push(key);
    }
  }
  return staleKeys;
}

/** A `compute:` entry that has gone this long with no completion record is
 * flagged as a leak (Issue #8305, parent #8257 AC 4).
 *
 * **Deliberately 6x [`STALE_SWEEP_MS`], not a reuse of it.** The two bounds
 * measure different things. A `sweep:` entry is refreshed by every
 * `sweep.phase` record, so 4h of silence means the *reporting* died, not that
 * the work is long. A `compute:` entry is refreshed by nothing at all between
 * launch and completion, so its age is simply the job's wall clock — and an
 * elastic EDA batch job (full-chip simulation/synthesis on a large spot
 * instance) plausibly runs for the better part of a day, an order of
 * magnitude past any sweep. At 4h this would flag long-but-healthy jobs as
 * leaks constantly, which is worse than useless: it trains the reader to
 * ignore the flag. 24h clears the realistic job envelope while still bounding
 * a lost completion record to one day of phantom "running now" state.
 * `wall_clock_sec` on the completion records already landing in D1 is the
 * evidence to re-tune this against once real job durations accumulate. */
export const STALE_COMPUTE_MS = 24 * 60 * 60 * 1000; // 24 hours

/** A leaked `compute:` entry is deleted from the Durable Object entirely
 * once it is this old — the hygiene half of leak handling, mirroring
 * [`PRUNE_AFTER_MS`]'s role for `health:`/`tokens:` entries and sharing its
 * 7-day value. Much larger than [`STALE_COMPUTE_MS`] on purpose: a leak has
 * to stay *visible* long enough for an operator to see it and go reap the
 * instance, so flagging and pruning are separate steps rather than the
 * single prune-on-sight step `sweep:` entries get. */
export const PRUNE_COMPUTE_AFTER_MS = PRUNE_AFTER_MS;

/** `true` once a `compute:` entry has outlived [`STALE_COMPUTE_MS`] with no
 * completion record — i.e. it is a leak, not a live job.
 *
 * An unparseable `updatedAt` decodes to `NaN` and is treated as NOT leaked
 * (fail open), the same "unknown is not evidence" posture
 * [`isSweepEntryStale`] and [`isPruneable`] take. */
export function isComputeEntryLeaked(
  entry: ActiveComputeState,
  nowMs: number,
  staleMs: number = STALE_COMPUTE_MS,
): boolean {
  const updatedAtMs = Date.parse(entry.updatedAt);
  return Number.isFinite(updatedAtMs) && nowMs - updatedAtMs > staleMs;
}

/** Pure core of `buildSnapshot`'s `compute:` pass (Issue #8305), split out so
 * the leak/prune decisions are unit-testable without a Durable Object — same
 * split as [`classifyAndPruneHosts`] and [`selectStaleSweepKeys`].
 *
 * Returns every entry that still belongs in the snapshot, each with an
 * explicit `leaked` boolean, plus the storage keys old enough to delete.
 * `leaked` is always set (never left `undefined`) so a consumer can branch on
 * it without having to know whether this backend computed it. */
export function classifyComputeEntries(
  entries: Iterable<[string, ActiveComputeState]>,
  nowMs: number,
  staleMs: number = STALE_COMPUTE_MS,
  pruneMs: number = PRUNE_COMPUTE_AFTER_MS,
): { activeCompute: ActiveComputeState[]; pruneKeys: string[] } {
  const activeCompute: ActiveComputeState[] = [];
  const pruneKeys: string[] = [];
  for (const [key, entry] of entries) {
    const ageMs = nowMs - Date.parse(entry.updatedAt);
    if (Number.isFinite(ageMs) && ageMs > pruneMs) {
      pruneKeys.push(key);
      continue;
    }
    activeCompute.push({ ...entry, leaked: isComputeEntryLeaked(entry, nowMs, staleMs) });
  }
  return { activeCompute, pruneKeys };
}

export class FleetState implements DurableObject {
  private readonly state: DurableObjectState;

  constructor(state: DurableObjectState) {
    this.state = state;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    if (request.method === "POST" && url.pathname === "/update") {
      const body = (await request.json()) as FleetStateUpdate;
      await this.applyUpdate(body);
      return new Response(null, { status: 204 });
    }

    if (request.method === "GET" && url.pathname === "/snapshot") {
      const snapshot = await this.buildSnapshot();
      return new Response(JSON.stringify(snapshot), {
        headers: { "content-type": "application/json" },
      });
    }

    if (request.method === "POST" && url.pathname === "/remove-host") {
      const body = (await request.json()) as { hostId?: unknown };
      const hostId = typeof body.hostId === "string" ? body.hostId : undefined;
      if (!hostId) {
        return new Response("hostId is required", { status: 400 });
      }
      await this.removeHost(hostId);
      return new Response(null, { status: 204 });
    }

    return new Response("not found", { status: 404 });
  }

  /**
   * Remove a host's live-state entries (`health:`/`tokens:`/`queue:<hostId>`)
   * outright — the DO-side half of retiring a host (issue #4957 AC: "fleet
   * drain removes the host's live-state entries"). Wired from
   * `src/index.ts`'s `POST /admin/hosts/:hostId/revoke`, the dashboard's
   * existing "this host is gone" signal — there is no separate "drain"
   * concept at this layer. Does **not** touch that host's `sweep:<sweepId>`
   * entries: an in-flight sweep on a host being revoked mid-run is a real
   * anomaly worth surfacing (via its own staleness), not something this
   * best-effort cleanup should paper over. Its `compute:<jobId>` entries
   * (Issue #8305) are left alone for the same reason — and they are not even
   * keyed by host, so reaping them here would mean a full prefix scan to hide
   * a still-running, still-billing cloud instance. Because the caller's fetch to
   * this route is itself best-effort (issue #5078), a failure here can leave
   * these entries behind indefinitely — [`filterRevokedHosts`] is the
   * read-time backstop for exactly that case, treating D1's `revoked_at` as
   * authoritative over whatever this method did or did not manage to delete.
   */
  private async removeHost(hostId: string): Promise<void> {
    await this.state.storage.delete([`health:${hostId}`, `tokens:${hostId}`, `queue:${hostId}`]);
  }

  private async applyUpdate({ hostId, record }: FleetStateUpdate): Promise<void> {
    const kind = record.kind;
    const now = new Date().toISOString();

    switch (kind) {
      case "host.health": {
        await this.state.storage.put(`health:${hostId}`, { record, updatedAt: now });
        await this.reconcileActiveSweeps(hostId, record.active_sweep_ids);
        break;
      }
      case "tokens.snapshot": {
        await this.state.storage.put(`tokens:${hostId}`, { record, updatedAt: now });
        break;
      }
      case "queue.snapshot": {
        // Issue #8852: newest tick wins; a redelivered or older snapshot must
        // not refresh `updatedAt` (see `shouldReplaceQueue`).
        const snapshot = normalizeQueueSnapshot(record);
        if (!snapshot) return;
        const key = `queue:${hostId}`;
        const existing = await this.state.storage.get<HostQueueEntry>(key);
        if (!shouldReplaceQueue(existing, snapshot)) return;
        const entry: HostQueueEntry = { record: snapshot, updatedAt: now };
        await this.state.storage.put(key, entry);
        break;
      }
      case "sweep.started": {
        const sweepId = record.sweep_id;
        if (typeof sweepId !== "string") return;
        const entry: ActiveSweepState = {
          hostId,
          sweepId,
          repo: typeof record.repo === "string" ? record.repo : undefined,
          visibility: record.visibility === "public" ? "public" : "private",
          issue: typeof record.issue === "number" ? record.issue : undefined,
          startedAt: typeof record.started_at === "string" ? record.started_at : undefined,
          model: typeof record.model === "string" ? record.model : undefined,
          effort: typeof record.effort === "string" ? record.effort : undefined,
          runtime: identityString(record.runtime),
          provider: identityString(record.provider),
          updatedAt: now,
        };
        await this.state.storage.put(`sweep:${sweepId}`, entry);
        break;
      }
      case "sweep.identity": {
        const sweepId = record.sweep_id;
        if (typeof sweepId !== "string") return;
        const key = `sweep:${sweepId}`;
        const existing = await this.state.storage.get<ActiveSweepState>(key);
        // Late metadata cannot create/resurrect a sweep or cross host ownership.
        if (!existing || existing.hostId !== hostId) return;
        await this.state.storage.put(key, {
          ...existing,
          runtime: identityString(record.runtime) ?? existing.runtime,
          provider: identityString(record.provider) ?? existing.provider,
          model: identityString(record.model) ?? existing.model,
        });
        break;
      }
      case "sweep.phase": {
        const sweepId = record.sweep_id;
        if (typeof sweepId !== "string") return;
        const existing = await this.state.storage.get<ActiveSweepState>(`sweep:${sweepId}`);
        const entry: ActiveSweepState = {
          hostId,
          sweepId,
          repo: existing?.repo ?? (typeof record.repo === "string" ? record.repo : undefined),
          visibility:
            existing?.visibility ?? (record.visibility === "public" ? "public" : "private"),
          issue: existing?.issue ?? (typeof record.issue === "number" ? record.issue : undefined),
          startedAt: existing?.startedAt,
          phase: typeof record.phase === "string" ? record.phase : existing?.phase,
          enteredPhaseAt: typeof record.entered_at === "string" ? record.entered_at : now,
          model: existing?.model,
          effort: existing?.effort,
          runtime: existing?.runtime,
          provider: existing?.provider,
          updatedAt: now,
        };
        await this.state.storage.put(`sweep:${sweepId}`, entry);
        break;
      }
      case "sweep.completed": {
        const sweepId = record.sweep_id;
        if (typeof sweepId !== "string") return;
        // A completed sweep is no longer "live" — its full record already
        // landed in D1 via the same ingest batch. Removing it here keeps
        // the DO's working set bounded by concurrently-running sweeps only.
        await this.state.storage.delete(`sweep:${sweepId}`);
        break;
      }
      case "ephemeral_compute": {
        // Issue #8305. Both a launch record and a completion record arrive
        // under this one kind; `ended_at` is what tells them apart (a launch
        // record simply omits it — see `migrations/0003_ephemeral_compute.sql`).
        // A present-but-empty/null `ended_at` reads as "not ended", the
        // conservative direction: it keeps the entry live (visible, and
        // eventually flagged as a leak) rather than silently retiring a job
        // that may still be burning money.
        const jobId = record.job_id;
        if (typeof jobId !== "string" || jobId.length === 0) return;
        const endedAt = record.ended_at;
        if (typeof endedAt === "string" && endedAt.length > 0) {
          // A completed job is no longer "live" — both of its records are
          // already durable in D1. A completion whose launch record never
          // reached this DO (host restarted mid-job, or the DO was recreated)
          // deletes a key that does not exist, which Durable Object storage
          // treats as a no-op: the right behaviour, since the end state
          // ("this job is not running") is identical either way, and the DO
          // has no log sink to report the anomaly to anyway. D1 still holds
          // the completion record for anyone reconstructing history.
          await this.state.storage.delete(`compute:${jobId}`);
          break;
        }
        const existing = await this.state.storage.get<ActiveComputeState>(`compute:${jobId}`);
        const computeSweepId = record.sweep_id;
        const entry: ActiveComputeState = {
          hostId,
          jobId,
          // Issue #8835: the submitting sweep, when the emitter stamped one.
          // An empty string is normalized away rather than stored — it would
          // never match a live sweep, and storing it would make "no sweep" two
          // distinct values for every downstream reader to handle.
          sweepId:
            typeof computeSweepId === "string" && computeSweepId.length > 0 ? computeSweepId : undefined,
          instanceId: typeof record.instance_id === "string" ? record.instance_id : undefined,
          region: typeof record.region === "string" ? record.region : undefined,
          instanceType: typeof record.instance_type === "string" ? record.instance_type : undefined,
          spot: typeof record.spot === "boolean" ? record.spot : undefined,
          ami: typeof record.ami === "string" ? record.ami : undefined,
          startedAt: typeof record.started_at === "string" ? record.started_at : undefined,
          // Keying on `job_id` alone (never `jobId+instanceId`) means a
          // re-sent launch record collapses onto the same entry instead of
          // creating a second one. First-seen `updatedAt` wins so that
          // at-least-once redelivery cannot postpone leak detection
          // indefinitely by resetting the entry's clock on every retry.
          updatedAt: existing?.updatedAt ?? now,
        };
        await this.state.storage.put(`compute:${jobId}`, entry);
        break;
      }
      default:
        // sweep.outcome and any forward-compatible unknown kind carry no
        // additional live-state signal beyond what sweep.started/phase/
        // completed already captured — D1 is the durable record of it.
        break;
    }
  }

  /** Fix layer 2 (Issue #4955): reconcile this host's `sweep:` entries
   * against its own daemon's authoritative in-flight sweep-id set, carried
   * on every `host.health` record as `active_sweep_ids`. See
   * [`selectReconciledAwaySweepKeys`] for the (pure, directly-tested)
   * decision logic — this method is only the I/O around it. */
  private async reconcileActiveSweeps(hostId: string, rawActiveSweepIds: unknown): Promise<void> {
    // Skip the `list()` call entirely for the common case (field absent —
    // every daemon predating #4955, or this host's daemon hasn't ticked its
    // next health sample yet): `selectReconciledAwaySweepKeys` would return
    // `[]` anyway, but there is no reason to pay for a full storage scan to
    // learn that.
    if (!Array.isArray(rawActiveSweepIds) || rawActiveSweepIds.length === 0) {
      return;
    }
    const sweepEntries = await this.state.storage.list<ActiveSweepState>({ prefix: "sweep:" });
    const staleKeys = selectReconciledAwaySweepKeys(sweepEntries, hostId, rawActiveSweepIds);
    if (staleKeys.length > 0) {
      await this.state.storage.delete(staleKeys);
    }
  }

  /**
   * Build the current fleet snapshot, classifying every `health:`/`tokens:`
   * entry's freshness (issue #4957) and pruning any entry older than
   * [`PRUNE_AFTER_MS`] as a side effect — the "DO hygiene" half of the AC
   * ("long-gone hosts age out of the DO entirely"). Pruning piggybacks on
   * this read rather than needing its own cron/route: every consumer
   * (`/snapshot`, `/admin/fleet-state`, `/api/fleet-state`,
   * `/public/fleet-state`) already calls this on every request, so a
   * decommissioned host's entries are deleted the next time anyone looks —
   * `now` is threaded through for deterministic tests.
   *
   * The same pass also applies issue #4955's fix layer 1 to the `sweep:`
   * prefix (see below): host-entry pruning (#4957) and stale-sweep
   * exclusion (#4955) are independent policies over disjoint key prefixes
   * that share this one read, and `now` is threaded through both.
   */
  private async buildSnapshot(now: Date = new Date()): Promise<FleetSnapshot> {
    const healthEntries = await this.state.storage.list<{ record: Record<string, unknown>; updatedAt: string }>({
      prefix: "health:",
    });
    const tokenEntries = await this.state.storage.list<{ record: Record<string, unknown>; updatedAt: string }>({
      prefix: "tokens:",
    });
    const { hosts, pruneKeys } = classifyAndPruneHosts(healthEntries, tokenEntries, now);
    // Issue #8852: the `queue:` prefix shares the host prune horizon.
    const queueEntries = await this.state.storage.list<HostQueueEntry>({ prefix: "queue:" });
    const { queues, pruneKeys: queuePruneKeys } = classifyAndPruneQueues(queueEntries, now);
    for (const [hostId, queue] of Object.entries(queues)) {
      hosts[hostId] ??= {};
      hosts[hostId].queue = queue;
    }
    pruneKeys.push(...queuePruneKeys);
    if (pruneKeys.length > 0) {
      await this.state.storage.delete(pruneKeys);
    }

    // Fix layer 1 (Issue #4955): a `sweep:` entry whose `updatedAt` exceeds
    // the staleness bound is excluded from the snapshot AND lazily deleted
    // (self-healing — no separate cleanup pass needed). See
    // `selectStaleSweepKeys`'s doc comment for the (pure, directly-tested)
    // decision logic.
    const sweepEntries = await this.state.storage.list<ActiveSweepState>({ prefix: "sweep:" });
    const staleKeys = new Set(selectStaleSweepKeys(sweepEntries, now.getTime()));
    const activeSweeps: ActiveSweepState[] = [];
    for (const [key, entry] of sweepEntries) {
      if (!staleKeys.has(key)) {
        activeSweeps.push(entry);
      }
    }
    if (staleKeys.size > 0) {
      await this.state.storage.delete(Array.from(staleKeys));
    }

    // Issue #8305: the `compute:` prefix's own pass — flag leaks, prune the
    // long-dead. Same shape as the two passes above (a third independent
    // policy over a disjoint key prefix, sharing this one read path), with
    // its own bounds; see `classifyComputeEntries`.
    const computeEntries = await this.state.storage.list<ActiveComputeState>({ prefix: "compute:" });
    const { activeCompute, pruneKeys: computePruneKeys } = classifyComputeEntries(
      computeEntries,
      now.getTime(),
    );
    if (computePruneKeys.length > 0) {
      await this.state.storage.delete(computePruneKeys);
    }

    return { hosts, activeSweeps, activeCompute };
  }
}

/** Missing/malformed identity is unknown, never a request to erase known data. */
function identityString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}
