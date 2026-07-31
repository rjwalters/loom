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

export interface FleetSnapshot {
  hosts: Record<
    string,
    {
      health?: { record: Record<string, unknown>; updatedAt: string };
      tokens?: { record: Record<string, unknown>; updatedAt: string };
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

    return new Response("not found", { status: 404 });
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

  private async buildSnapshot(): Promise<FleetSnapshot> {
    const hosts: FleetSnapshot["hosts"] = {};
    const healthEntries = await this.state.storage.list<{ record: Record<string, unknown>; updatedAt: string }>({
      prefix: "health:",
    });
    for (const [key, value] of healthEntries) {
      const hostId = key.slice("health:".length);
      hosts[hostId] ??= {};
      hosts[hostId].health = value;
    }
    const tokenEntries = await this.state.storage.list<{ record: Record<string, unknown>; updatedAt: string }>({
      prefix: "tokens:",
    });
    for (const [key, value] of tokenEntries) {
      const hostId = key.slice("tokens:".length);
      hosts[hostId] ??= {};
      hosts[hostId].tokens = value;
    }

    const sweepEntries = await this.state.storage.list<ActiveSweepState>({ prefix: "sweep:" });
    const activeSweeps = Array.from(sweepEntries.values());

    return { hosts, activeSweeps };
  }
}
