# Fleet Observability: end-to-end reference

> Epic [#4702](https://github.com/rjwalters/loom/issues/4702), Phase 4
> (#4860). This is the single entry point tying the whole pipeline together —
> **daemon config → wire schema → exporter → Cloudflare backend → dashboard
> views** — matching the map-plus-links pattern `daemon-reference.md` and
> `token-pool.md` already use. It is an operating summary, not a duplicate:
> every claim below has a canonical detail doc linked next to it, and this
> page should stay a map even as those detail docs grow.

## The pipeline, in one picture

```
loom-daemon (per host)
  observability.* config block (opt-in, off by default)
        │  collector: EventBus subscriber -> TelemetryEnvelope
        ▼
  durable disk-backed queue (survives sink outage / sleep)
        │  drains via a jittered-retry loop
        ▼
  exporter: HttpsExporter (default) or OtlpExporter (opt-in, #4858)
        │
        ▼
Cloudflare Worker backend (deploy-your-own, or the 2AM reference instance)
  D1 (durable history) + Durable Object (live "what's running now")
        │
        ├── /api/*     authenticated, full detail   (Cloudflare Access)
        └── /public/*  unauthenticated, redacted     (always reachable)
        │
        ▼
Dashboard UI (served by the same Worker) — authenticated + public views
```

Nothing here is mandatory: with no `observability` block (or `enabled:
false`), the daemon does none of the above — no subscription, no queue file,
no HTTP client, zero extra syscalls. Loom never phones home; every hop in
this pipeline is infrastructure **you** deploy and point your own daemons at.

## 1. Enable telemetry on a daemon

Add the `observability` block to that host's `.loom/config.json`:

```json
{
  "observability": {
    "enabled": true,
    "endpoint": "https://<your-worker>.workers.dev/ingest",
    "ingestKeyFile": "/etc/loom/observability-ingest.key",
    "batchSize": 50,
    "flushIntervalSecs": 30,
    "queueCapacity": 2000
  }
}
```

Precedence is **env > config > default**, the same rule every other
`autonomous.*`-style daemon subsystem follows
(`loom-daemon/src/config_resolver.rs`). Every key has a
`LOOM_OBSERVABILITY_*` env override:

| Config key | Env override | Default |
|---|---|---|
| `enabled` | `LOOM_OBSERVABILITY_ENABLED` | `false` |
| `endpoint` | `LOOM_OBSERVABILITY_ENDPOINT` | unset (disables export) |
| `ingestKeyFile` | `LOOM_OBSERVABILITY_INGEST_KEY_FILE` | unset (disables export) |
| `batchSize` | `LOOM_OBSERVABILITY_BATCH_SIZE` | 50 |
| `flushIntervalSecs` | `LOOM_OBSERVABILITY_FLUSH_INTERVAL_SECS` | 30 |
| `queueCapacity` | `LOOM_OBSERVABILITY_QUEUE_CAPACITY` | 2000 |
| `exporter` | `LOOM_OBSERVABILITY_EXPORTER` | `"https"` (or `"otlp"`, §3) |

The ingest key is **never inline in config** — `ingestKeyFile` is a path the
daemon reads once at startup and holds only in memory, sent solely as an
`Authorization: Bearer` header. A misconfigured block (missing endpoint or
key file) degrades to off; it does not crash the daemon. Source of truth:
`loom-daemon/src/observability/mod.rs`'s module doc (config resolution,
FLAGS-OFF posture, read-only invariant) and its `collector.rs` / `queue.rs` /
`exporter.rs` / `sender.rs` siblings (collector, durable queue, exporter
trait + HTTPS implementation, retry-drain loop).

## 2. What gets sent: the wire schema

Every push is a batch of versioned `TelemetryEnvelope`s
(`schema_version`, `emitted_at`, `host_id`, `record`). Record kinds:
`sweep.started`, `sweep.phase`, `sweep.completed`, `sweep.outcome`
(repo-scoped, each carrying a `visibility: public|private` tag derived from
the forge, private-by-default and private-safe-by-construction),
`tokens.snapshot`, `host.health` (host-level, no repo/visibility). Full
field-by-field reference, the `visibility` anti-leak contract, and the local
`sweep-outcome-telemetry.jsonl` journal (kept **regardless of whether any
exporter is configured**):
[`.loom/docs/telemetry-schema.md`](telemetry-schema.md).

## 3. Exporters: HTTPS (default) or OTLP (opt-in)

The default exporter is `HttpsExporter` — JSON-over-HTTPS `POST /ingest`,
batched (`batchSize`), retried with jitter, backed by the durable disk queue
so a sink outage or a sleeping host never silently drops data up to
`queueCapacity`. The `Exporter` trait (`exporter.rs`) and the drain loop
(`sender.rs`) are both deliberately generic, so a second sink is a drop-in
addition rather than a rewrite: `OtlpExporter` (epic Phase 4, issue
[#4858](https://github.com/rjwalters/loom/issues/4858)) translates the same
`TelemetryEnvelope` batches into OTLP logs (`/v1/logs`) and metrics
(`/v1/metrics`) requests for operators with an existing OpenTelemetry stack
(a self-hosted collector, Grafana, Honeycomb, …), reusing `sender.rs`'s
drain/retry loop unchanged.

Select it with `observability.exporter = "otlp"`
(`LOOM_OBSERVABILITY_EXPORTER` env override; **env > config > default**,
default `"https"`). It is opt-in twice over: off unless explicitly selected,
*and* gated behind the `otlp` Cargo feature — a default `loom-daemon` build
never compiles in the `opentelemetry-proto` dependency, so choosing
`HttpsExporter` costs nothing extra. The field-by-field
`TelemetryEnvelope` → OTLP mapping (which record kinds become logs vs.
metrics; how `host_id` / `emitted_at` / the repo-visibility tag map onto OTLP
resource/record attributes) is documented in
`loom-daemon/src/observability/otlp/mod.rs`'s module doc comment, verified by
`loom-daemon/src/observability/otlp/mapping.rs`'s unit tests.

**The HTTPS exporter verifies its own identity** (issue #4830). Each `/ingest`
success response echoes the `host_id` the presented key is bound to; the
exporter compares that against the identity this daemon resolved for itself
(`$LOOM_HOST_ID` > `$HOSTNAME` > `hostname`). On a disagreement — the wrong
host's key file installed on a machine, which silently mislabeled a whole
night of telemetry on 2026-07-31 — it logs a WARN **once per daemon lifetime**
and `loom-daemon health` reports an `observability DEGRADED` section (exit
`1`). Nothing else changes: the batch is still acked, and the key's binding
stays authoritative on the backend. Fix by installing the right key or setting
`$LOOM_HOST_ID` to match, then restarting the daemon.

This check is specific to the native ingest protocol, which is what defines
the echo. OTLP/HTTP has no equivalent — a success response carries only
`partial_success`, and a generic OTLP sink has no notion of a per-host key
binding to disagree with — so under `exporter = "otlp"` no mismatch is ever
published and the `observability` health section stays silent. Choosing OTLP
therefore trades this particular misconfiguration guardrail away; keep the
default `"https"` sink if you want it.

## 4. The backend: deploy your own Cloudflare Worker

The Phase-2 backend is a Cloudflare Worker (D1 for durable history, a
Durable Object for live "what's running now" state, an hourly retention
cron) that also serves the dashboard UI as static assets. Full deploy
runbook — Wrangler setup, D1 migrations, admin token, per-host ingest key
provisioning, verifying telemetry lands — is
[`dashboard/docs/deploy-runbook.md`](../../dashboard/docs/deploy-runbook.md).
This is **your own infrastructure**; nothing in Loom points at a shared
backend by default.

Per-host reporting is deliberately redundant — every host independently
emits `tokens.snapshot` / `host.health` for the full account pool it can see,
at real storage cost but with no single point of failure. This was evaluated
as a trade study (issue #4999) and kept as-is: see
[`.loom/docs/telemetry-schema.md`](telemetry-schema.md#per-host-reporting-redundancy-why-3x-storage-is-intentional-issue-4999)
for the reasoning.

## 5. Authenticated vs. public: two views, one redaction policy

Every query route exists twice — `/api/*` (authenticated, full detail) and
`/public/*` (unauthenticated, always reachable, redacted per record kind) —
enforced both at the edge (a Cloudflare Access policy in front of `/api/*`
only) and in the Worker itself (a per-kind field allowlist, defense in
depth). The dashboard root `/` is a single URL for both audiences: it
verifies the visitor's Access session in-Worker and falls back to the
redacted public variant on any failure (missing/expired/wrong-audience
token, even a JWKS fetch failure) — fail-closed by construction, never a
dead-end login wall for an anonymous visitor.

- Gating setup (custom domain requirement, route map, Access application
  config, the single-URL fallback mechanics):
  [`dashboard/docs/cloudflare-access.md`](../../dashboard/docs/cloudflare-access.md)
- Query API + live event tail, request/response shapes, pagination:
  [`dashboard/docs/query-api.md`](../../dashboard/docs/query-api.md)
- Token/cost analytics (burn curves, forecasting, per-repo attribution, and
  why that surface is authenticated-only): `dashboard/docs/token-analytics.md`

## 6. The 2AM reference instance

`dashboard.2amlogic.com` is a live, operator-owned deployment of this same
backend (not a shared Loom service — every fleet deploys its own). Its
specific account/database IDs, Access application layout, credential file
locations, and cutover history (the hostname-wide Access app was retired in
favor of the single-URL `/login`-scoped layout on 2026-07-31) are recorded
in [`dashboard/docs/reference-deployment.md`](../../dashboard/docs/reference-deployment.md)
— useful as a concrete filled-in example of every value the deploy runbook
asks you to supply, not as a second copy of the how-to.

## 7. Renaming a host

The `host_id` a fleet host reports (`$LOOM_HOST_ID` if set, else `$HOSTNAME`,
else `hostname` — `loom-daemon/src/sweep_registry/mod.rs::host_identity`) is
also the identity its ingest key is bound to and the primary key every stored
telemetry row is filed under. There is no rename endpoint — changing it means
provisioning a **new** identity, cutting the host over, and only then dealing
with the old one. Follow these steps in order; do not skip ahead to step 5 or
6 before step 4 is green. `$BASE` (the Worker's URL) and `$ADMIN` (the admin
token) below are the same values set up in the deploy runbook's §7 admin
token / §8 host provisioning.

1. **Provision the new identity.**

   ```bash
   curl -sS -X POST "$BASE/admin/hosts" -H "authorization: Bearer $ADMIN" \
     -H 'content-type: application/json' -d '{"host_id":"<new-host-id>"}'
   # => {"host_id":"<new-host-id>","ingest_key":"<64 hex chars — SHOWN ONLY ONCE>"}
   ```

   Capture the `ingest_key` now — only its SHA-256 hash is stored server-side.

2. **Install the key on the host.**

   ```bash
   printf '%s' '<the ingest_key from step 1>' > ~/.loom/observability/ingest.key
   chmod 600 ~/.loom/observability/ingest.key
   ```

   (A different path is fine as long as it matches that host's
   `observability.ingestKeyFile` / `LOOM_OBSERVABILITY_INGEST_KEY_FILE` — see
   §9 of the deploy runbook.)

3. **Flip `$LOOM_HOST_ID` and restart the daemon.** On a launchd-managed host,
   edit the `LOOM_HOST_ID` environment entry in the daemon's plist to
   `<new-host-id>`, then cycle the job so launchd picks up the new
   environment (a plain process restart does not re-read the plist):

   ```bash
   launchctl bootout gui/$(id -u)/com.rjwalters.loom-daemon
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.rjwalters.loom-daemon.plist
   ```

   `bootout` is asynchronous — it can return before the old job has fully
   torn down — and a `bootstrap` issued into that window can fail
   transiently with `Bootstrap failed: 5: Input/output error`. Don't treat
   that as terminal: confirm the job is actually gone
   (`launchctl print gui/$(id -u)/com.rjwalters.loom-daemon` fails to find
   it), then re-issue `bootstrap`. Either way, verify the outcome by pid, not
   by the exit code of `bootstrap` itself:

   ```bash
   launchctl print gui/$(id -u)/com.rjwalters.loom-daemon | grep -E 'state|pid'
   # expect: state = running, with a NEW pid distinct from the pre-rename one
   ```

   If `state = running` doesn't appear (or the pid is unchanged), the
   bootstrap didn't take — retry it rather than moving on.

4. **Verify via `loom-daemon health` before touching any data.**

   ```bash
   loom-daemon health
   ```

   Confirm the daemon reports `<new-host-id>` (not the old one, and not an
   `observability_host_id_mismatch`/DEGRADED line — see the deploy runbook's
   §8 mismatch-detection note) and that telemetry is actually landing under
   the new id (§10 of the deploy runbook: a D1 row count or the dashboard
   card for `<new-host-id>`). Do not proceed to step 5 or 6 until this is
   green — an unverified rename with a still-broken exporter means the next
   step either backs up the wrong boundary or revokes the only working key.

5. **Optional: back up and relabel historical D1 rows.** Only worth doing if
   you need continuous historical trend lines across the rename; otherwise
   skip this step entirely and let 90-day retention age the old rows out on
   its own (§"Tuning retention" in the deploy runbook) — that is a legitimate
   default, not a shortcut.

   If you do want continuity, take a backup first (`wrangler d1 export`, or
   the `SELECT` pattern from §10), then relabel. Determine the cutover
   boundary **from the data itself**, not from an assumed timestamp — query
   the actual last row written under the old id and the first row written
   under the new one:

   ```bash
   npx wrangler d1 execute loom-observability --remote \
     --command "SELECT max(ts) FROM records WHERE host_id = '<old-host-id>'"
   npx wrangler d1 execute loom-observability --remote \
     --command "SELECT min(ts) FROM records WHERE host_id = '<new-host-id>'"
   ```

   Only relabel rows at or before the verified last-old-id timestamp; do not
   guess a boundary from wall-clock time, which can silently misattribute
   rows written during any overlap or clock skew around the cutover.

6. **Revoke the old key**, only after step 4 is green and any relabeling in
   step 5 you intended to do is complete:

   ```bash
   curl -sS -X POST "$BASE/admin/hosts/<old-host-id>/revoke" -H "authorization: Bearer $ADMIN"
   ```

### Known gaps

These are real, currently-open sharp edges in this procedure — not fixed by
following the steps above carefully, tracked separately rather than restated
here:

- A daemon restart during the rename (step 3) can silently drop in-flight
  sweep outcome telemetry rather than exporting it before shutdown — #5084.
- Step 4's health check gives no *positive* confirmation that telemetry is
  actually flowing; its silence looks identical whether export is healthy or
  has silently never worked — #5083.
- Revoking the old key (step 6) while sweep entries still reference the old
  `host_id` and are active or orphaned can leave a phantom fleet member on
  the dashboard until those entries clear — #5078.
Reusing a previously-revoked `host_id` (e.g. reclaiming the old name later)
used to 409 with no override; `POST /admin/hosts` now re-provisions a revoked
host in place — minting a new key and clearing the revocation atomically, never
reviving the dead one — and only 409s for a host that is currently live
(#5082).

## Map of every detail doc

| Doc | Covers |
|---|---|
| [`.loom/docs/telemetry-schema.md`](telemetry-schema.md) | Wire envelope, record kinds, visibility contract, local journal |
| `dashboard/docs/deploy-runbook.md` | Deploy your own Cloudflare backend end to end |
| `dashboard/docs/cloudflare-access.md` | Gating the authenticated view behind SSO; single-URL fallback |
| `dashboard/docs/query-api.md` | `/api/*` vs `/public/*` routes, redaction policy, live tail |
| `dashboard/docs/token-analytics.md` | Burn curves, forecasting, per-repo attribution |
| `dashboard/docs/reference-deployment.md` | The 2AM instance specifically — concrete IDs, current state |
| `loom-daemon/src/observability/mod.rs` | Config resolution, collector/queue/exporter/sender source of truth |
