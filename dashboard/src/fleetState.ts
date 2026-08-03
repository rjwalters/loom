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
 * Storage layout (three key prefixes, iterated via `list({ prefix })` to
 * build a snapshot):
 *   `health:<hostId>`  → latest `host.health` record + when it was applied.
 *   `tokens:<hostId>`  → latest `tokens.snapshot` record + when applied.
 *   `sweep:<sweepId>`  → the in-flight sweep's current known state; removed
 *                        entirely on `sweep.completed` (a finished sweep is
 *                        not "live" — its full history lives in D1).
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
 */

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
  updatedAt: string;
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
    }
  >;
  activeSweeps: ActiveSweepState[];
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
 * gone — see `publicPage.ts`'s `renderFleetOverview`).
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
  return { hosts, activeSweeps: snapshot.activeSweeps };
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
   * Remove a host's live-state entries (`health:<hostId>` / `tokens:<hostId>`)
   * outright — the DO-side half of retiring a host (issue #4957 AC: "fleet
   * drain removes the host's live-state entries"). Wired from
   * `src/index.ts`'s `POST /admin/hosts/:hostId/revoke`, the dashboard's
   * existing "this host is gone" signal — there is no separate "drain"
   * concept at this layer. Does **not** touch that host's `sweep:<sweepId>`
   * entries: an in-flight sweep on a host being revoked mid-run is a real
   * anomaly worth surfacing (via its own staleness), not something this
   * best-effort cleanup should paper over. Because the caller's fetch to
   * this route is itself best-effort (issue #5078), a failure here can leave
   * these entries behind indefinitely — [`filterRevokedHosts`] is the
   * read-time backstop for exactly that case, treating D1's `revoked_at` as
   * authoritative over whatever this method did or did not manage to delete.
   */
  private async removeHost(hostId: string): Promise<void> {
    await this.state.storage.delete([`health:${hostId}`, `tokens:${hostId}`]);
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
          updatedAt: now,
        };
        await this.state.storage.put(`sweep:${sweepId}`, entry);
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

    return { hosts, activeSweeps };
  }
}
