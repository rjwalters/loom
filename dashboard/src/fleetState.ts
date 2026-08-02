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

/** Body accepted by the internal `POST /update` route — one record's worth
 * of live-state effect, already authenticated/validated by the Worker
 * before it reaches the Durable Object. */
export interface FleetStateUpdate {
  hostId: string;
  record: Record<string, unknown>;
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
   * best-effort cleanup should paper over.
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

    const sweepEntries = await this.state.storage.list<ActiveSweepState>({ prefix: "sweep:" });
    const activeSweeps = Array.from(sweepEntries.values());

    return { hosts, activeSweeps };
  }
}
