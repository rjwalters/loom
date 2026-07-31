# loom-observability-backend

Cloudflare Workers reference backend — **ingest + storage only** — for the
Loom fleet observability epic ([#4702](https://github.com/rjwalters/loom/issues/4702),
Phase 2). Accepts the telemetry batches `loom-daemon`'s `HttpsExporter`
(`loom-daemon/src/observability/exporter.rs`, Phase 1 — #4705) pushes,
authenticates the sending host, persists durable history to D1, and
maintains a live "what is running right now" snapshot in a Durable Object.

Wire format reference: [`.loom/docs/telemetry-schema.md`](../.loom/docs/telemetry-schema.md).

**Out of scope for this issue** (later Phase-2 sibling issues / Phase 3):
a dashboard read/query API, Cloudflare Access auth, visibility-based
redaction beyond faithfully storing the `visibility` tag, and a polished
one-click deploy/hosting template (tracked separately — #4728).

## Architecture

- **D1** (`records` table) — durable history of every accepted record, one
  row per record, indexed for `host_id`/`repo`/time-range queries. Bounded
  by the retention sweep (age + size caps — see `src/retention.ts`).
- **Durable Object** (`FleetState`, singleton) — live per-host health/token
  snapshots plus currently in-flight sweeps. Independent of D1; a
  best-effort cache, not a source of truth (see `src/fleetState.ts`).
- **`hosts` table** — per-host ingest key auth. Only a SHA-256 hash of each
  key is stored; a key is only ever shown once, at creation time.

## Local development

```bash
npm install
npx wrangler d1 migrations apply loom-observability --local
npm run dev          # wrangler dev — binds DB/FLEET_STATE locally
```

`wrangler dev` does not read `wrangler.toml`'s `[vars]`-declared secrets —
set `ADMIN_TOKEN` for local testing via a `.dev.vars` file (gitignored,
never commit it):

```bash
echo 'ADMIN_TOKEN="local-dev-admin-token"' > .dev.vars
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
```

Every test runs inside the real Workers runtime (Miniflare) against an
isolated in-memory D1 instance with `migrations/` applied — see
`vitest.config.ts` / `test/apply-migrations.ts`.

## Deploying your own instance

1. `npx wrangler d1 create loom-observability` and paste the returned
   `database_id` into `wrangler.toml`.
2. `npx wrangler d1 migrations apply loom-observability --remote`.
3. `npx wrangler secret put ADMIN_TOKEN` (generate one with e.g.
   `openssl rand -hex 32` — this gates every `/admin/*` route).
4. `npm run deploy`.
5. Provision each fleet host: `POST /admin/hosts {"host_id": "..."}`, capture
   the returned `ingest_key` (shown once), and configure it on that host's
   daemon (`loom-daemon`'s observability exporter config — endpoint +
   ingest key).
6. Revoke a compromised/decommissioned host at any time:
   `POST /admin/hosts/<host_id>/revoke` — other hosts' keys are unaffected.

A full hosting template + turnkey runbook (one-click deploy button, DNS/
custom-domain guidance, etc.) is a separate, later issue (#4728); the steps
above are the minimum needed to stand up a working instance.

## Routes

| Route | Auth | Purpose |
|---|---|---|
| `POST /ingest` | `Authorization: Bearer <ingest_key>` | Accept a batch (bare JSON array of `TelemetryEnvelope`s). |
| `POST /admin/hosts` | `Authorization: Bearer <ADMIN_TOKEN>` | Provision a host + ingest key (`{"host_id": "..."}`, optional `"key"` to bring your own). |
| `POST /admin/hosts/:hostId/revoke` | `Authorization: Bearer <ADMIN_TOKEN>` | Revoke a host's key. |
| `POST /admin/retention/run` | `Authorization: Bearer <ADMIN_TOKEN>` | Run the retention sweep on demand (also runs hourly via `[triggers] crons`). |
| `GET /admin/fleet-state` | `Authorization: Bearer <ADMIN_TOKEN>` | Read the Durable Object's live snapshot (introspection/manual verification — not the Phase-3 dashboard query API). |

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
