# The 2AM reference deployment (dashboard.2amlogic.com)

A durable record of the **one live, operator-owned instance** of this backend
— epic [#4702](https://github.com/rjwalters/loom/issues/4702)'s "2AM"
reference deployment — so the next person who needs to touch it (redeploy,
rotate a credential, add a host, debug an incident) does not have to
rediscover the account, database, Access apps, and credential locations from
scratch. That rediscovery is exactly what happened on 2026-07-31 (see
"Incident: the shell-without-bindings deploy" below); this document exists so
it does not happen again.

This is **not** a second copy of the generic instructions —
[`deploy-runbook.md`](deploy-runbook.md) and
[`cloudflare-access.md`](cloudflare-access.md) remain the how-to for
deploying your *own* instance. This doc records the specific values,
locations, and current state of the *2AM* instance only.

**No secret values live in this file** — only account/resource IDs (not
secrets), file paths, environment-variable names, and instructions for where
to obtain or mint the actual credentials. If you are looking for a token or
key value, it is not here; follow the path/location to the machine-local file
or Cloudflare dashboard that holds it.

---

## 1. Worker identity

| Field | Value |
|---|---|
| Worker name | `loom-fleet-dashboard` |
| Cloudflare account ID | `a7a402ccb9616532d8f4ee64447affe9` |
| Custom domain | `dashboard.2amlogic.com` |
| `workers_dev` | `false` (disabled — see [`cloudflare-access.md`](cloudflare-access.md) §1 for why this is load-bearing, not cosmetic) |

**Naming gotcha that bit the 2026-07-31 deploy**: the committed
[`wrangler.toml`](../wrangler.toml) template's `name` field is
`loom-observability-ingest` — that is the *template's* create-name, used when
you deploy straight off the committed file. The 2AM instance overrides this
to `loom-fleet-dashboard` via the local config overlay (§3 below). If you
`grep` this repo for `loom-fleet-dashboard` expecting to find it in
`wrangler.toml`, you won't — it only exists in the uncommitted overlay file
on the operator's machine and in Cloudflare's own records (`wrangler
deployments list --config wrangler.2amlogic.toml`, or the dashboard UI).

## 2. D1 database

| Field | Value |
|---|---|
| Database name | `loom-fleet-telemetry` |
| Database ID | `e96d9d26-aa3a-4bd0-8dd2-aa32025364db` |
| Migrations | [`dashboard/migrations/`](../migrations/) (currently `0001_init.sql`) — same migrations directory the template uses, applied with `--remote` against this database |

Same naming-mismatch note as §1 applies: the template's default
`database_name` is `loom-observability` (see `wrangler.toml`'s
`[[d1_databases]]` block); the 2AM instance's database is named
`loom-fleet-telemetry` instead. Both `database_name` *and* `database_id` are
overridden in the local config overlay — get either one wrong and `wrangler`
either fails to find the database or (worse) silently binds to the wrong one
if you happen to have more than one D1 database in the account.

## 3. Local config overlay pattern (`wrangler.2amlogic.toml`)

The 2AM instance's account-specific values (Worker `name`, `database_id`,
`database_name`, the `[[routes]]` custom-domain block, `workers_dev = false`)
live in **`dashboard/wrangler.2amlogic.toml`** — a full copy of
[`wrangler.toml`](../wrangler.toml) with those fields substituted.

**This overlay file is never committed.** It exists only on the operator's
machine. This is deliberate: `wrangler.toml` is both the working config for
this repo's test suite *and* the public template (see its own header
comment); a real deployment's account ID, custom domain, and database
identifiers do not need to be secret, but they also have no reason to be
committed to a public template repo, and keeping them out avoids the file
churning every time the reference deployment's config changes independent of
the template.

**How it is generated** (there is no script — do this by hand, or write one
if this becomes a repeated multi-instance need):

```bash
cd dashboard
cp wrangler.toml wrangler.2amlogic.toml
# then hand-edit wrangler.2amlogic.toml:
#   name              -> loom-fleet-dashboard
#   database_name     -> loom-fleet-telemetry
#   database_id       -> e96d9d26-aa3a-4bd0-8dd2-aa32025364db
#   workers_dev       -> false
#   [[routes]] pattern -> dashboard.2amlogic.com (custom_domain = true)
```

**Using the overlay** — `wrangler`'s `-c`/`--config` flag points every
command at an alternate config file instead of the default `wrangler.toml`;
the `npm run deploy` / `npm run preflight` package-script shortcuts do not
take extra arguments, so invoke `wrangler` (and this repo's preflight script)
directly with the overlay when operating on this instance:

```bash
npx wrangler deploy --config wrangler.2amlogic.toml
npx wrangler d1 migrations apply loom-fleet-telemetry --remote --config wrangler.2amlogic.toml
npx wrangler secret list --config wrangler.2amlogic.toml

# scripts/check-deploy-config.sh (the `npm run preflight` target) reads
# LOOM_DASHBOARD_WRANGLER_CONFIG instead of a CLI flag:
LOOM_DASHBOARD_WRANGLER_CONFIG=wrangler.2amlogic.toml npm run preflight
```

**Gitignored**: `dashboard/.gitignore` ignores the `wrangler.*.toml` shape
(any per-instance overlay), while leaving the committed `wrangler.toml`
template untouched — the pattern intentionally does not match a bare
`wrangler.toml` (no `wrangler.<anything>.toml` reads as `wrangler.toml`
itself). If you create an overlay for a different instance later, it is
covered by the same rule; no per-instance gitignore edits are needed.

## 4. Cloudflare Access layout

**Status check performed at doc-writing time (2026-07-31)**: issue
[#4795](https://github.com/rjwalters/loom/issues/4795) (the single-URL
fallback re-plumb) is still open (`loom:building`, not yet merged) — no PR
for it exists yet. `dashboard/src/index.ts` and
[`cloudflare-access.md`](cloudflare-access.md) on `origin/main` still
describe the **app-per-path layout** below, so that is what is actually live
for the 2AM instance today. **Re-check this section against #4795's state
before relying on it** — once #4795 merges and changes the live Access
layout, this section needs a follow-up edit (it does not auto-update).

Per [`cloudflare-access.md`](cloudflare-access.md)'s route map, the 2AM
instance's Zero Trust → Access → Applications configuration is:

| Application | Public hostname / path | Action | Policy |
|---|---|---|---|
| `loom-dashboard (private)` (root app) | `dashboard.2amlogic.com` (no path — everything not matched by a more specific app) | Allow | Emails ending in `2amlogic.com`, plus `rjwalters@gmail.com` explicitly (an out-of-domain address that still needs access); **non_identity policy**: a service token (§5 below) for scripted `/admin` calls |
| `loom-dashboard ingest (bypass)` | `dashboard.2amlogic.com/ingest` | Bypass | Everyone (machine-to-machine — authenticated by the ingest key instead, see `src/auth.ts`) |
| `loom-dashboard public view (bypass)` | `dashboard.2amlogic.com/public` | Bypass | Everyone (the deliberately public, redacted fleet view) |
| *(bypass app for `/healthz`)* | `dashboard.2amlogic.com/healthz` | Bypass | Everyone |

**App IDs**: not recorded in this doc — record them here once you have them
from the Zero Trust dashboard (**Access → Applications**, each app's overview
page shows its ID and Audience/AUD tag) or `curl` the Access Apps API
(`cloudflare-access.md` §7). Treat the table above as the *shape* to
reproduce; fill in the actual IDs as an edit to this file once you have them
to hand, since this doc was written without direct Cloudflare dashboard
access.

**`/healthz` discrepancy**: `dashboard/src/index.ts` does not implement a
dedicated `/healthz` route today (only `/`, `/ingest`, `/admin/*`, `/api/*`,
`/public/*` exist — see the route table in [`../README.md`](../README.md)).
A Cloudflare Access application does not require the path it covers to map
to Worker-side code — bypassing `/healthz` at the edge is valid even though
requests to it currently fall through to the Worker's catch-all 404 — but
this is worth a second look: either an uptime-monitoring integration expects
a real `/healthz` 200 that does not exist yet, or the bypass app is vestigial
and should be removed. Flagging rather than resolving, since resolving it
requires an operator decision (add the route, or delete the unused app), not
a Builder call.

## 5. Credential files (machine-local, never committed)

| Location | Contents | Purpose |
|---|---|---|
| `~/.cloudflare/2amlogic/dashboard-admin.env` | `ADMIN_TOKEN`, `CF_ACCESS_CLIENT_ID`, `CF_ACCESS_CLIENT_SECRET`, `SERVICE_TOKEN_ID` | The Worker's own admin bearer secret (gates `/admin/*`, see `deploy-runbook.md` §7) plus the Access service-token credential pair used for scripted `/admin` calls through the Access gate (`cloudflare-access.md` §3d) |
| `~/.loom/observability/ingest.key` (per host — see §6) | One 64-hex-char ingest key per fleet host | What that host's `loom-daemon` reads via its `.loom/config.json` `observability.ingestKeyFile` (or the `LOOM_OBSERVABILITY_INGEST_KEY_FILE` env override) to authenticate `/ingest` pushes |

Minting a new host's ingest key: `POST /admin/hosts` against the deployed
Worker, exactly as [`deploy-runbook.md`](deploy-runbook.md) §8 documents —
capture the `ingest_key` from the response (shown once) and write it to that
host's `~/.loom/observability/ingest.key` (or an equivalent path — see
`deploy-runbook.md` §9a for permissions), then point that host's
`.loom/config.json` `observability` block at
`https://dashboard.2amlogic.com/ingest`.

## 6. Host enrollment status (as of 2026-07-31)

| Host | Ingest key minted | Verified end-to-end |
|---|---|---|
| `robb-pro` | 2026-07-31 | Yes — confirmed a real `loom-daemon` push landed in D1 |
| `loom-worker-1` | 2026-07-31 | Not yet independently confirmed |
| `robb-studio` | 2026-07-31 | Not yet independently confirmed |

Update this table (or replace it with a live query — `GET /admin/fleet-state`
or a D1 query per `deploy-runbook.md` §10) as hosts are added, rotated, or
revoked; this snapshot will go stale quickly and is not a substitute for
checking the Worker's actual `hosts` table.

## 7. Incident: the shell-without-bindings deploy (2026-07-31)

During the 2026-07-31 push-out of this reference instance, the first
deployed Worker at `dashboard.2amlogic.com` turned out to be a **bindings-less
HTML shell** — it could render a page but had no D1 binding and no Durable
Object, so it could never actually ingest telemetry. The full backend (with
the D1 + Durable Object bindings this repo's `wrangler.toml` declares) was
deployed later the same day, once the gap was noticed. Nothing about the
committed `wrangler.toml` template is broken — `[[d1_databases]]` and
`[[durable_objects.bindings]]` are both declared unconditionally in the
committed file — so the most likely explanation is that the very first
deploy did not go through this repo's `npm run deploy` / `wrangler.toml` at
all (e.g. a quick static/placeholder deploy while other pieces of the
reference instance were still being assembled), and the mistake surfaced
only once someone tried to use `/ingest` for real.

**What this means for `deploy-runbook.md`'s "Validation status" section**:
that section still correctly states that a live from-scratch deploy against
a real Cloudflare account, following the runbook step-by-step, "has not been
performed" as a **deliberate validation exercise**. Today's incident does
**not** satisfy that outstanding acceptance criterion (issue
[#4728](https://github.com/rjwalters/loom/issues/4728), closed) — it was a
real deploy that hit a real problem, but not a controlled run through the
runbook's numbered steps with each one checked off. The from-scratch
validation this repo still owes itself is: starting from a brand-new
Cloudflare account, follow `deploy-runbook.md` steps 1-10 verbatim (no
shortcuts, no separately-assembled placeholder Worker) and confirm each step
produces exactly what the runbook says it will. Until that happens, treat
`deploy-runbook.md`'s validation status as **partially** proven (dry-run +
Miniflare tests, confirmed in the runbook itself) plus **one real production
deploy that needed a same-day fix**, not as a clean confirmation of the
runbook's accuracy end to end.

---

## Redeploying this instance from scratch, using only this document

1. `wrangler login` / confirm account `a7a402ccb9616532d8f4ee64447affe9` (§1).
2. Recreate `dashboard/wrangler.2amlogic.toml` per §3, using the `loom-fleet-dashboard` /
   `loom-fleet-telemetry` / `e96d9d26-aa3a-4bd0-8dd2-aa32025364db` / `dashboard.2amlogic.com`
   values from §1-§2 (if the D1 database itself still exists, reuse its ID rather than
   creating a new one — a new `wrangler d1 create` mints a *different* ID).
3. Apply migrations, preflight, and deploy using the overlay commands in §3.
4. Set `ADMIN_TOKEN` (`wrangler secret put ADMIN_TOKEN --config wrangler.2amlogic.toml`) —
   value lives at `~/.cloudflare/2amlogic/dashboard-admin.env` (§5).
5. Reproduce the Access layout in §4 (Zero Trust dashboard), including the
   service-token policy — the service token's own credential also lives in
   `~/.cloudflare/2amlogic/dashboard-admin.env` (§5).
6. Re-mint or re-verify per-host ingest keys for each host in §6, writing each
   to that host's `~/.loom/observability/ingest.key`.
7. Confirm each host's `.loom/config.json` `observability.endpoint` points at
   `https://dashboard.2amlogic.com/ingest` and restart its daemon.

This is the concrete instantiation of `deploy-runbook.md` + `cloudflare-access.md`
for this specific instance — read those two documents first if any step above
is unclear; this document assumes familiarity with them and only records the
2AM-specific values layered on top.
