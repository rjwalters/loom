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

## 3a. CI auto-deploy (issue #4958)

`.github/workflows/dashboard-deploy.yml` deploys this instance automatically
on every push to `main` touching `dashboard/**` — see that workflow for the
full pipeline (tests gate the deploy, `wrangler d1 migrations apply` runs
idempotently, the deploying commit is stamped via `--var
BUILD_COMMIT:$GITHUB_SHA` and served at `/api/version` + the dashboard
footer). This section records only the CI-specific secrets it needs, which
extend — do not replace — the local overlay pattern in §3.

| Secret | Contents | Status |
|---|---|---|
| `CLOUDFLARE_API_TOKEN` | The `gha-loom-dashboard-deploy` CI token (Workers Scripts:Edit + D1:Write + Account Settings:Read on the account; Workers Routes:Edit + Zone:Read on the `2amlogic.com` zone) | Provisioned 2026-08-02 |
| `CLOUDFLARE_ACCOUNT_ID` | `a7a402ccb9616532d8f4ee64447affe9` (§1) | Provisioned 2026-08-02 |
| `CLOUDFLARE_WRANGLER_CONFIG_2AMLOGIC` | The **full contents** of this instance's `wrangler.2amlogic.toml` overlay (§3) | **Operator action required** — not yet provisioned as of this writing |

**Why a whole-file secret instead of individual account/database/route
secrets**: those values are not sensitive (§3 already says so), but the
workflow has no safe way to reconstruct them on its own — in particular the
`CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` `[vars]` this instance's overlay also
carries (§4's cutover) are load-bearing for the single-URL Access gate, and a
generated overlay that silently omitted or mis-set them would risk
deploying a Worker that treats every request as unauthenticated. Reusing the
overlay file an operator already maintains locally (§3) avoids the workflow
ever guessing at those values.

**Provisioning it** (from the operator's machine, where `wrangler.2amlogic.toml`
already exists per §3):

```bash
cd dashboard
gh secret set CLOUDFLARE_WRANGLER_CONFIG_2AMLOGIC < wrangler.2amlogic.toml
```

Until this secret exists, the deploy job fails loudly on its first step (a
`::error::` annotation naming the missing secret) rather than deploying with
a wrong or incomplete config — see the workflow's "Materialize the 2AM
instance's wrangler config" step. Re-run the same command any time the
overlay changes (a new D1 database, a rotated route, an Access app change);
no workflow edit is needed.

## 4. Cloudflare Access layout

**Read against the live API, 2026-07-31.** An earlier revision of this
section described an app-per-path layout with dedicated `/login`, `/api/*`
and `/admin/*` applications. **That was never live.** The account actually
had one hostname-wide app plus three bypasses, which matters a great deal:
the hostname-wide app was the only thing gating `/api/*`, so deleting it to
open `/` — the whole point of the single-URL re-plumb — would have exposed
the unredacted fleet. Verify with the API before trusting any table here:

```bash
set -a; . ~/.cloudflare/2amlogic/access-2amlogic.env; set +a
curl -sS -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" \
  "https://api.cloudflare.com/client/v4/accounts/$CLOUDFLARE_ACCOUNT_ID/access/apps?per_page=50"
```

### Current applications

| Application | Path | Action | App ID / AUD |
|---|---|---|---|
| `loom-dashboard login (cookie mint)` | `/login` | Allow | `e1855040-f181-421b-9b86-2db037fe57e8` / `78838a7fdc48659bf54eec1525822827f812feb4463807ac5dcfe35d1c92c28a` |
| `loom-dashboard admin` | `/admin` | Allow + service-token `non_identity` | `4267ee80-138b-4a47-9fe2-1f3917500282` / `9426832665cfa69a4f9134eeae549d8c4fad424f7f9fc8fe5b24acfa6d8ce5b7` |
| `loom-dashboard ingest (bypass)` | `/ingest` | Bypass everyone | `c8edf172-05b3-4a7a-9c15-75079b224e21` |
| `loom-dashboard public view (bypass)` | `/public` | Bypass everyone | `5c2c787a-4311-4d2c-8c17-6f7ddb8cc4d5` |
| `Loom Fleet Dashboard healthz (public)` | `/healthz` | Bypass everyone | `732fb318-706e-47e4-97bd-c8122ee3d3ff` |

**There is no hostname-wide application any more.** `/` is matched by no
Access app at all, which is what lets it serve the public view; the Worker
does its own JWT check there and on `/api/*` (see
[`cloudflare-access.md`](cloudflare-access.md) §2 for why `/api/*` cannot be
gated at the edge).

The identity policy on both Allow apps is `email_domain: 2amlogic.com` OR
`email: rjwalters@gmail.com`. `allowed_idps: []` (all — Google + one-time
PIN). The `/admin` app sets `path_cookie_attribute: true` so its session
cookie is scoped to `/admin` and cannot overwrite the root-scoped cookie the
Worker validates at `/`.

AUD tags and app IDs are **not secrets** — the AUD is visible in the Access
redirect URL of any unauthenticated request — so they are recorded here on
purpose. The service token's *secret* is not, and lives only in §5's file.

### Cutover: completed 2026-07-31

The hostname-wide app (`facc1038-3095-4a43-8b62-6483d0bdac39`, aud
`79a6896a…`) was deleted. `/login` and `/admin` had been created first
because they were additive — both paths were already covered by the root
app's identical policy, so adding them changed no behavior and the risky
step stayed isolated.

**`CF_ACCESS_AUD` carries two audiences, on purpose:**

```
CF_ACCESS_TEAM_DOMAIN = 2amlogic.cloudflareaccess.com
CF_ACCESS_AUD         = 79a6896a…,78838a7f…      # root app, /login app
```

It is a comma-separated allowlist (`accessAuth.ts`'s
`parseAcceptedAudiences`). Carrying both is what made the cutover
zero-downtime: operators holding a cookie minted by the *old* root app kept
working across the deletion instead of being silently demoted to the public
view, and a rollback would still validate. With only the `/login` aud set,
every already-signed-in operator would have dropped to the public view the
moment the Worker deployed — a failure that is completely silent, because
`accessAuth.ts` fails closed.

**Follow-up**: drop `79a6896a…` once the fleet has re-signed-in through
`/login`. Nothing breaks if it lingers — the app it referenced no longer
exists, so no live token can carry that audience — but it is dead
configuration.

Verified after the cutover, anonymously against `dashboard.2amlogic.com`:

| Path | Expected | Got |
|---|---|---|
| `/` | 200, SPA, `authenticated:false` | ✅ |
| `/public/fleet-state`, `/public/history` | 200, redacted | ✅ |
| `/api/fleet-state`, `/api/history`, `/api/events` | 401 | ✅ |
| `/admin/*` | 302 (Access challenge) | ✅ |
| `/public` | 301 → `/` | ✅ |

Plus a data check: 17 private-visibility sweeps present with no `repo`,
`issue` or `sweepId`; no token-pool account identifiers anywhere.

**Expect 1-2 minutes of mixed edge state** after deleting an Access app —
some nodes answer from the pre-delete config and return a `text/plain` 404
with Access's headers (`x-frame-options: SAMEORIGIN`) rather than the
Worker's JSON. Not a fault; re-probe until consistent.

**Rollback**: recreate a hostname-wide Allow app on
`dashboard.2amlogic.com` with the identity policy above plus the
service-token `non_identity` policy. Because both audiences are still
accepted, the recreated app's cookies validate immediately.

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
