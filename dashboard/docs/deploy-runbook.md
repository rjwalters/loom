# Deploy the fleet observability backend to your own Cloudflare account

This is the deploy-to-your-own-account runbook for the Loom fleet
observability backend (epic
[#4702](https://github.com/rjwalters/loom/issues/4702), Phase 2). Follow it
end to end and you will have your **own** telemetry backend — no dependency
on anyone else's deployment — receiving pushes from one or more
`loom-daemon` hosts.

Companion documents:

| Document | Covers |
|---|---|
| [`../wrangler.toml`](../wrangler.toml) | The deployment template itself — every value you must supply is tagged `CHANGE ME` |
| [`../web/README.md`](../web/README.md) | The dashboard UI this Worker also serves — architecture, local development, why it deploys as Workers Assets |
| [`cloudflare-access.md`](cloudflare-access.md) | Gating the authenticated view behind zero-trust SSO while leaving the public view ungated |
| [`reference-deployment.md`](reference-deployment.md) | The 2AM reference instance (`dashboard.2amlogic.com`) — a concrete, filled-in example of every value this runbook asks you to supply, plus its credential-file locations and current Access layout |
| [`../README.md`](../README.md) | Architecture, routes, local development, tests |
| [`../../.loom/docs/telemetry-schema.md`](../../.loom/docs/telemetry-schema.md) | The wire contract the daemon pushes |

> **Everything here is your own infrastructure.** Loom never phones home:
> the daemon's `observability` block is opt-in, off by default, and points
> only at the endpoint you configure.

---

## 0. What you are deploying

One Cloudflare Worker with three pieces of state, plus the dashboard UI:

- **D1 database** — durable history (`records` table) plus per-host ingest
  keys (`hosts` table, SHA-256 hashed, individually revocable).
- **Durable Object** (`FleetState`) — a live "what is running right now"
  snapshot. Created implicitly on first deploy; nothing to provision.
- **Cron trigger** — hourly retention sweep bounded by `RETENTION_DAYS` and
  `MAX_RECORDS`.
- **Static assets** — the Phase-3 dashboard UI (`web/`), uploaded with the
  Worker. Nothing to provision, but it must be **built** before you deploy;
  `npm run deploy` does that for you. The UI and the API deliberately share one
  hostname so a single Cloudflare Access policy gates both — see
  [`../web/README.md`](../web/README.md).

### Prerequisites

| Requirement | Notes |
|---|---|
| Cloudflare account | The free Workers plan is sufficient to start: it includes D1, cron triggers, and SQLite-backed Durable Objects (this Worker declares `new_sqlite_classes`, the free-plan-eligible storage backend — **not** the paid-only KV-backed classes). |
| Node.js 20+ | `node --version` |
| This repository | Only the `dashboard/` directory is needed (including `dashboard/web/`). |
| ~15 minutes | Steps 1-8 are the whole deploy. |

Cost expectation for a small fleet (a handful of hosts): comfortably inside
the free tier. Telemetry volume is dominated by sweep lifecycle events plus
one `host.health` + one `tokens.snapshot` record per host per 5 minutes.

---

## 1. Install dependencies

```bash
cd dashboard
npm install
npm test              # 83 backend tests, all offline (Miniflare)

npm run install:web   # the dashboard UI's own dependencies
npm run test:web      # 95 UI tests, all offline (happy-dom)
npm run build:web     # -> web/dist, what the Worker uploads as static assets
```

Both suites passing proves your checkout is sound. `npm run check:all` runs
typechecks plus both suites in one command.

> **Why the UI build matters even if you only care about the API**:
> `wrangler.toml` declares `[assets] directory = "./web/dist"`, and Wrangler
> refuses to parse the config while that directory is missing — which would
> break `npm test`, `npm run dev`, and `npm run preflight` too. Those three
> commands therefore run `scripts/ensure-web-dist.sh` first, which writes a
> labelled "not built" placeholder page. Everything works with the
> placeholder; you just would not want to serve it to real users, so the
> preflight warns about it.

## 2. Authenticate Wrangler

Interactive (easiest):

```bash
npx wrangler login
npx wrangler whoami     # confirms the account (and shows the account id)
```

Non-interactive / CI — create an API token at
**Cloudflare dashboard → My Profile → API Tokens** with these permissions,
then export it:

| Scope | Permission |
|---|---|
| Account | `Workers Scripts: Edit` |
| Account | `D1: Edit` |
| Account | `Workers KV Storage: Edit` (used for Wrangler's internal state) |
| Zone (only if you use a custom domain) | `Zone: Read`, `DNS: Edit` |

```bash
export CLOUDFLARE_API_TOKEN="..."
export CLOUDFLARE_ACCOUNT_ID="..."   # required if the token can see >1 account
```

> Setting `CLOUDFLARE_ACCOUNT_ID` in the environment is preferred over
> uncommenting `account_id` in `wrangler.toml` — it keeps your account id out
> of version control.

## 3. Create the D1 database

```bash
npx wrangler d1 create loom-observability
```

Wrangler prints a config snippet containing a `database_id`. **Paste that id
into `wrangler.toml`**, replacing the all-zeros placeholder:

```toml
[[d1_databases]]
binding = "DB"
database_name = "loom-observability"
database_id = "PASTE-THE-ID-WRANGLER-PRINTED"   # <- was 00000000-0000-...
migrations_dir = "migrations"
```

Leave `binding = "DB"` alone — `src/index.ts` reads `env.DB`.

## 4. Apply the schema migrations

```bash
npx wrangler d1 migrations apply loom-observability --remote
```

Verify:

```bash
npx wrangler d1 execute loom-observability --remote \
  --command "SELECT name FROM sqlite_master WHERE type='table'"
# expect: hosts, records (plus sqlite internals)
```

`--local` runs the same migrations against the local Miniflare database used
by `npm run dev`; `--remote` is the one that touches your real D1 instance.

## 5. Preflight

```bash
npm run preflight
```

This fails while any template placeholder survives (all-zeros `database_id`,
`REPLACE_WITH_*`, an `example.com` route), warns about the dangerous-but-silent
misconfigurations, and finishes with a `wrangler deploy --dry-run` so a
config or bundling error surfaces before you touch your account. Expected
output at this point:

```
ok     no REPLACE_WITH_* placeholders remain
ok     database_id is set (…)
ok     wrangler deploy --dry-run succeeded (config parses, Worker bundles)

PASSED: 0 errors, 0 warning(s)
```

Zero **errors** is the bar. One warning about `account_id` is expected and
harmless if you authenticated with `wrangler login` on a single-account
credential.

## 6. Deploy

```bash
npm run deploy      # builds web/ then runs wrangler deploy
```

Wrangler prints the deployed URL —
`https://loom-observability-ingest.<your-subdomain>.workers.dev`. Smoke test
it:

```bash
BASE="https://loom-observability-ingest.<your-subdomain>.workers.dev"

curl -sS -o /dev/null -w '%{http_code} %{content_type}\n' "$BASE/"
# 200 text/html; charset=utf-8

curl -sS "$BASE/public/fleet-state"
# {"hosts":{},"activeSweeps":[]}   <- empty until step 9 lands the first push
```

`/` serves the dashboard UI (issue #4749) in its **redacted public variant**,
with a Sign in link, for any request without a valid Cloudflare Access
session — which is every request at this point, since Access is not
configured yet and `CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` are unset in
`wrangler.toml`. A `200` here (never a redirect, never a 500) is the whole
smoke test; wiring the authenticated variant is
[`cloudflare-access.md`](cloudflare-access.md)'s job.

Open `$BASE/` in a browser and you should get the dashboard, reporting "No
hosts are reporting yet" — that empty state is the correct answer at this
point in the runbook, not a fault.

> If you instead see the plain-text `loom-observability-ingest: see /ingest,
> /admin/*` banner, the UI build did not run — that is the Worker's
> server-rendered fallback when no assets are uploaded. Use `npm run deploy`,
> not a bare `wrangler deploy`.

## 7. Set the admin token

`/admin/*` is gated by an `ADMIN_TOKEN` secret. **Until it is set, every
`/admin/*` route answers `503`** and no host can be provisioned.

```bash
openssl rand -hex 32                       # generate; store it in your password manager
npx wrangler secret put ADMIN_TOKEN        # paste it at the prompt
```

Secrets take effect immediately — no redeploy needed. Confirm:

```bash
npx wrangler secret list                   # ADMIN_TOKEN present
npm run preflight -- --remote              # also asserts the secret exists
```

> If you run `wrangler secret put` *before* the first deploy, Wrangler offers
> to create a draft Worker for the name; accepting that is fine, but deploying
> first (step 6) is the simpler order.

## 8. Provision an ingest key per host

One key per fleet host, so any single host can be revoked without disturbing
the others.

```bash
BASE="https://loom-observability-ingest.<your-subdomain>.workers.dev"
ADMIN="<the ADMIN_TOKEN from step 7>"

curl -sS -X POST "$BASE/admin/hosts" \
  -H "authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' \
  -d '{"host_id":"my-laptop"}'
# => {"host_id":"my-laptop","ingest_key":"<64 hex chars — SHOWN ONLY ONCE>"}
```

**Capture the `ingest_key` now.** Only its SHA-256 hash is stored; there is
no way to read it back.

Choose `host_id` to match what the daemon reports for that machine — it is
`$LOOM_HOST_ID` if set, else `$HOSTNAME`, else the output of `hostname`
(`loom-daemon/src/sweep_registry/mod.rs::host_identity`). The backend always
records the host id **bound to the authenticated key**, so a mismatch is not
a security problem, only a confusing one when you read the data back.

Bring-your-own-key is supported too — `{"host_id":"my-laptop","key":"…"}`.

## 9. Point a daemon at your backend

On each fleet host:

**a. Write the ingest key to a file the daemon can read, and nothing else can.**
The key is *never* inline in config — the daemon reads a path.

```bash
sudo install -d -m 700 /etc/loom
printf '%s' '<the ingest_key from step 8>' | sudo tee /etc/loom/observability-ingest.key >/dev/null
sudo chmod 600 /etc/loom/observability-ingest.key
```

A user-owned path (e.g. `~/.config/loom/observability-ingest.key`, mode
`600`) works equally well; the daemon only needs to be able to read it.
Trailing whitespace/newlines are trimmed.

**b. Add the `observability` block to that host's `.loom/config.json`:**

```json
{
  "observability": {
    "enabled": true,
    "endpoint": "https://loom-observability-ingest.<your-subdomain>.workers.dev/ingest",
    "ingestKeyFile": "/etc/loom/observability-ingest.key",
    "batchSize": 50,
    "flushIntervalSecs": 30,
    "queueCapacity": 2000
  }
}
```

Note the `/ingest` path on `endpoint` — the daemon POSTs the batch to exactly
this URL. Every key also has an env override (**env > config > default**):
`LOOM_OBSERVABILITY_ENABLED`, `LOOM_OBSERVABILITY_ENDPOINT`,
`LOOM_OBSERVABILITY_INGEST_KEY_FILE`, `LOOM_OBSERVABILITY_BATCH_SIZE`,
`LOOM_OBSERVABILITY_FLUSH_INTERVAL_SECS`, `LOOM_OBSERVABILITY_QUEUE_CAPACITY`.
Full reference: `.loom/docs/daemon-reference.md` → "Observability exporter".

**c. Restart the daemon and confirm it armed the exporter:**

```bash
./.loom/scripts/stop-daemon.sh && ./.loom/scripts/start-daemon.sh
# or, on a supervised host: loom-daemon restart --drain

grep observability .loom/logs/daemon.log | tail -5
# observability: enabled (endpoint=https://…/ingest, batch_size=50, flush_interval=30s, queue_capacity=2000)
```

A misconfigured block **degrades to off, it does not crash the daemon** — if
you see `observability: enabled but no endpoint configured` or
`… no ingestKeyFile configured`, or nothing at all, re-check step 9b.

## 10. Verify telemetry is landing

Trigger some activity (dispatch a sweep, or just wait ≤5 minutes for the
periodic `host.health` / `tokens.snapshot` sample), then:

```bash
npx wrangler d1 execute loom-observability --remote \
  --command "SELECT host_id, kind, count(*) AS n FROM records GROUP BY host_id, kind ORDER BY n DESC"

curl -sS "$BASE/admin/fleet-state" -H "authorization: Bearer $ADMIN"
```

You should see rows for your `host_id`, and the Durable Object snapshot
should list the host. Reload the dashboard at `$BASE/` and that host now has a
card; click it for the per-host drill-down (health fields, token pool, and any
in-flight sweeps). If `records` is empty, work the troubleshooting table
below.

---

## Operations

### Rotating an ingest key

`POST /admin/hosts` refuses a `host_id` that already exists (`409`), so
rotation is a two-move operation. Pick whichever fits:

**A. Rolling rotation (no raw SQL, brief dual-identity window)** — provision a
successor identity, cut the host over, then revoke the old one:

```bash
curl -sS -X POST "$BASE/admin/hosts" -H "authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' -d '{"host_id":"my-laptop-2"}'
# update /etc/loom/observability-ingest.key on the host, restart the daemon, then:
curl -sS -X POST "$BASE/admin/hosts/my-laptop/revoke" -H "authorization: Bearer $ADMIN"
```

Historical rows keep the old `host_id`; new rows use the new one.

**B. In-place rotation (same `host_id`, needs one D1 statement)** — delete the
host row, then re-provision the same id with a fresh key:

```bash
npx wrangler d1 execute loom-observability --remote \
  --command "DELETE FROM hosts WHERE host_id = 'my-laptop'"
curl -sS -X POST "$BASE/admin/hosts" -H "authorization: Bearer $ADMIN" \
  -H 'content-type: application/json' -d '{"host_id":"my-laptop"}'
```

Between the delete and the daemon picking up the new key, that host's pushes
get `401` — they are **not lost**: the daemon's durable queue retries with
backoff and drains once the new key is in place (up to `queueCapacity`).

### Revoking a host

```bash
curl -sS -X POST "$BASE/admin/hosts/<host_id>/revoke" -H "authorization: Bearer $ADMIN"
```

Takes effect on the next request. Other hosts are unaffected. Already-stored
records are retained (revocation stops writes, it does not erase history).

### Tuning retention

`RETENTION_DAYS` and `MAX_RECORDS` in `wrangler.toml`'s `[vars]`; both are
enforced on every hourly sweep. Change them and redeploy, then optionally
force a sweep:

```bash
curl -sS -X POST "$BASE/admin/retention/run" -H "authorization: Bearer $ADMIN"
# => {"deletedByAge":…,"deletedBySize":…}
```

### Custom domain and SSO

A `*.workers.dev` URL cannot be protected by Cloudflare Access — Access only
covers hostnames in a zone you own. To gate the authenticated view behind
SSO, attach a custom domain (`[[routes]]` in `wrangler.toml`), set
`workers_dev = false`, and follow
[`cloudflare-access.md`](cloudflare-access.md). `npm run preflight` warns
when a custom domain is configured while `workers_dev` is still enabled,
because that leaves an unauthenticated bypass around the Access policy.

### Tearing it all down

```bash
npx wrangler delete                                   # removes the Worker (and its Durable Object)
npx wrangler d1 delete loom-observability             # removes all stored telemetry
```

Then set `observability.enabled` to `false` on every daemon host (or remove
the block) and restart, so daemons stop queueing pushes to a dead endpoint.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `wrangler deploy` → "more than one account" | Credential spans several accounts | `export CLOUDFLARE_ACCOUNT_ID=…` (`wrangler whoami` lists them) |
| Deploy fails on `database_id` | Placeholder never replaced | Step 3; `npm run preflight` catches this |
| `/admin/*` returns `503` | `ADMIN_TOKEN` secret unset | Step 7 |
| `/admin/*` returns `401` | Wrong admin token | Compare with what you stored; `wrangler secret put ADMIN_TOKEN` to reset |
| `/ingest` returns `401` | Unknown or revoked ingest key | Re-provision (step 8); confirm the key file has no stray characters |
| `/ingest` returns `400 envelope N: …` | Malformed batch — a whole batch is rejected on the first bad envelope | Version-skew between daemon and backend; check `.loom/docs/telemetry-schema.md` |
| Daemon log shows nothing about observability | Block absent or `enabled` false | Step 9b, then restart |
| `observability: enabled but no endpoint configured` | Missing/empty `endpoint` | Step 9b |
| Records stop arriving after a host sleeps | Expected — the durable queue drains on wake | No action; check `observability-queue.jsonl` growth if it persists |
| `records` grows without bound | Cron trigger not firing | `wrangler deployments list`; force with `POST /admin/retention/run` |
| `GET /` shows "Dashboard UI not built" | Deployed the placeholder — a bare `wrangler deploy` skipped the UI build | `npm run install:web && npm run deploy` |
| `GET /` returns the plain-text route banner | No assets were uploaded at all | Confirm `[assets]` is present in `wrangler.toml`, then `npm run deploy` |
| Dashboard shows "No hosts are reporting yet" | Correct empty state — no host has pushed yet | Steps 8-9; verify with step 10 |
| Dashboard shows "your Cloudflare Access session may have expired" | Access session lapsed, or `/api/*` is gated but the browser session is not | Reload to re-authenticate ([`cloudflare-access.md`](cloudflare-access.md)) |
| A host card shows `—` for CPU/load/disk | Expected — the daemon omits a measurement it could not take, and the UI never renders an absent value as zero | No action |
| `wrangler` errors "assets.directory ... does not exist" | Ran `wrangler` directly on a checkout where the UI was never built | `bash scripts/ensure-web-dist.sh`, or `npm run build:web` |

---

## Validation status

The template and every command in this runbook were validated against the
Wrangler CLI (`wrangler deploy --dry-run` for the default config, the
commented `[[routes]]` custom-domain variant, and the commented
`[env.staging]` variant) and against the backend's own 83-test Miniflare
suite plus the dashboard UI's 95-test happy-dom suite. The dashboard was
additionally validated end to end against a local `wrangler dev`: assets
served at `/`, two hosts provisioned through `/admin/hosts`, telemetry pushed
through `/ingest`, and the aggregated `/api/fleet-state` response rendered
through the real view code. **A live from-scratch deploy against a real
Cloudflare account — steps 2-10 end to end, including a real daemon push — has
not been performed** and is recommended before treating this as fully proven.
Please report any step that does not work as written.

The 2026-07-31 deploy of the [2AM reference instance](reference-deployment.md)
was a real production deploy on a real account, but **it does not satisfy the
"live from-scratch deploy" item above** — the first Worker that went live at
that instance's domain turned out to be a bindings-less shell (no D1, no
Durable Object), which points at that deploy not having followed this
runbook's steps verbatim from the start. See
[`reference-deployment.md`](reference-deployment.md) §7 for what happened.
The outstanding validation this line calls for is still: a fresh account,
this runbook's steps 1-10, no shortcuts, checked off one by one.
