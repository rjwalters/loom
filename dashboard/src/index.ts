/**
 * Worker entrypoint — the Epic #4702 Phase 2 ingest endpoint + storage
 * backend. Routes:
 *
 *   POST /ingest
 *     The wire contract this route MUST satisfy is fixed by the already-
 *     merged Phase-1 sender (`loom-daemon/src/observability/exporter.rs`):
 *     a bare JSON array of `TelemetryEnvelope`s, `Authorization: Bearer
 *     <ingest_key>`. See `handleIngest` below for the full contract.
 *
 *   POST /admin/hosts               — provision a new host + ingest key.
 *   POST /admin/hosts/:hostId/revoke — revoke a host's key.
 *   POST /admin/retention/run       — run the retention sweep on demand
 *                                      (the cron `scheduled()` handler runs
 *                                      the same sweep hourly).
 *   GET  /admin/fleet-state         — read the Durable Object's live
 *                                      snapshot (introspection/verification
 *                                      only — the `/api/*` routes below are
 *                                      the actual Phase 3 dashboard query
 *                                      API).
 *
 *   GET  /api/fleet-state           — Phase 3 query API: current state of
 *                                      every known host/sweep (issue #4726).
 *   GET  /api/history               — Phase 3 query API: filterable,
 *                                      paginated D1 history query (issue
 *                                      #4726).
 *   GET  /api/events                — Phase 3 query API: SSE live tail of
 *                                      newly-ingested telemetry (issue
 *                                      #4726).
 *
 * All `/admin/*` routes require `Authorization: Bearer <ADMIN_TOKEN>`
 * (a `wrangler secret`, never committed) — see README.md. The `/api/*`
 * routes are deliberately **not** admin-gated: they are the unclassified
 * query surface issue #4726 defines, returning full record detail.
 * Visibility-based redaction (public vs. authenticated views) is issue
 * #4727's job, as a wrapper in front of these routes — not implemented here.
 *
 * Scope boundary (per the issue): ingest + storage, plus the query API and
 * live tail added by #4726. Cloudflare Access auth / redaction are later
 * epic phases (#4727).
 */

import { extractBearerToken, authenticateHost, hashIngestKey } from "./auth";
import { FleetState } from "./fleetState";
import {
  createLiveTailStream,
  parseHistoryQuery,
  queryHistory,
  type LiveTailFilter,
} from "./query";
import { parseRetentionConfig, runRetentionSweep } from "./retention";
import { validateEnvelope, extractRecordFields, type TelemetryEnvelope } from "./telemetry";

export { FleetState };

export interface Env {
  DB: D1Database;
  FLEET_STATE: DurableObjectNamespace;
  RETENTION_DAYS?: string;
  MAX_RECORDS?: string;
  ADMIN_TOKEN?: string;
}

/** A single global Durable Object instance holds fleet-wide live state —
 * see the module doc in `fleetState.ts` for why a singleton is sufficient. */
const FLEET_STATE_ID_NAME = "fleet-singleton";

const JSON_HEADERS = { "content-type": "application/json" };

function jsonError(status: number, message: string): Response {
  return new Response(JSON.stringify({ error: message }), { status, headers: JSON_HEADERS });
}

function fleetStateStub(env: Env): DurableObjectStub {
  return env.FLEET_STATE.get(env.FLEET_STATE.idFromName(FLEET_STATE_ID_NAME));
}

// ---------------------------------------------------------------------------
// /ingest
// ---------------------------------------------------------------------------

async function handleIngest(request: Request, env: Env): Promise<Response> {
  const auth = await authenticateHost(env.DB, request.headers.get("authorization"));
  if (!auth) {
    return jsonError(401, "invalid or revoked ingest key");
  }

  let body: unknown;
  try {
    body = await request.json();
  } catch {
    return jsonError(400, "request body must be valid JSON");
  }
  if (!Array.isArray(body)) {
    return jsonError(400, "request body must be a bare JSON array of telemetry envelopes");
  }
  if (body.length === 0) {
    return new Response(JSON.stringify({ accepted: 0 }), { status: 200, headers: JSON_HEADERS });
  }

  // Whole-batch rejection on the first malformed envelope. Documented
  // decision (see the issue's Implementation Guidance): the exporter only
  // acks a batch on 2xx and otherwise retries the *entire* batch with
  // backoff, so partial-accept/partial-reject would need per-record
  // idempotency tracking on the exporter side that does not exist. Reject-
  // whole-batch is simplest and correct given that retry contract — a
  // single malformed envelope from a buggy client fails the same way every
  // retry (never a silent partial success), and the response's error
  // message identifies exactly which envelope index is at fault so the
  // problem is visible in the daemon's own export-failure logs.
  const envelopes: TelemetryEnvelope[] = [];
  for (let i = 0; i < body.length; i++) {
    const result = validateEnvelope(body[i], i);
    if (!result.ok) {
      return jsonError(400, `envelope ${result.error.index}: ${result.error.reason}`);
    }
    envelopes.push(result.envelope);
  }

  const nowIso = new Date().toISOString();
  const statements = envelopes.map((envelope) => {
    const fields = extractRecordFields(envelope.record);
    return env.DB.prepare(
      `INSERT INTO records
         (schema_version, emitted_at, host_id, kind, repo, visibility, issue, sweep_id, payload, ingested_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    ).bind(
      envelope.schema_version,
      envelope.emitted_at,
      // Authoritative host id from the authenticated key — NOT
      // `envelope.host_id` (opaque/client-supplied; see auth.ts doc).
      auth.hostId,
      fields.kind,
      fields.repo ?? null,
      fields.visibility,
      fields.issue ?? null,
      fields.sweepId ?? null,
      JSON.stringify(envelope.record),
      nowIso,
    );
  });

  try {
    // `D1Database.batch()` runs all statements as a single all-or-nothing
    // transaction — either every envelope in this batch is persisted, or
    // none are (matching the whole-batch semantics above).
    await env.DB.batch(statements);
  } catch (error) {
    return jsonError(500, `failed to persist batch: ${(error as Error).message}`);
  }

  // Best-effort live-state update. Deliberately NOT allowed to fail the
  // response: the D1 write above already durably committed this batch, so
  // failing the response here would make the exporter retry and duplicate
  // those D1 rows. The Durable Object is a live-state cache, not the
  // source of truth — see fleetState.ts's module doc.
  try {
    const stub = fleetStateStub(env);
    for (const envelope of envelopes) {
      await stub.fetch("https://fleet-state/update", {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({ hostId: auth.hostId, record: envelope.record }),
      });
    }
  } catch (error) {
    console.error(`fleet state update failed (D1 write already committed): ${(error as Error).message}`);
  }

  return new Response(JSON.stringify({ accepted: envelopes.length }), {
    status: 200,
    headers: JSON_HEADERS,
  });
}

// ---------------------------------------------------------------------------
// /admin/*
// ---------------------------------------------------------------------------

interface CreateHostBody {
  host_id?: unknown;
  key?: unknown;
}

function generateIngestKey(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

async function handleCreateHost(request: Request, env: Env): Promise<Response> {
  let body: CreateHostBody;
  try {
    body = (await request.json()) as CreateHostBody;
  } catch {
    return jsonError(400, "request body must be valid JSON");
  }
  const hostId = body.host_id;
  if (typeof hostId !== "string" || hostId.length === 0) {
    return jsonError(400, "host_id is required");
  }

  const existing = await env.DB.prepare("SELECT host_id FROM hosts WHERE host_id = ?").bind(hostId).first();
  if (existing) {
    return jsonError(409, `host_id "${hostId}" already exists`);
  }

  const key = typeof body.key === "string" && body.key.length > 0 ? body.key : generateIngestKey();
  const keyHash = await hashIngestKey(key);
  await env.DB.prepare("INSERT INTO hosts (host_id, key_hash, created_at, revoked_at) VALUES (?, ?, ?, NULL)")
    .bind(hostId, keyHash, new Date().toISOString())
    .run();

  // The plaintext key is returned exactly once, here — only its hash is
  // ever persisted, so this response is the operator's only chance to
  // capture it (for the daemon's `[observability].ingest_key` config).
  return new Response(JSON.stringify({ host_id: hostId, ingest_key: key }), {
    status: 201,
    headers: JSON_HEADERS,
  });
}

async function handleRevokeHost(env: Env, hostId: string): Promise<Response> {
  const result = await env.DB.prepare("UPDATE hosts SET revoked_at = ? WHERE host_id = ? AND revoked_at IS NULL")
    .bind(new Date().toISOString(), hostId)
    .run();
  if ((result.meta.changes ?? 0) === 0) {
    return jsonError(404, `host_id "${hostId}" not found or already revoked`);
  }
  return new Response(JSON.stringify({ host_id: hostId, revoked: true }), {
    status: 200,
    headers: JSON_HEADERS,
  });
}

async function handleAdmin(request: Request, env: Env, url: URL): Promise<Response> {
  if (!env.ADMIN_TOKEN) {
    return jsonError(503, "admin routes are not configured (ADMIN_TOKEN secret unset)");
  }
  const token = extractBearerToken(request.headers.get("authorization"));
  if (token !== env.ADMIN_TOKEN) {
    return jsonError(401, "invalid admin token");
  }

  if (request.method === "POST" && url.pathname === "/admin/hosts") {
    return handleCreateHost(request, env);
  }

  const revokeMatch = /^\/admin\/hosts\/([^/]+)\/revoke$/.exec(url.pathname);
  if (request.method === "POST" && revokeMatch?.[1]) {
    return handleRevokeHost(env, decodeURIComponent(revokeMatch[1]));
  }

  if (request.method === "POST" && url.pathname === "/admin/retention/run") {
    const config = parseRetentionConfig(env);
    const result = await runRetentionSweep(env.DB, config);
    return new Response(JSON.stringify(result), { status: 200, headers: JSON_HEADERS });
  }

  if (request.method === "GET" && url.pathname === "/admin/fleet-state") {
    const response = await fleetStateStub(env).fetch("https://fleet-state/snapshot");
    return new Response(response.body, { status: response.status, headers: JSON_HEADERS });
  }

  return jsonError(404, "not found");
}

// ---------------------------------------------------------------------------
// /api/* — Phase 3 dashboard query API (issue #4726)
// ---------------------------------------------------------------------------

/** `GET /api/fleet-state` — the unclassified equivalent of
 * `GET /admin/fleet-state`: same Durable Object snapshot, no admin token
 * required. This is the route the Phase 3 dashboard UI actually consumes;
 * `/admin/fleet-state` remains for operator introspection/verification. */
async function handleFleetStateQuery(env: Env): Promise<Response> {
  const response = await fleetStateStub(env).fetch("https://fleet-state/snapshot");
  return new Response(response.body, { status: response.status, headers: JSON_HEADERS });
}

/** `GET /api/history` — filterable, paginated D1 history query. See
 * `query.ts`'s `parseHistoryQuery`/`queryHistory` doc comments for the full
 * filter/pagination contract. */
async function handleHistoryQuery(env: Env, url: URL): Promise<Response> {
  const filter = parseHistoryQuery(url.searchParams);
  if ("error" in filter) {
    return jsonError(400, filter.error);
  }
  const result = await queryHistory(env.DB, filter);
  return new Response(JSON.stringify(result), { status: 200, headers: JSON_HEADERS });
}

/** `GET /api/events` — SSE live tail of newly-ingested telemetry. See
 * `query.ts`'s `createLiveTailStream` doc comment for the framing/polling
 * contract. */
function handleLiveTail(request: Request, env: Env, url: URL): Response {
  const filter: LiveTailFilter = {};
  const host = url.searchParams.get("host");
  if (host) filter.host = host;
  const repo = url.searchParams.get("repo");
  if (repo) filter.repo = repo;

  const stream = createLiveTailStream(env.DB, filter, { signal: request.signal });
  return new Response(stream, {
    status: 200,
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache, no-store",
      "x-accel-buffering": "no",
      connection: "keep-alive",
    },
  });
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

export default {
  async fetch(request, env, _ctx) {
    const url = new URL(request.url);

    if (request.method === "POST" && url.pathname === "/ingest") {
      return handleIngest(request, env);
    }
    if (url.pathname.startsWith("/admin/")) {
      return handleAdmin(request, env, url);
    }
    if (request.method === "GET" && url.pathname === "/api/fleet-state") {
      return handleFleetStateQuery(env);
    }
    if (request.method === "GET" && url.pathname === "/api/history") {
      return handleHistoryQuery(env, url);
    }
    if (request.method === "GET" && url.pathname === "/api/events") {
      return handleLiveTail(request, env, url);
    }
    if (request.method === "GET" && url.pathname === "/") {
      return new Response("loom-observability-ingest: see /ingest, /admin/*, /api/*", { status: 200 });
    }
    return jsonError(404, "not found");
  },

  async scheduled(_event, env) {
    const config = parseRetentionConfig(env);
    const result = await runRetentionSweep(env.DB, config);
    console.log(
      `retention sweep: deleted ${result.deletedByAge} by age, ${result.deletedBySize} by size cap`,
    );
  },
} satisfies ExportedHandler<Env>;
