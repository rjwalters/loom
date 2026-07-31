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
 *   GET  /api/fleet-state            — authenticated query API: current
 *                                      state of every known host/sweep,
 *                                      full detail regardless of visibility
 *                                      (issue #4726 built the data access;
 *                                      #4727 added the auth/redaction split
 *                                      below).
 *   GET  /api/history                — authenticated: filterable, paginated
 *                                      D1 history query, full detail.
 *   GET  /api/events                 — authenticated: SSE live tail of
 *                                      newly-ingested telemetry, full
 *                                      detail.
 *   GET  /public/fleet-state         — public: same query, redacted —
 *                                      private-visibility data is
 *                                      summarized (see `./redaction.ts`).
 *   GET  /public/history             — public: same query, redacted.
 *   GET  /public/events              — public: same live tail, redacted.
 *   GET  /public                     — public: the Phase-3 dashboard page
 *                                      itself (issue #4753), server-rendered
 *                                      from the same redacted data the three
 *                                      routes above return. See
 *                                      `./publicPage.ts` for the rendering +
 *                                      the token/cost-widget exposure
 *                                      decision.
 *
 * All `/admin/*` routes require `Authorization: Bearer <ADMIN_TOKEN>`
 * (a `wrangler secret`, never committed) — see README.md.
 *
 * **`/api/*` vs `/public/*` is a route-based authentication split, not an
 * in-Worker one** (issue #4727; full rationale in `./redaction.ts`'s module
 * doc and `docs/cloudflare-access.md`): `/api/*` is the surface an
 * operator's Cloudflare Access policy is expected to gate (the "everything
 * else… Allow" rule the Access guide already documents), `/public/*` is
 * always unauthenticated and always redacted, mirroring the `/public` path
 * that guide already reserves as a Bypass application. Neither route
 * verifies a JWT or any other credential in-Worker — per the epic's
 * constraint, that verification lives entirely at the Cloudflare Access
 * edge layer.
 */

import { extractBearerToken, authenticateHost, hashIngestKey } from "./auth";
import { FleetState, type FleetSnapshot } from "./fleetState";
import { renderPublicPage } from "./publicPage";
import {
  createLiveTailStream,
  DEFAULT_HISTORY_LIMIT,
  parseHistoryQuery,
  queryHistory,
  type LiveTailFilter,
} from "./query";
import { redactFleetSnapshot, redactHistoryQueryResult, redactLiveTailStream } from "./redaction";
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
// /api/* (authenticated) + /public/* (redacted) — issues #4726 + #4727
// ---------------------------------------------------------------------------

/** `GET /api/fleet-state` (authenticated) / `GET /public/fleet-state`
 * (public, redacted) — the query-API equivalent of `GET /admin/fleet-state`:
 * same Durable Object snapshot, no admin token required. `isAuthenticated`
 * is purely a function of which route matched (see the module doc); the
 * response is redacted via `./redaction.ts` when it is `false`. */
async function handleFleetStateQuery(env: Env, isAuthenticated: boolean): Promise<Response> {
  const response = await fleetStateStub(env).fetch("https://fleet-state/snapshot");
  const snapshot = (await response.json()) as FleetSnapshot;
  const body = isAuthenticated ? snapshot : redactFleetSnapshot(snapshot, isAuthenticated);
  return new Response(JSON.stringify(body), { status: response.status, headers: JSON_HEADERS });
}

/** `GET /api/history` (authenticated) / `GET /public/history` (public,
 * redacted) — filterable, paginated D1 history query. See `query.ts`'s
 * `parseHistoryQuery`/`queryHistory` doc comments for the full
 * filter/pagination contract; `./redaction.ts`'s `redactHistoryQueryResult`
 * for the redaction applied when `isAuthenticated` is `false`. */
async function handleHistoryQuery(env: Env, url: URL, isAuthenticated: boolean): Promise<Response> {
  const filter = parseHistoryQuery(url.searchParams);
  if ("error" in filter) {
    return jsonError(400, filter.error);
  }
  const result = await queryHistory(env.DB, filter);
  const body = redactHistoryQueryResult(result, isAuthenticated);
  return new Response(JSON.stringify(body), { status: 200, headers: JSON_HEADERS });
}

/** `GET /api/events` (authenticated) / `GET /public/events` (public,
 * redacted) — SSE live tail of newly-ingested telemetry. See `query.ts`'s
 * `createLiveTailStream` doc comment for the framing/polling contract;
 * `./redaction.ts`'s `redactLiveTailStream` for the per-frame redaction
 * applied when `isAuthenticated` is `false`. */
function handleLiveTail(request: Request, env: Env, url: URL, isAuthenticated: boolean): Response {
  const filter: LiveTailFilter = {};
  const host = url.searchParams.get("host");
  if (host) filter.host = host;
  const repo = url.searchParams.get("repo");
  if (repo) filter.repo = repo;

  const rawStream = createLiveTailStream(env.DB, filter, { signal: request.signal });
  const stream = redactLiveTailStream(rawStream, isAuthenticated);
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

/** `GET /public` (issue #4753) — the unauthenticated dashboard page itself.
 * Sources its data by calling `fleetStateStub`/`queryHistory` directly, the
 * same in-process calls `handleFleetStateQuery`/`handleHistoryQuery` make,
 * then redacts with `isAuthenticated: false` before handing off to
 * `./publicPage.ts` for rendering — there is no HTTP round-trip through
 * `/api/*` anywhere in this path, so this route cannot accidentally source
 * unredacted data. */
async function handlePublicPage(env: Env): Promise<Response> {
  const stateResponse = await fleetStateStub(env).fetch("https://fleet-state/snapshot");
  const snapshot = (await stateResponse.json()) as FleetSnapshot;
  const redactedSnapshot = redactFleetSnapshot(snapshot, /* isAuthenticated */ false);

  const historyResult = await queryHistory(env.DB, { limit: DEFAULT_HISTORY_LIMIT });
  const redactedHistory = redactHistoryQueryResult(historyResult, /* isAuthenticated */ false);

  const html = renderPublicPage(redactedSnapshot, redactedHistory);
  return new Response(html, { status: 200, headers: { "content-type": "text/html; charset=utf-8" } });
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
      return handleFleetStateQuery(env, /* isAuthenticated */ true);
    }
    if (request.method === "GET" && url.pathname === "/public/fleet-state") {
      return handleFleetStateQuery(env, /* isAuthenticated */ false);
    }
    if (request.method === "GET" && url.pathname === "/api/history") {
      return handleHistoryQuery(env, url, /* isAuthenticated */ true);
    }
    if (request.method === "GET" && url.pathname === "/public/history") {
      return handleHistoryQuery(env, url, /* isAuthenticated */ false);
    }
    if (request.method === "GET" && url.pathname === "/api/events") {
      return handleLiveTail(request, env, url, /* isAuthenticated */ true);
    }
    if (request.method === "GET" && url.pathname === "/public/events") {
      return handleLiveTail(request, env, url, /* isAuthenticated */ false);
    }
    if (request.method === "GET" && url.pathname === "/public") {
      return handlePublicPage(env);
    }
    if (request.method === "GET" && url.pathname === "/") {
      return new Response("loom-observability-ingest: see /ingest, /admin/*, /api/*, /public/*", { status: 200 });
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
