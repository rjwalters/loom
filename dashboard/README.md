# loom-observability-backend

Cloudflare Workers reference backend for the Loom fleet observability epic
([#4702](https://github.com/rjwalters/loom/issues/4702), Phase 2), **plus the
Phase-3 dashboard UI in [`web/`](web/)** which the same Worker serves as
static assets. Accepts the telemetry batches `loom-daemon`'s `HttpsExporter`
(`loom-daemon/src/observability/exporter.rs`, Phase 1 — #4705) pushes,
authenticates the sending host, persists durable history to D1, maintains a
live "what is running right now" snapshot in a Durable Object, and exposes the
read-side query API + live tail (issue #4726) that the dashboard UI consumes.

Wire format reference: [`.loom/docs/telemetry-schema.md`](../.loom/docs/telemetry-schema.md).
Query API reference: [`docs/query-api.md`](docs/query-api.md) — includes the
`/api/*` (authenticated) vs. `/public/*` (redacted) route split and the
visibility-based redaction policy (issue #4727).

**`/api/*` vs. `/public/*` remains a route-based auth split, not an
in-Worker one**: the [Access guide](docs/cloudflare-access.md) covers gating
`/api/*` via the edge proxy; the Worker decides what a `/api/*`/`/public/*`
request sees purely by which route matched, never by verifying a credential
itself. **The one exception is the dashboard root `/`** (issue
[#4795](https://github.com/rjwalters/loom/issues/4795)): it validates the
visitor's Cloudflare Access JWT in-Worker (`src/accessAuth.ts`) so a single
URL can serve both an anonymous public view and, for an allowed identity, the
full dashboard — see the Access guide's §5 for exactly what that check
covers and its fail-closed contract.

## Architecture

- **Static assets** (`web/dist`, built from `web/`) — the Phase-3 dashboard UI,
  uploaded with this Worker so the UI and the API share one hostname and
  therefore one Cloudflare Access policy. Requests that do not match a built
  file fall through to the Worker routes below
  (`not_found_handling = "none"`). See [`web/README.md`](web/README.md).
- **D1** (`records` table) — durable history of every accepted record, one
  row per record, indexed for `host_id`/`repo`/time-range queries. Bounded
  by the retention sweep (age + size caps — see `src/retention.ts`).
- **Durable Object** (`FleetState`, singleton) — live per-host health/token
  snapshots plus currently in-flight sweeps. Independent of D1; a
  best-effort cache, not a source of truth (see `src/fleetState.ts`). Every
  health/tokens entry is tagged `live`/`stale`/`offline` from its
  `updatedAt` age (issue #4957) and entries older than 7 days are pruned on
  the next snapshot build, so a host that stops reporting eventually
  disappears rather than rendering its last-known state as current forever.
- **`hosts` table** — per-host ingest key auth. Only a SHA-256 hash of each
  key is stored; a key is only ever shown once, at creation time.

## Local development

```bash
npm install
npx wrangler d1 migrations apply loom-observability --local
npm run dev          # wrangler dev — binds DB/FLEET_STATE locally
```

`npm run dev` (like `npm test` and `npm run preflight`) first runs
`scripts/ensure-web-dist.sh`, because `wrangler.toml`'s `[assets] directory`
must exist before Wrangler will parse the config at all. That script writes a
labelled placeholder page when the UI has not been built; build the real one
with `npm run install:web && npm run build:web`. For UI development with hot
reload, run Vite alongside `wrangler dev` — see [`web/README.md`](web/README.md).

`wrangler dev` does not read `wrangler.toml`'s `[vars]`-declared secrets —
set `ADMIN_TOKEN` for local testing via a `.dev.vars` file (gitignored,
never commit it):

```bash
cp .dev.vars.example .dev.vars
```

Provision a host and push a batch exactly like the daemon does (bare JSON
array body, `Authorization: Bearer <ingest_key>`):

```bash
curl -X POST http://localhost:8787/admin/hosts \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-dev-admin-token' \
  -d '{"host_id":"my-host"}'
# => {"host_id":"my-host","ingest_key":"<shown once>"}

curl -X POST http://localhost:8787/ingest \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer <ingest_key from above>' \
  -d '[{"schema_version":1,"emitted_at":"2026-07-30T12:00:00Z","host_id":"my-host",
       "record":{"kind":"host.health","captured_at":"2026-07-30T12:00:00Z",
                  "daemon_version":"0.16.0","uptime_sec":10,"logical_cpus":8}}]'

curl http://localhost:8787/admin/fleet-state -H 'authorization: Bearer local-dev-admin-token'
```

## Tests

```bash
npm test          # vitest run, via @cloudflare/vitest-pool-workers (Miniflare)
npm run typecheck # tsc --noEmit
npm run check     # both
npm run test:web  # the UI suite (happy-dom) — a separate runner, see below
npm run check:all # backend + UI
```

The UI has its own Vitest runner (`web/vite.config.ts`, happy-dom): browser
code cannot run inside the Workers runtime, so this project's
`vitest.config.ts` scopes itself to `test/**` and never picks up `web/test/**`.

Every test runs inside the real Workers runtime (Miniflare) against an
isolated in-memory D1 instance with `migrations/` applied — see
`vitest.config.ts` / `test/apply-migrations.ts`.

## Deploying your own instance

The short version:

```bash
npx wrangler login
npx wrangler d1 create loom-observability      # paste database_id into wrangler.toml
npx wrangler d1 migrations apply loom-observability --remote
npm run install:web                            # once, for the dashboard UI
npm run preflight                              # fails while any template placeholder remains
npm run deploy                                 # builds web/ then wrangler deploy
npx wrangler secret put ADMIN_TOKEN            # gates every /admin/* route
```

Then provision each fleet host (`POST /admin/hosts {"host_id": "..."}`,
capture the once-shown `ingest_key`) and point that host's daemon at the
endpoint via its `observability` config block.

**[`docs/deploy-runbook.md`](docs/deploy-runbook.md) is the full
walkthrough** — API-token scopes, verification commands after each step, key
rotation/revocation, retention tuning, teardown, and a troubleshooting
table. To gate the authenticated view behind SSO, follow it with
[`docs/cloudflare-access.md`](docs/cloudflare-access.md).

`npm run preflight` (`scripts/check-deploy-config.sh`) is the guard rail: it
refuses to pass while `wrangler.toml` still holds a template placeholder,
warns when a custom domain is configured with `workers_dev` still enabled
(an unauthenticated bypass around any Access policy), and finishes with
`wrangler deploy --dry-run`. `npm run preflight -- --remote` additionally
asserts the `ADMIN_TOKEN` secret exists on the deployed Worker.

## Routes

| Route | Auth | Purpose |
|---|---|---|
| `GET /` + `/assets/*` | none in-Worker — put behind Cloudflare Access | The dashboard UI (static assets built from `web/`). Served by the asset router before any Worker code runs; the plain-text banner in `src/index.ts` is only the fallback when no assets are uploaded. |
| `POST /ingest` | `Authorization: Bearer <ingest_key>` | Accept a batch (bare JSON array of `TelemetryEnvelope`s). |
| `POST /admin/hosts` | `Authorization: Bearer <ADMIN_TOKEN>` | Provision a host + ingest key (`{"host_id": "..."}`, optional `"key"` to bring your own). |
| `POST /admin/hosts/:hostId/revoke` | `Authorization: Bearer <ADMIN_TOKEN>` | Revoke a host's key and clear its `FleetState` Durable Object entries (issue #4957) — no more rendering the last-known health/tokens sample as current. |
| `POST /admin/retention/run` | `Authorization: Bearer <ADMIN_TOKEN>` | Run the retention sweep on demand (also runs hourly via `[triggers] crons`). |
| `GET /admin/fleet-state` | `Authorization: Bearer <ADMIN_TOKEN>` | Read the Durable Object's live snapshot (operator introspection/manual verification). |
| `GET /api/fleet-state` | none in-Worker — put behind Cloudflare Access (see below) | Authenticated query API: current state of every known host/sweep, full detail. |
| `GET /api/history` | none in-Worker — put behind Cloudflare Access | Authenticated query API: filterable, paginated D1 history query, full detail. |
| `GET /api/events` | none in-Worker — put behind Cloudflare Access | Authenticated query API: SSE live tail of newly-ingested telemetry, full detail. |
| `GET /public/fleet-state` | none (always public) | Public query API: same data as `/api/fleet-state`, redacted per record `visibility` (issue #4727). |
| `GET /public/history` | none (always public) | Public query API: same data as `/api/history`, redacted. |
| `GET /public/events` | none (always public) | Public query API: same data as `/api/events`, redacted. |
| `GET /` | **in-Worker** Access JWT check (`src/accessAuth.ts`) — the only route that verifies a credential itself | The dashboard page (issues #4753, #4795). A valid, correctly-audienced `CF_Authorization` cookie renders the full unredacted view (live feed over `/api/events`); **anything else** — no cookie, malformed/expired/wrong-`aud` token, JWKS fetch failure — falls back to the redacted public view with a Sign in link (live feed over `/public/events`). Never a 302, never a 500. Read-only. |
| `GET /login` | none in-Worker — **must** be behind Cloudflare Access | Bare 302 back to `/`. Its whole purpose is to make Access run the SSO round trip and mint the `CF_Authorization` cookie that `/` then validates. |
| `GET /public` | none (always public) | 301 → `/` (issue #4795 merged the public page into the root; kept for old bookmarks). |

Full request/response shapes for the `/api/*` and `/public/*` routes,
including the exact redaction policy per record kind:
[`docs/query-api.md`](docs/query-api.md). Cloudflare Access setup to gate
`/login`, `/api/*`, and `/admin/*` at the edge — and to leave `/` ungated so
its in-Worker fallback can run:
[`docs/cloudflare-access.md`](docs/cloudflare-access.md).

## Design decisions worth knowing

- **Whole-batch rejection.** `HttpsExporter` only acks (removes from its
  durable queue) a batch on a 2xx response and otherwise retries the whole
  batch with backoff. So a single malformed envelope in an otherwise-valid
  batch rejects the *entire* batch (400, with the offending envelope's
  index in the error message) rather than a partial accept/reject — the
  simpler, correct choice given that retry contract (see `src/index.ts`'s
  `handleIngest` doc comment).
- **Authoritative host identity comes from the key, not the envelope.**
  `envelope.host_id` is an opaque, client-supplied string per the schema;
  the row persisted to D1 always uses the host id bound to the
  authenticated ingest key, so one host's key can never write records
  under another host's identity.
- **The Durable Object update is best-effort.** The D1 write is the
  durability boundary; if updating the live-state DO fails after a
  successful D1 write, the response still succeeds (logging the DO
  failure) rather than causing the exporter to retry and duplicate the
  already-committed D1 rows.
- **`visibility` fail-safe-to-private** is enforced in code
  (`src/telemetry.ts`'s `decodeVisibility`) before every D1 write, exactly
  mirroring the Rust daemon's decode rule: only the exact string
  `"public"` is public, everything else (missing, unknown label, wrong
  type, `null`) is private.
- **Redaction is a per-kind field allowlist, not a blocklist.**
  `src/redaction.ts` names, per record `kind`, exactly which fields survive
  into a private/public response — a future schema field (e.g. an issue
  title, once one exists on the wire) is dropped by default until this
  table is deliberately updated, rather than leaking by accident because no
  one thought to add it to a blocklist. `/api/*` vs `/public/*` is a
  route-based auth split (no JWT/header parsing in this Worker, per the
  epic's "no auth code in the dashboard itself" constraint) — see
  `docs/query-api.md` for the full rationale.
