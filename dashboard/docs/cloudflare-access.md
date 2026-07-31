# Gating the dashboard with Cloudflare Access

How to put zero-trust SSO in front of the **authenticated** fleet view while
leaving the **public** view and the machine-to-machine `/ingest` endpoint
reachable without a login.

This is the Cloudflare-side configuration only. Phase 3 of epic
[#4702](https://github.com/rjwalters/loom/issues/4702) builds the actual
public-view page; the Access policies that distinguish the two routes are set
up here, at the edge, and are what make that split enforceable.

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

| Path | Access decision | Why |
|---|---|---|
| `/ingest` | **Bypass** | Machine-to-machine. `loom-daemon` sends `Authorization: Bearer <ingest_key>`, not an SSO cookie — an Access challenge here breaks every push in your fleet. Already authenticated by the per-host key (`src/auth.ts`). |
| `/public` *(Phase 3)* | **Bypass** | The deliberately public, redacted fleet view. Reserve the path now so the policy is in place before the page exists. |
| `/admin/*` | **Allow** (+ optional service-token policy) | Host-key management. Already gated by the `ADMIN_TOKEN` bearer secret; Access in front of it is defense in depth. |
| everything else (`/`, the authenticated view) | **Allow** | The private fleet view: your identities only. |

Cloudflare evaluates the **most specific matching application** — a
path-scoped app (`loom-dashboard.example.com/ingest`) wins over a
hostname-wide app (`loom-dashboard.example.com`). That precedence is what
lets one hostname host both gated and ungated routes.

**Create the Bypass applications before the hostname-wide Allow
application.** In the window between "hostname is gated" and "`/ingest` is
bypassed", every daemon push gets an Access redirect instead of a 2xx and
backs off. Nothing is lost (the daemon's durable queue retries), but you will
watch your fleet go quiet for no reason.

---

## 3. Configure it (Zero Trust dashboard)

Cloudflare dashboard → **Zero Trust** → **Access** → **Applications**.

### 3a. First-time setup: an identity provider

**Settings → Authentication → Login methods**. The built-in **One-time PIN**
(email) provider needs no configuration and is enough to start; Google /
GitHub / Okta / any OIDC or SAML provider works the same way from Access's
point of view.

### 3b. Bypass application for `/ingest`

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

Repeat for path `public` (`loom-dashboard public view (bypass)`), so the
Phase-3 public page is ungated the day it lands.

### 3c. Allow application for the authenticated view

**Add an application → Self-hosted**

| Field | Value |
|---|---|
| Application name | `loom-dashboard (private)` |
| Session duration | 24 hours (your call) |
| Public hostname | `loom-dashboard.example.com` (no path — covers everything not matched by a more specific app) |

Policy:

| Field | Value |
|---|---|
| Policy name | `fleet operators` |
| Action | **Allow** |
| Include | **Emails** → your addresses (or **Emails ending in** `@yourcompany.com`, or an Access group) |

### 3d. Optional: a service token for scripted `/admin` calls

Once `/admin/*` sits behind Access, an interactive login is required — which
breaks `curl`-driven host provisioning. Two options:

**Service token (recommended).** **Access → Service Auth → Create service
token**, then add a second policy on the private application:

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

**Or** add a third Bypass application scoped to `admin` and rely solely on
`ADMIN_TOKEN`. That is a real, defensible choice — the routes are already
authenticated — but it drops the second factor; prefer the service token.

---

## 4. Verify the split

```bash
BASE=https://loom-dashboard.example.com

# Gated: an unauthenticated browser request is redirected to the Access login.
curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' "$BASE/"
# expect 302 → https://<your-team>.cloudflareaccess.com/cdn-cgi/access/login/...

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

**The 401-vs-302 distinction on `/ingest` is the single most useful check
here** — a 302 means Access is intercepting the daemon's pushes.

---

## 5. Optional hardening: verify the Access JWT in the Worker

Access injects a signed `CF-Access-Jwt-Assertion` header on every request it
lets through. Verifying it inside the Worker closes the gap where someone
finds a way to reach the Worker without traversing Access (a leftover
workers.dev route, a `[[routes]]` pattern on another hostname).

**This is not implemented today** — the Worker trusts the edge. It is a
sensible Phase-3 addition, and the ingredients are:

- JWKS: `https://<your-team>.cloudflareaccess.com/cdn-cgi/access/certs`
- Expected `aud`: the application's **Application Audience (AUD) tag** (shown
  on the app's overview page).
- Verify signature, `aud`, `iss`, and expiry before serving the private view;
  skip the check on the Bypass paths (Access sends no JWT there).

Until then, the belt-and-braces controls that *are* live are `workers_dev =
false` (§1) and the `ADMIN_TOKEN` bearer on `/admin/*`.

---

## 6. Pitfalls

| Symptom | Cause | Fix |
|---|---|---|
| Every daemon push suddenly fails / fleet goes quiet | `/ingest` is covered by the hostname-wide Allow app | Add the `/ingest` Bypass app (§3b) — it takes precedence |
| Access login works but the Worker 404s | Custom domain route not deployed | `wrangler deploy` after adding `[[routes]]` |
| The private view is reachable without logging in | workers.dev still enabled, or another route/hostname points at the Worker | `workers_dev = false`, redeploy, and audit `wrangler deployments list` / your zone's routes |
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
