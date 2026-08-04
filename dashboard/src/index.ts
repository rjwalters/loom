/**
 * Worker entrypoint — the Epic #4702 Phase 2 ingest endpoint + storage
 * backend.
 *
 * Since Phase 3 (#4749) this Worker **also serves the dashboard UI** as static
 * assets (`wrangler.toml`'s `[assets] directory = "./web/dist"`, built from
 * `web/`). The asset router runs *before* this module: a request matching a
 * built file (`/`, `/assets/*`) is served from the bundle and never reaches
 * `fetch()` below, and everything else falls through here because the config
 * sets `not_found_handling = "none"`. The practical consequence is that the
 * `GET /` handler at the bottom of this file is now only a fallback for a
 * deploy with no assets uploaded.
 *
 * Routes:
 *
 *   POST /ingest
 *     The wire contract this route MUST satisfy is fixed by the already-
 *     merged Phase-1 sender (`loom-daemon/src/observability/exporter.rs`):
 *     a bare JSON array of `TelemetryEnvelope`s, `Authorization: Bearer
 *     <ingest_key>`. See `handleIngest` below for the full contract.
 *
 *   POST /admin/hosts               — provision a new host + ingest key.
 *   POST /admin/hosts/:hostId/revoke — revoke a host's ingest key AND clear
 *                                      its `FleetState` Durable Object
 *                                      entries (issue #4957) — the
 *                                      dashboard's "this host is gone"
 *                                      signal.
 *   POST /admin/retention/run       — run the retention sweep on demand
 *                                      (the cron `scheduled()` handler runs
 *                                      the same sweep hourly).
 *   GET  /admin/fleet-state         — read the Durable Object's live
 *                                      snapshot (introspection/verification
 *                                      only — the `/api/*` routes below are
 *                                      the actual Phase 3 dashboard query
 *                                      API).
 *
 *   GET  /api/version                 — public, unauthenticated (issue
 *                                      #4958): the deploying commit SHA,
 *                                      `{"commit":"unknown"}` when the
 *                                      deployment never stamped one. See
 *                                      `handleVersion` below.
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
 *   GET  /public                     — 301s to `/` (issue #4795 — the
 *                                      single-URL fallback layout replaced
 *                                      the separate public path; kept as a
 *                                      redirect only so old bookmarks/links
 *                                      still work).
 *   GET  /                            — single-URL dashboard root (issue
 *                                      #4795): validates the visitor's
 *                                      Cloudflare Access JWT, carried as the
 *                                      `CF_Authorization` cookie (see
 *                                      `./accessAuth.ts`). A valid,
 *                                      correctly-audienced token renders the
 *                                      full authenticated dashboard (same
 *                                      page as the old `/public`, but with
 *                                      unredacted data + `/api/*` live
 *                                      feed); anything else — no cookie, a
 *                                      malformed/expired/wrong-aud token, a
 *                                      JWKS fetch failure — fails CLOSED to
 *                                      the redacted public view with a
 *                                      "Sign in" link, exactly like the old
 *                                      `/public` page. Never a 500, never
 *                                      the full view on a doubtful token.
 *   GET  /login                      — Access-gated at the edge (config,
 *                                      not code — see
 *                                      `docs/cloudflare-access.md`): the
 *                                      only job of this route, once Access
 *                                      lets a request through, is to bounce
 *                                      back to `/` so the freshly-minted
 *                                      `CF_Authorization` cookie is
 *                                      re-validated there.
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
 * verifies a JWT or any other credential in-Worker. **The dashboard root `/`
 * is the one exception** (issue #4795): it is the Worker's first in-Worker
 * credential check, added specifically so a single URL can serve both
 * audiences — see `./accessAuth.ts`'s module doc for the full fail-closed
 * contract.
 */

import { extractBearerToken, authenticateHost, hashIngestKey } from "./auth";
import { validateAccessJwt } from "./accessAuth";
import { FleetState, filterRevokedHosts, type FleetSnapshot } from "./fleetState";
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
  /** Single-URL dashboard root (issue #4795) — see `./accessAuth.ts`. Both
   * are plain, non-secret `[vars]`; leaving either unset makes `/` always
   * render the public view (fails closed, never the other way). */
  CF_ACCESS_TEAM_DOMAIN?: string;
  CF_ACCESS_AUD?: string;
  /** IANA timezone the UI renders times and buckets chart days in (e.g.
   * `America/Los_Angeles`). Unset — the committed default — makes the UI fall
   * back to each viewer's own browser zone. Set it on a deployment whose
   * chart buckets should mean the same thing to everyone looking (issue
   * #4857); the UI validates it and falls back rather than failing. */
  DISPLAY_TIMEZONE?: string;
  /** Workers Assets binding for the built dashboard UI (`web/dist`). Absent
   * in the Miniflare test env and on a deploy whose UI build did not run —
   * `handleRoot` falls back to the server-rendered page in that case, so this
   * is optional by design rather than an invariant to assert. */
  ASSETS?: Fetcher;
  /** The commit this Worker was built/deployed from — a plain, non-secret
   * `[vars]`-equivalent injected at deploy time via `wrangler deploy --var
   * BUILD_COMMIT:$GITHUB_SHA` (issue #4958), never written into the committed
   * `wrangler.toml`. Absent in local `wrangler dev` and the Miniflare test
   * env, where `/api/version` and the footer fall back to `"unknown"` rather
   * than throwing. */
  BUILD_COMMIT?: string;
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

/** D1 host_ids, among `hostIds`, whose `hosts` row is revoked — bounded to
 * the snapshot's own host set rather than scanning the whole `hosts` table,
 * since that is all `filterRevokedHosts` needs. */
async function fetchRevokedHostIds(env: Env, hostIds: readonly string[]): Promise<Set<string>> {
  if (hostIds.length === 0) return new Set();
  const placeholders = hostIds.map(() => "?").join(",");
  const { results } = await env.DB.prepare(
    `SELECT host_id FROM hosts WHERE revoked_at IS NOT NULL AND host_id IN (${placeholders})`,
  )
    .bind(...hostIds)
    .all<{ host_id: string }>();
  return new Set(results.map((row) => row.host_id));
}

/**
 * The Durable Object's live snapshot, with any host D1 has recorded as
 * revoked filtered out (issue #5078, mechanism 2) — the single point every
 * dashboard-facing read (`/api/fleet-state`, `/public/fleet-state`, the
 * server-rendered `/` fallback) goes through, so a `handleRevokeHost` cleanup
 * fetch that silently failed can never leave a stale `health:`/`tokens:`
 * entry rendering as a live host. `GET /admin/fleet-state` deliberately does
 * **not** use this helper — it is introspection of the DO's own raw state
 * (see its route doc), and filtering there would hide the very staleness it
 * exists to let an operator diagnose. */
async function fetchLiveFleetSnapshot(env: Env): Promise<FleetSnapshot> {
  const response = await fleetStateStub(env).fetch("https://fleet-state/snapshot");
  const snapshot = (await response.json()) as FleetSnapshot;
  const revokedHostIds = await fetchRevokedHostIds(env, Object.keys(snapshot.hosts));
  return filterRevokedHosts(snapshot, revokedHostIds);
}

// ---------------------------------------------------------------------------
// /api/version — the build/deploy commit stamp (issue #4958)
// ---------------------------------------------------------------------------

/**
 * Deliberately unauthenticated (unlike the rest of `/api/*`): the deploying
 * commit SHA is not sensitive — it is already public in the repo's own commit
 * history — and drift-detection tooling (a health check, a CI smoke test, an
 * operator's terminal) should not need a Cloudflare Access session just to
 * ask "what is live right now?". Routed *before* the `/api/*` auth gate in
 * the entrypoint below, the same way `/ingest` and `/admin/*` are.
 */
function handleVersion(env: Env): Response {
  return new Response(JSON.stringify({ commit: env.BUILD_COMMIT ?? "unknown" }), {
    status: 200,
    headers: JSON_HEADERS,
  });
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
    return ingestAck(0, auth.hostId);
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
    // `OR IGNORE` (Issue #5084): `idx_records_terminal_sweep_once`
    // (migrations/0002) enforces a partial UNIQUE(kind, sweep_id) for
    // exactly `sweep.completed`/`sweep.outcome` — the two kinds a sweep
    // emits once, ever. A re-sent record for a sweep already ingested
    // (the daemon-side backfill drain's cursor is an efficiency
    // optimization, not a correctness guarantee — see
    // `loom-daemon/src/observability/backfill.rs`'s module doc) is
    // silently absorbed here instead of duplicating the row. Every other
    // `kind` has no matching unique index, so this is a no-op change for
    // them — `INSERT OR IGNORE` behaves exactly like `INSERT` when there is
    // no conflicting constraint.
    return env.DB.prepare(
      `INSERT OR IGNORE INTO records
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

  return ingestAck(envelopes.length, auth.hostId);
}

/**
 * The `/ingest` success response (issue #4830).
 *
 * `host_id` echoes back the identity the *authenticated key* is bound to — the
 * one this request's rows were actually filed under (see the INSERT above),
 * which is not necessarily the `host_id` inside the envelopes. The exporter
 * compares it against the daemon's own host identity and warns when they
 * disagree.
 *
 * That echo exists because of a live 2026-07-31 incident: a Mac Studio spent
 * hours pushing telemetry under `robb-pro` because the wrong host's key file
 * had been installed on it. The backend cannot detect this — a key-bound
 * host_id is authoritative here by design — and the exporter had no idea what
 * its key was bound to, so nothing anywhere warned. Only the daemon holds both
 * halves; this field is what hands it the second one.
 *
 * Purely additive: `accepted` is unchanged, no envelope `schema_version` rev is
 * involved, and a pre-#4830 exporter that ignores the response body is
 * unaffected.
 */
function ingestAck(accepted: number, hostId: string): Response {
  return new Response(JSON.stringify({ accepted, host_id: hostId }), {
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

  const key = typeof body.key === "string" && body.key.length > 0 ? body.key : generateIngestKey();
  const keyHash = await hashIngestKey(key);

  // A *revoked* host_id must be re-provisionable without hand-editing D1
  // (#5082): a revoked row is a dead credential, but `host_id` is the primary
  // key, so it would otherwise reserve the name forever. This single statement
  // both creates a brand-new host and re-mints a retired one:
  //
  //   - live row  → `DO UPDATE ... WHERE hosts.revoked_at IS NOT NULL` does not
  //     match, so nothing changes and `meta.changes` is 0 ⇒ 409 (accidental
  //     re-minting of an *active* host still fails).
  //   - revoked row → the conflict target matches and the row is rewritten with
  //     the NEW `key_hash` and a cleared `revoked_at`. The old (revoked) hash is
  //     replaced, never reactivated, and `created_at` is restamped because the
  //     row now describes a freshly minted credential.
  //
  // It is an upsert rather than DELETE + INSERT precisely so there is no window
  // in which the host row is absent.
  const result = await env.DB.prepare(
    `INSERT INTO hosts (host_id, key_hash, created_at, revoked_at) VALUES (?, ?, ?, NULL)
     ON CONFLICT(host_id) DO UPDATE SET
       key_hash = excluded.key_hash,
       created_at = excluded.created_at,
       revoked_at = NULL
     WHERE hosts.revoked_at IS NOT NULL`,
  )
    .bind(hostId, keyHash, new Date().toISOString())
    .run();
  if ((result.meta.changes ?? 0) === 0) {
    return jsonError(409, `host_id "${hostId}" already exists`);
  }

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
  // Issue #4957 AC: "fleet drain removes the host's live-state entries" —
  // this is the dashboard's own "this host is gone" signal (there is no
  // separate drain concept at this layer), so it also clears the Durable
  // Object's `health:`/`tokens:` entries for it, rather than leaving a
  // revoked host's last-known numbers rendering as current until the
  // 7-day prune horizon (`fleetState.ts`'s `PRUNE_AFTER_MS`) catches up.
  // Best-effort: a DO hiccup here must not fail the revoke itself — the
  // D1 key revocation (above) is the security-relevant half of this route,
  // already committed by the time this runs.
  try {
    await fleetStateStub(env).fetch("https://fleet-state/remove-host", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hostId }),
    });
  } catch (err) {
    console.error(`fleet-state remove-host failed for "${hostId}":`, err);
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
 * (public, redacted) — the query-API equivalent of `GET /admin/fleet-state`,
 * but through `fetchLiveFleetSnapshot` rather than the DO's raw snapshot, so
 * a D1-revoked host filtered there never reaches either response — no admin
 * token required. `isAuthenticated` is purely a function of which route
 * matched (see the module doc); the response is redacted via
 * `./redaction.ts` when it is `false`. */
async function handleFleetStateQuery(env: Env, isAuthenticated: boolean): Promise<Response> {
  const snapshot = await fetchLiveFleetSnapshot(env);
  const body = isAuthenticated ? snapshot : redactFleetSnapshot(snapshot, isAuthenticated);
  return new Response(JSON.stringify(body), { status: 200, headers: JSON_HEADERS });
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

/** The single-URL dashboard root's page body (issue #4795; originally issue
 * #4753's `/public` page, generalized here to serve both variants). Sources
 * its data by calling `fleetStateStub`/`queryHistory` directly, the same
 * in-process calls `handleFleetStateQuery`/`handleHistoryQuery` make.
 *
 * `isAuthenticated: false` (the `/` fallback / old `/public`) redacts before
 * handing off to `./publicPage.ts` — there is no HTTP round-trip through
 * `/api/*` anywhere in this path, so this branch cannot accidentally source
 * unredacted data. `isAuthenticated: true` (a validated Access identity)
 * passes the raw snapshot/history straight through instead — the caller
 * (the root route below) has already confirmed the JWT via
 * `./accessAuth.ts` before reaching here. */
async function handleDashboardPage(env: Env, isAuthenticated: boolean): Promise<Response> {
  const snapshot = await fetchLiveFleetSnapshot(env);
  const displaySnapshot = isAuthenticated ? snapshot : redactFleetSnapshot(snapshot, false);

  const historyResult = await queryHistory(env.DB, { limit: DEFAULT_HISTORY_LIMIT });
  const displayHistory = isAuthenticated ? historyResult : redactHistoryQueryResult(historyResult, false);

  const html = renderPublicPage(displaySnapshot, displayHistory, { isAuthenticated });
  return new Response(html, {
    status: 200,
    headers: {
      "content-type": "text/html; charset=utf-8",
      // One URL, two bodies, keyed on a cookie: any shared cache that stored
      // the authenticated variant and replayed it to an anonymous visitor
      // would be a data leak that no amount of in-Worker verification could
      // catch. Worker responses are not edge-cached by default, but a zone
      // cache rule or an intermediary proxy could change that — so state the
      // requirement rather than rely on the default. `Vary: Cookie` is
      // belt-and-braces for any cache that ignores `private`/`no-store`.
      "cache-control": "private, no-store",
      vary: "Cookie",
    },
  });
}

/** The global the SPA reads to learn which dataset it may request. Injected
 * server-side (never inferred in the browser) so there is exactly one
 * authority on auth state — the same `validateAccessJwt` call that decides
 * the server-rendered variant. See `web/src/api.ts`. */
const AUTH_STATE_GLOBAL = "__LOOM_FLEET__";

/**
 * Serialize a value for embedding inside an inline `<script>`.
 *
 * An earlier revision noted that `JSON.stringify` of a *boolean* can never
 * emit `<` or `&`, so no escaping was needed — and warned to revisit that if
 * the payload ever carried a string. It does now (the operator's email), so
 * this is that revisit.
 *
 * The attack this closes is the classic one: a value containing the literal
 * `</script>` terminates the block early and everything after it parses as
 * markup. Escaping `<`, `>` and `&` as `\uXXXX` prevents it — those escapes
 * are valid inside a JSON string literal and decode back to the original
 * characters, so the value the browser sees is unchanged.
 *
 * The email arrives from a Cloudflare-signed, signature-verified JWT, so this
 * is defense in depth rather than the primary control. It costs nothing and
 * removes the need to reason about how much an IdP-supplied claim can be
 * trusted.
 */
function serializeForScript(value: unknown): string {
  return JSON.stringify(value)
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026");
}

/** The auth state the SPA reads out of the page. `email` is present only for
 * an authenticated viewer, and only when the token carried one. */
interface InjectedAuthState {
  authenticated: boolean;
  email?: string;
  /** Display timezone for the UI, when the deployment configures one. */
  timeZone?: string;
  /** The deploying commit SHA (issue #4958) — same value `/api/version`
   * reports, injected here too so the footer can show it with zero extra
   * requests. Omitted (not `"unknown"`) when `BUILD_COMMIT` is unset, so the
   * footer can distinguish "no stamp available" from a real value. */
  commit?: string;
}

/** Inject the auth state into the SPA shell as a `<script>` immediately
 * before `</head>`, so it is defined before the module bundle executes. */
function injectAuthState(html: string, state: InjectedAuthState): string {
  const tag = `<script>window.${AUTH_STATE_GLOBAL}=${serializeForScript(state)};</script>`;
  return html.includes("</head>") ? html.replace("</head>", `${tag}</head>`) : `${tag}${html}`;
}

/** `GET /` (issues #4795 + #4749) — validate the visitor's Access JWT (the
 * `CF_Authorization` cookie; see `./accessAuth.ts`'s module doc for why this
 * is a cookie, not the `Cf-Access-Jwt-Assertion` header) and serve the
 * dashboard UI with that auth state baked in. Fails CLOSED to the public
 * variant on anything but a fully valid, correctly-audienced token — never a
 * 500, never the full view on a doubtful token (#4795's core acceptance
 * criterion).
 *
 * Two variants of "the dashboard", in preference order:
 *
 * 1. **The SPA** (`web/dist/index.html` via the `ASSETS` binding) — the real
 *    Phase-3 UI. `wrangler.toml`'s `run_worker_first = ["/"]` is what routes
 *    `/` here instead of letting the asset router serve index.html directly,
 *    which is the only reason this Worker gets to stamp the auth state on it.
 * 2. **The server-rendered page** (`./publicPage.ts`) — the fallback when no
 *    UI build is uploaded (`ASSETS` unbound, or the asset miss). Keeps a
 *    bindings-only deploy and the Miniflare suite working, and keeps #4795's
 *    behavior intact on a deploy that skipped the UI build. */
async function handleRoot(request: Request, env: Env): Promise<Response> {
  const identity = await validateAccessJwt(request.headers.get("cookie"), env);
  const isAuthenticated = identity !== null;

  if (env.ASSETS) {
    const shell = await env.ASSETS.fetch(new Request(new URL("/index.html", request.url), { method: "GET" }));
    if (shell.ok) {
      // The timezone is deployment config, not identity — an anonymous
      // visitor reads the same charts and must bucket them the same way.
      const timeZone = env.DISPLAY_TIMEZONE ? { timeZone: env.DISPLAY_TIMEZONE } : {};
      // Likewise the build commit: every viewer, signed in or not, should be
      // able to see whether the live page is current.
      const commit = env.BUILD_COMMIT ? { commit: env.BUILD_COMMIT } : {};
      const state: InjectedAuthState = isAuthenticated
        ? { authenticated: true, ...(identity.email ? { email: identity.email } : {}), ...timeZone, ...commit }
        : { authenticated: false, ...timeZone, ...commit };

      return new Response(injectAuthState(await shell.text(), state), {
        status: 200,
        headers: {
          "content-type": "text/html; charset=utf-8",
          // Same reasoning as `handleDashboardPage`, and it matters more
          // here: this body now carries the viewer's own email, so a shared
          // cache collapsing two viewers' responses would show one
          // operator's identity to another.
          "cache-control": "private, no-store",
          vary: "Cookie",
        },
      });
    }
  }

  return handleDashboardPage(env, isAuthenticated);
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

    if (request.method === "GET" && url.pathname === "/api/version") {
      // Ahead of the auth gate below on purpose — see `handleVersion`'s doc.
      return handleVersion(env);
    }

    // Every `/api/*` route is operator-only, and the Worker — not the edge —
    // is what enforces that under the single-URL layout (#4795).
    //
    // Before #4795 the reference deployment had one hostname-wide Access
    // application, so "the request reached /api/*" really did imply "Access
    // let it through" and the handlers below could hardcode
    // `isAuthenticated: true`. Serving the public view at `/` means that app
    // has to go: it is what makes `/` require a login. Deleting it without
    // this check would leave `/api/*` matched by no Access application at
    // all — i.e. the unredacted fleet, open to the internet.
    //
    // A narrower Access app covering only `/api/*` does not work either.
    // Every app on a hostname sets the same `CF_Authorization` cookie, so a
    // second app's session silently overwrites the one `/` validates; and
    // the SPA reaches these routes by `fetch`, which cannot follow an SSO
    // redirect. Verifying the same cookie `handleRoot` already verifies
    // keeps one app, one audience, one cookie.
    if (url.pathname.startsWith("/api/")) {
      const identity = await validateAccessJwt(request.headers.get("cookie"), env);
      if (identity === null) {
        // 401, not 302: these routes answer JSON to an XHR client. `api.ts`
        // treats 401/403 as "your session expired, reload" rather than a
        // backend fault, which is exactly the right remedy here.
        return jsonError(401, "authentication required");
      }
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
      // Issue #4795: the single-URL fallback layout replaced the separate
      // public path — `/` now serves the same content (redacted, for an
      // unauthenticated visitor). A permanent redirect keeps old
      // bookmarks/links working without a second route to maintain.
      return Response.redirect(new URL("/", request.url).toString(), 301);
    }
    if (request.method === "GET" && url.pathname === "/login") {
      // Access-gated at the edge (config, not code — see
      // docs/cloudflare-access.md). By the time this Worker code runs,
      // Access has already minted the `CF_Authorization` session cookie;
      // this route's only job is to bounce back to `/`, where that cookie
      // gets validated.
      return Response.redirect(new URL("/", request.url).toString(), 302);
    }
    if (request.method === "GET" && url.pathname === "/") {
      return handleRoot(request, env);
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
