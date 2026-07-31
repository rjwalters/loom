# Gating the dashboard with Cloudflare Access

How to wire zero-trust SSO into the **single-URL dashboard** (issue
[#4795](https://github.com/rjwalters/loom/issues/4795)): one hostname,
`dashboard.example.com`, where an anonymous visitor to `/` sees the
redacted public view and an allowed identity sees the full view — no second
URL, no dead-end login wall — plus the machine-to-machine `/ingest` endpoint
and the `/admin/*` management routes, both reachable without a browser login.

**This is a Worker-code-plus-config split, not config alone.** Cloudflare
Access cannot itself express "try to authenticate, otherwise serve something
else" — an Access **Allow** application always forces a login, full stop.
The single-URL fallback works because `src/index.ts`'s root `/` handler does
its own in-Worker Access-JWT check (`src/accessAuth.ts`) and falls back to
the public view when it finds no valid one; Access itself only gates the
narrow `/login` path that mints the session cookie the root handler reads.
This doc covers the Cloudflare-side configuration that makes that split
correct; the code side is `src/accessAuth.ts` and `src/index.ts`.

Prerequisite: a deployed backend — see
[`deploy-runbook.md`](deploy-runbook.md).

---

## 1. The constraint that shapes everything: workers.dev cannot be gated

Cloudflare Access protects **hostnames in a zone on your account**. A
`*.workers.dev` URL is not in your zone, so **no Access policy can cover
it**. If your Worker is reachable at both `dash.example.com` (gated) and
`loom-observability-ingest.you.workers.dev` (not gated), the gate is
decorative — anyone who guesses the workers.dev URL walks straight past it.

So, before any Access configuration:

1. Attach a custom domain in `wrangler.toml`:

   ```toml
   [[routes]]
   pattern = "loom-dashboard.example.com"
   custom_domain = true
   ```

   The zone (`example.com`) must already be active on the same Cloudflare
   account. `custom_domain = true` lets Wrangler create and manage the DNS
   record.

2. **Disable workers.dev** in the same file:

   ```toml
   workers_dev = false
   ```

3. Redeploy and confirm the old URL is gone:

   ```bash
   npm run preflight     # warns if a route exists while workers_dev is still true
   npm run deploy
   curl -sS -o /dev/null -w '%{http_code}\n' \
     https://loom-observability-ingest.<your-subdomain>.workers.dev/
   # expect 404/530 — the workers.dev route must no longer serve this Worker
   ```

> Requests to a Worker **Custom Domain** traverse Cloudflare's security stack
> (including Access) before reaching your Worker. Requests to a workers.dev
> subdomain do not. That difference is the whole reason step 2 is mandatory
> rather than cosmetic.

---

## 2. Route map: what to gate and what to leave open

This is the **single-URL fallback layout** (issue #4795) — the reference
layout as of this writing. See §2a below for the split-path layout this
replaced, kept only as a variant note for deployments that haven't
re-plumbed yet.

| Path | Access decision | Why |
|---|---|---|
| `/ingest` | **Bypass** | Machine-to-machine. `loom-daemon` sends `Authorization: Bearer <ingest_key>`, not an SSO cookie — an Access challenge here breaks every push in your fleet. Already authenticated by the per-host key (`src/auth.ts`). |
| `/healthz` | **Bypass** | Cloudflare-level health check, no Worker route behind it — unrelated to this Worker's own auth, just needs to stay reachable without a login. |
| `/login` | **Allow** | The *only* path this reference layout gates. Its sole purpose is to force the SSO round trip and mint the `CF_Authorization` session cookie, then bounce back to `/` (`src/index.ts`'s `/login` route is a bare redirect — Access has already done the real work by the time the Worker sees the request). |
| `/admin/*` | **Allow** (+ service-token policy) | Host-key management. Already gated by the `ADMIN_TOKEN` bearer secret; Access in front of it is defense in depth. Its own application now (previously the service-token policy lived on the hostname-wide app) — scripted admin never touches `/login`, so giving it a dedicated app keeps the two identity policies from tangling. |
| `/api/*` | **Allow** | The authenticated JSON query API (`docs/query-api.md`). Kept on its own dedicated app with the same identity policy `/login` uses — narrowing the old hostname-wide app to `/login` only would otherwise leave `/api/*` matched by **no** application at all, i.e. wide open, which is not this issue's intent. |
| `/`, `/public`, everything else | **No Access application at all** (not even Bypass — simply unmatched) | `/` does its own in-Worker JWT check (`src/accessAuth.ts`) and falls back to the public view on anything but a fully valid, correctly-audienced token; `/public` is a bare 301 to `/`. Neither needs an edge policy. |

Cloudflare evaluates the **most specific matching application** — a
path-scoped app (`dashboard.example.com/login`) wins over a hostname-wide
one. With this layout there is no hostname-wide application left at all: every
gated path (`/login`, `/admin/*`, `/api/*`) gets its own narrowly-scoped app,
and everything else (chiefly `/`) is simply never matched by any Access
application, which is equivalent to Bypass but doesn't need to be declared
as one.

**Create `/ingest` and `/healthz` Bypass applications (if you deploy any at
all — most reference deployments don't need an explicit Bypass app for a
path no Access application would otherwise match) before the `/login`,
`/admin/*`, or `/api/*` Allow applications.** In the window between "an
Allow app exists" and "the matching Bypass app exists", a request to the
not-yet-bypassed path gets an Access redirect instead of reaching the
Worker. This mostly matters if you widen an Allow app's path prefix by
mistake (e.g. accidentally scoping `/admin/*`'s app to `/` during a config
edit) — with the narrow, path-scoped apps this layout uses, it should not
come up in normal operation.

### 2a. Variant: the split-path layout (legacy, pre-#4795)

Before issue #4795, this backend had no in-Worker JWT verification: `/`
itself was Access-gated (Allow, hostname-wide) and `/public` was a
separately-Bypassed page with its own content. That layout is still valid
Cloudflare configuration — Access doesn't care what the Worker does — but it
dead-ends an anonymous visitor to `/` at the SSO login wall instead of
falling back to the public view, which is the exact UX gap #4795 closed. If
you're running that older layout:

| Path | Access decision | Why |
|---|---|---|
| `/ingest` | **Bypass** | Same as above. |
| `/public` | **Bypass** | The public, redacted fleet view — a separate path from `/`. |
| `/admin/*` | **Allow** (+ optional service-token policy) | Same as above, but the service-token policy could also live on the hostname-wide app below. |
| everything else (`/`, the authenticated view) | **Allow, hostname-wide** | The private fleet view — anonymous visitors get an SSO redirect, full stop. |

Migrating from this to the single-URL layout is a Worker deploy (this
issue's code) plus the Access-app changes in §2/§3 above/below — no data
migration, no downtime beyond a brief window while you swap the
applications over.

---

## 3. Configure it (Zero Trust dashboard)

Cloudflare dashboard → **Zero Trust** → **Access** → **Applications**.

### 3a. First-time setup: an identity provider

**Settings → Authentication → Login methods**. The built-in **One-time PIN**
(email) provider needs no configuration and is enough to start; Google /
GitHub / Okta / any OIDC or SAML provider works the same way from Access's
point of view.

### 3b. Bypass applications for `/ingest` (and `/healthz`, if you use one)

**Add an application → Self-hosted**

| Field | Value |
|---|---|
| Application name | `loom-dashboard ingest (bypass)` |
| Session duration | *(irrelevant for bypass)* |
| Public hostname | `loom-dashboard.example.com`, path `ingest` |

Policy:

| Field | Value |
|---|---|
| Policy name | `allow all — key-authenticated` |
| Action | **Bypass** |
| Include | **Everyone** |

Repeat for path `healthz` if your uptime checker needs one — there is no
Worker route behind it today (see the Curator note in issue #4795), so this
is purely a Cloudflare-level health-check path, unrelated to anything the
Worker itself does.

### 3c. Allow application for `/login` — the single-URL fallback's SSO bounce

**Add an application → Self-hosted**

| Field | Value |
|---|---|
| Application name | `loom-dashboard login` |
| Session duration | 24 hours (your call — this is how long the `CF_Authorization` cookie the root handler reads stays valid) |
| Public hostname | `loom-dashboard.example.com`, path `login` |

Policy:

| Field | Value |
|---|---|
| Policy name | `fleet operators` |
| Action | **Allow** |
| Include | **Emails ending in** `2amlogic.com` (or **Emails** → specific addresses, e.g. add `rjwalters@gmail.com` as a second Include rule for an external operator identity, or an Access group) |

Note the **Application Audience (AUD) tag** shown on this app's overview
page once created — that value goes in `CF_ACCESS_AUD` in `wrangler.toml`'s
`[vars]` block (see §5 below, which also explains why you list the `/api/*`
app's tag alongside it). `src/accessAuth.ts` pins every JWT it verifies to
that allowlist, so a token minted for an Access app you did not list will
not unlock the dashboard root even though it comes from the same
team/identity provider.

### 3d. Allow application for `/admin/*` — its own app, with the service token

**Add an application → Self-hosted**

| Field | Value |
|---|---|
| Application name | `loom-dashboard admin` |
| Session duration | 24 hours |
| Public hostname | `loom-dashboard.example.com`, path `admin` |

Once `/admin/*` sits behind its own Access application, an interactive login
is required for a human — which breaks `curl`-driven host provisioning.
Attach a **second** policy for scripted callers:

**Service token (recommended).** **Access → Service Auth → Create service
token**, then add this policy alongside the human `fleet operators` policy
from §3c (reuse the same Include rule, or restrict to specific admins):

| Field | Value |
|---|---|
| Policy name | `admin automation` |
| Action | **Service Auth** |
| Include | **Service Token** → the token you created |

Scripts then send both the Access headers and the app's own admin bearer:

```bash
curl -sS -X POST "https://loom-dashboard.example.com/admin/hosts" \
  -H "CF-Access-Client-Id: $CF_ACCESS_CLIENT_ID" \
  -H "CF-Access-Client-Secret: $CF_ACCESS_CLIENT_SECRET" \
  -H "authorization: Bearer $ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"host_id":"my-laptop"}'
```

**Or** use `cloudflared` for ad-hoc human use, which handles the SSO flow:

```bash
cloudflared access login https://loom-dashboard.example.com
cloudflared access curl https://loom-dashboard.example.com/admin/fleet-state \
  -H "authorization: Bearer $ADMIN_TOKEN"
```

**Or** skip the Access app for `/admin/*` entirely and rely solely on
`ADMIN_TOKEN`. That is a real, defensible choice — the routes are already
authenticated — but it drops the second factor; prefer the service token.

### 3e. Allow application for `/api/*` — keeps the JSON query API edge-gated

Narrowing the old hostname-wide app down to `/login` (§3c) would otherwise
leave `/api/*` matched by no Access application at all — wide open, full
unredacted fleet data, to anyone who finds the URL. Give it its own app so
its protection level is unchanged from before this issue:

**Add an application → Self-hosted**

| Field | Value |
|---|---|
| Application name | `loom-dashboard api` |
| Session duration | 24 hours |
| Public hostname | `loom-dashboard.example.com`, path `api` |

Policy: the same `fleet operators` policy as §3c (same Include rule — one
identity, two apps).

---

## 4. Verify the layout

```bash
BASE=https://loom-dashboard.example.com

# The single-URL fallback: an anonymous request to / renders the public
# view directly — 200, no redirect, never a login wall.
curl -sS -o /dev/null -w '%{http_code}\n' "$BASE/"
# expect 200 — a 302 here means an Access app is still covering / (check
# for a leftover hostname-wide app from the split-path layout, §2a)

# The only gated path: an unauthenticated browser request to /login is
# redirected to the Access login.
curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' "$BASE/login"
# expect 302 → https://<your-team>.cloudflareaccess.com/cdn-cgi/access/login/...

# /public is a bare redirect back to /, unauthenticated.
curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' "$BASE/public"
# expect 301 → https://loom-dashboard.example.com/

# Ungated machine path: reaches the Worker, which answers with ITS OWN 401
# (not an Access redirect) when the key is missing/bad.
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$BASE/ingest" -d '[]'
# expect 401 (from src/auth.ts) — a 302 here means the bypass app is missing

# Ungated machine path with a valid key: 200.
curl -sS -X POST "$BASE/ingest" -H "authorization: Bearer $INGEST_KEY" \
  -H 'content-type: application/json' -d '[]'
# => {"accepted":0}
```

Then confirm end to end that a real daemon still pushes: restart it and check
that new rows land.

```bash
npx wrangler d1 execute loom-observability --remote \
  --command "SELECT max(ingested_at) FROM records"
```

Finally, verify the actual SSO round trip as an allowed identity in a
browser: visit `$BASE/`, click **Sign in**, complete the login flow at
`/login`, and confirm you land back on `/` with the full (unredacted)
dashboard — no separate URL to remember.

**The 401-vs-302 distinction on `/ingest` is the single most useful
machine-path check here** — a 302 means Access is intercepting the daemon's
pushes. For the human-facing side, **`/` returning 200 (not 302) is the
single most useful check** — a 302 there means the old split-path layout's
hostname-wide app is still active and this issue's UX fix isn't live yet.

---

## 5. Verify the Access JWT in the Worker (src/accessAuth.ts)

Unlike the rest of this backend, the dashboard root `/` **does** carry its
own in-Worker credential check (issue #4795) — this is what makes the
single-URL fallback possible at all, since Access itself cannot express
"try to authenticate, else serve something else". `src/accessAuth.ts`
validates the `CF_Authorization` session cookie Access sets after a
successful `/login` round trip (not the `CF-Access-Jwt-Assertion` header —
see that module's doc comment for why: `/` isn't traversing an Access
application, so Access never injects that header there).

The ingredients, both configured as plain (non-secret) `[vars]` in
`wrangler.toml`:

- `CF_ACCESS_TEAM_DOMAIN` — your team's Cloudflare Access domain, e.g.
  `yourteam.cloudflareaccess.com`. Used to build the JWKS URL
  (`https://<team domain>/cdn-cgi/access/certs`, fetched and cached per
  Worker isolate) and the expected `iss` claim.
- `CF_ACCESS_AUD` — the **Application Audience (AUD) tag** of the `/login`
  application (§3c above shows where to find it on that app's overview
  page). Every verified token's `aud` claim is pinned to this allowlist, so
  a token minted for an Access app you did *not* list — anyone else's app on
  your zone — cannot unlock the dashboard root.

  **List the `/api/*` app's AUD here too, comma-separated.** Access names
  the session cookie `CF_Authorization` per *hostname* but mints the token
  per *application*, so once the authenticated page opens its `/api/events`
  live tail the browser can be holding the `/api/*` app's token instead of
  `/login`'s. Pin to `/login` alone and an operator who just signed in flaps
  back to the public view on their next page load:

  ```toml
  CF_ACCESS_AUD = "<login app AUD>,<api app AUD>"
  ```

  This is still a pin, not a loophole — only tags you enumerated are
  accepted, and a blank/whitespace-only value is treated as *unconfigured*
  (i.e. `/` renders the public view), never as "accept anything".

Signature, `aud`, `iss`, and expiry are all verified before serving the full
view; **any failure — missing cookie, malformed token, wrong aud/iss,
expired, or even a JWKS fetch failure — falls back to the public view,
never a 500 and never the full view on a doubtful token.** This fail-closed
contract is covered by automated tests in `test/accessAuth.test.ts` and
`test/index.test.ts`; if you're auditing this code path, that fail-closed
guarantee (not merely "does a valid token work") is the property to verify.

Leaving `CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` unset is a supported
configuration too — `/` then always renders the public view, useful for a
deployment that doesn't want the authenticated dashboard at all yet. The
belt-and-braces controls that are *always* live regardless are
`workers_dev = false` (§1) and the `ADMIN_TOKEN` bearer on `/admin/*`.

---

## 6. Pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| Every daemon push suddenly fails / fleet goes quiet | `/ingest` is covered by a wider Allow app | Add the `/ingest` Bypass app (§3b) — the more specific path wins |
| `/` redirects to the Access login instead of showing the public view | A leftover hostname-wide Allow app from the split-path layout (§2a) is still covering `/` | Remove/narrow that app so nothing but `/login`, `/admin/*`, and `/api/*` are gated (§2/§3) |
| Access login works but the Worker 404s | Custom domain route not deployed | `wrangler deploy` after adding `[[routes]]` |
| `/` always shows the public view even for an allowed identity that just logged in | `CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` unset or wrong, or the identity's browser isn't sending the `CF_Authorization` cookie back to `/` (check it isn't scoped to `/login` only — Access sets it zone-wide by default) | Check the `[vars]` values against the `/login` app's own overview page; check the cookie in browser devtools |
| `/` shows the full view right after sign-in, then flips back to the public view on the next load | The browser's one `CF_Authorization` cookie was re-minted by a *different* app on the hostname (typically `/api/*`, once the page opened its live tail), and its `aud` is not in `CF_ACCESS_AUD` | Add that app's AUD tag to `CF_ACCESS_AUD` — it takes a comma-separated list precisely for this (§5) |
| The `/api/*` query API is reachable without logging in | The `/api/*` Allow app (§3e) is missing — narrowing the old hostname-wide app to `/login` alone leaves `/api/*` unmatched by any application | Add the dedicated `/api/*` app from §3e |
| Scripts against `/admin` get an HTML login page | No Service Auth policy | §3d |
| `cloudflared access curl` prompts repeatedly | Session expired | `cloudflared access login <url>` again |
| Policy edits appear to do nothing | Existing Access session cookie | Test in a private window, or `cloudflared access logout` |

---

## 7. Appendix: configuring Access via the API

The dashboard flow above is authoritative; this is a sketch for
infrastructure-as-code setups. Cloudflare's Access API surface changes over
time — check the current
[Access applications API docs](https://developers.cloudflare.com/api/resources/zero_trust/subresources/access/subresources/applications/)
before relying on these shapes.

```bash
CF_API="https://api.cloudflare.com/client/v4"
AUTH=(-H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H 'content-type: application/json')

# Bypass application for /ingest
curl -sS -X POST "$CF_API/accounts/$CLOUDFLARE_ACCOUNT_ID/access/apps" "${AUTH[@]}" \
  -d '{"name":"loom-dashboard ingest (bypass)","type":"self_hosted",
       "domain":"loom-dashboard.example.com/ingest","session_duration":"24h"}'

# ... then attach a Bypass/Everyone policy to the returned app id:
curl -sS -X POST "$CF_API/accounts/$CLOUDFLARE_ACCOUNT_ID/access/apps/<APP_ID>/policies" "${AUTH[@]}" \
  -d '{"name":"allow all — key-authenticated","decision":"bypass","include":[{"everyone":{}}]}'
```

The API token needs **Account → Access: Apps and Policies: Edit**.
