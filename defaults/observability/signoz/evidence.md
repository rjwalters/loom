# SigNoz trial evidence

## Deployment identity

Validation began on 2026-09-21 using Foundry **v0.2.17** on macOS arm64 with
Docker Desktop's Linux arm64 VM (8 CPUs, 7.65 GiB). The downloaded Foundry archive
matched its published SHA-256. Registry inspection verified both Linux amd64 and
arm64 for every pinned component index in `casting.yaml`.

`gauge`, `forge`, repeated rendering without a generated diff, and Compose
configuration validation passed. The generated migration/init jobs and persistent
volumes were retained; declarative patches set separate names, private receiver
networking, a loopback-only UI and per-service resource limits. No legacy installer
or new executable Loom shell was introduced.

The host already runs unrelated containers. They were left untouched. Our own
ClickStack trial was stopped at approximately **22:16 UTC**, after its successful
rotation/persistence checks, to make room for SigNoz initialization. This creates
an intentional ClickStack observation gap; it cannot be used for a simultaneous
backend latency comparison. Its volumes remain intact and it must be restored
before the final #8529 comparison.

The first cold bootstrap completed ClickHouse migrations after roughly 15
minutes, followed by 126 PostgreSQL application migrations. Live startup exposed
an unset session-signing secret in the upstream default and an empty active-query
tracker path. The final casting requires the private secret (verified missing-key
Compose rejection) and sets the writable tracker path using SigNoz's doubled-
underscore environment-key escaping. The actual tracker directory exists and
the missing-secret warning is absent. API health returned `{"status":"ok"}`;
a synthetic local account was registered only after the secret was configured.
The account and organization survived app recreation.

The initial Foundry render pointed the ingester's OpAMP endpoint at PostgreSQL's
hostname on port 4320. Inspecting the mounted configuration and original lock
confirmed the incorrect target; it was not a DNS-cache failure. The casting
explicitly overrides `ingester.spec.config.data.opamp.yaml` with the app hostname.
It also declares app-health ordering with `restart: true` for Compose-controlled
updates. A container-running result alone is insufficient ingestion evidence.

The histogram helper's two Linux archives independently matched the upstream
SHA-256 manifest. The patched init job ran successfully in the pinned arm64 image
and printed `histogram-quantile.tar.gz: OK` before extraction. Both architecture
hashes are pinned in the casting; amd64 execution was not performed on this host.

SigNoz's retention APIs accepted 168 hours for traces/metrics and seven days for
logs. Actual DDL confirmed seven-day active signal tables and standard rollups;
the API left some auxiliary/legacy TTLs at 15/30 days. The explicit trial
`retention.sql` shortens those existing TTLs. Resource tables retain the upstream
30-minute grace, and schema/configuration metadata is not subject to signal TTL.
All 16 override statements completed successfully; a subsequent local-table DDL
query found no remaining 15-day, 30-day or one-month TTL among the three signal
databases. Shorter buffer/accounting TTLs and configuration tables were preserved.

## Stored fixture

After the OpAMP correction, all three signals sent through the neutral gateway
were actually indexed. Trace ID `85270000000000000000000000000001` has **4 rows /
4 unique spans**: root `0000000000000001` (`loom.sweep`, Ok, five seconds), rejected
Judge `...0002` (Error), Doctor `...0003` (Ok), and successful Judge `...0004` (Ok).
All three children have the exact root parent ID and one-second duration.
The ERROR log points to the rejected Judge and preserves timestamp
`1790026033610216000` ns and body `Synthetic Judge rejection; no workload content`.
The `loom.host.synthetic_capacity` gauge has value **3** at
`1790026033610` milliseconds. SigNoz's metric schema truncates ns to ms; trace
and log timestamps retain ns. No optional usage or fabricated zero was emitted.

These values match the earlier ClickStack fixture verification. ClickStack was
temporarily restored, then stopped again when host pressure caused queries and
browser startup to take minutes. This establishes sequential storage parity;
it does not establish simultaneous ingest latency or throughput.

The actual authenticated Trace Explorer displayed **four spans and one error**,
with the failed Judge, Doctor and successful retry beneath the root. Selecting
the failed Judge exposed its exact parent/span IDs and Error status. Its **Logs**
tab displayed `Synthetic Judge rejection; no workload content` at the matching
timestamp, establishing actual UI correlation rather than merely compatible
database fields. Sanitized screenshots contain only the synthetic fixture:
[waterfall](evidence/trace-waterfall.png) and
[selected span with correlated log](evidence/correlated-logs.png).
The standard pinned self-hosted image reports the `enterprise` build variant;
no license was supplied, and this proof uses the working base trace/log views.
It does not assert availability of paid features or a complete edition matrix.

A point-in-time sample after ingestion measured approximately **1.24 GiB** across
the five steady-state SigNoz containers (ClickHouse 1.03 GiB, app 105.7 MiB,
Keeper 50.87 MiB, PostgreSQL 30.16 MiB and ingester 27.7 MiB). Active parts in the
three signal databases totaled **77,373 bytes**. These small-fixture observations
exclude PostgreSQL, system tables, images and total volume usage; they are not a
capacity or comparative cost benchmark. The host was heavily contended.

## Restart and recovery

The complete project was stopped without removing volumes, then started from
the final stable deployment copy outside the managed worktree. The checksummed
helper and migration jobs exited successfully; Compose readiness and the actual
ingester receiver both recovered. Before sending anything new, stored counts
remained **4 span rows / 4 unique spans, 1 log and 1 gauge with value 3**. The
existing account authenticated successfully (HTTP 200), and effective DDL still
had no 15-day, 30-day or one-month signal TTL.

Replaying the same three-signal fixture through the still-running neutral gateway
then produced **8 span rows / 4 unique spans, 2 logs and 2 gauges with value 3**.
This verifies receiver recovery and preserves honest duplicate accounting; a
gateway HTTP success alone was not used as the delivery criterion. This was a
normal stop/start, not a volume-loss backup restoration or crash-recovery test.

The later machine-credential audit externalized PostgreSQL's initially upstream-
default password. Only this trial's database role was rotated, through private
stdin; both the database environment and app DSN were checked against the private
machine key using boolean-only output. Foundry percent-encodes placeholders in
DSNs, so the casting asserts the pinned generated value before replacing it with
Compose interpolation. Missing database credentials now fail configuration.
After app/database recreation, health and the existing account login succeeded;
the **8/4 span rows/unique spans, 2 logs and 2 gauges (value 3)** remained intact.
Private credential/session files live outside every checkout with mode 0600.

Attempting to keep both local backends running during this rotation made their
health probes time out under host contention, without OOM flags. Stopping only
the owned ClickStack restored SigNoz readiness. This is a deployment-capacity
limitation of the occupied trial host, not an ingest-latency result. Volumes are
preserved for sequential checks; the intended permanent Cloud destinations need
separate endpoint/credential configuration before any live comparison.

## Shared fixture query artifact

The deployment proof above predates the shared fixture generator. `telemetry-fixture`
(#8578), the durable span export path (#8524, PR #8577) and the owned lifecycle
boundaries (#8525, PR #8579) all merged after this trial's own PR #8574, so the
saved `queries.sql` could only address the ad-hoc single-trace probe: it hardcoded
trace `85270000000000000000000000000001` and the gauge
`loom.host.synthetic_capacity`, neither of which the shared manifest emits. It
therefore could not answer the scope's grouped-failure, grouped-duration,
missing-versus-zero, incomplete-root or privacy questions at all.

`fixture-queries.sql` adds those, run-scoped through the generator's
`host.id = loom-synthetic-<run-id>` resource, covering the manifest's 37 spans, 14
logs and 3 metric data points; failure and duration grouped by
`loom.repo`/`loom.role`/`loom.runtime`/`loom.model`; the repair chain's distinct
retry span IDs; traces with children but no `loom.sweep` root; `mapContains`
absence checks; present-zero versus absent token usage; and a required-zero search
for the fixture's privacy sentinel across all three signals. `queries.sql` is left
untouched as the live-verified record of what actually ran.

**Not executed against a live backend by the change that added it** — that
change ran on a sweep host with no Docker access and the trial deployment
stopped, so its column names followed the pinned v0.142.1 schema and were only
confirmed by its own query 0's design, not by running it. A later session did
execute it live; see "Shared fixture manifest, executed live" below for the
actual results. Nothing in *this* section is itself an observation.

What *is* verified is the artifact's vocabulary rather than its results.
`loom-daemon/tests/signoz_trial_artifacts.rs` runs in ordinary CI, with no Docker,
and derives its expectations instead of restating them: span names, metric names
and span attributes come from a generated fixture manifest, and the forwarding
allowlist is parsed out of the gateway `config.yaml` the deployment mounts. It
fails if a saved query reads an attribute or resource key the gateway's `keep_keys`
strips, filters a span name the fixture never emits, queries a metric name outside
the manifest, quotes an expected span/log/metric total the manifest no longer
reports, or stops asserting that `prompt.content` is dropped. That class of
drift is exactly what produced the stale artifact above, and it does not raise an
error when it happens — the query simply returns zero rows, which on a trial host
is indistinguishable from the backend having lost the data.

## Shared fixture manifest, executed live (2026-09-22)

The gap the previous section leaves open — `fixture-queries.sql` written but
never run — is closed here on a second, independent Foundry deployment (same
pins: Foundry v0.2.17, SigNoz v0.142.1, collector v0.144.10, ClickHouse/Keeper
25.12.5, PostgreSQL 16), this time on macOS arm64 Docker Desktop with the trial
host actually available. `gauge`, a fresh `forge` render into a scratch
directory and `diff -rq` against the committed `pours/` reconfirmed the
deterministic-render acceptance row on this independent host: byte-identical
output, no lock diff. `docker compose ... config --quiet` validated cleanly and
`up -d --wait --wait-timeout 1800` reported all five services healthy well
inside budget. The shared collector gateway (`defaults/observability/collector`)
was also started for the first time against a live SigNoz ingester: its own
`--profile trial run --rm collector validate` and `up -d` succeeded, and
`loom-observability` network inspection confirmed both the gateway and the
SigNoz ingester as the only attached containers, with the ingester reachable at
its documented `signoz-otel-collector:4318` alias.

`loom-daemon telemetry-fixture --run-id 8528a --start-time 2026-09-22T09:00:00Z`
produced the expected 52-envelope bundle (37 spans, 14 logs, 3 metric points).
`loom-daemon telemetry-export` through the gateway at `127.0.0.1:14318`
acknowledged all 52 with zero rejected/dropped, and the gateway's own
Prometheus endpoint (`127.0.0.1:18888/metrics`) independently confirmed
`otelcol_exporter_sent_{spans,log_records,metric_points}` at exactly
37/14/3 for `exporter="otlp_http/signoz"` — the delivery-health check the
README describes as the only place this is visible, since it is not exported
into SigNoz through the OTLP pipeline itself.

`fixture-queries.sql` then ran against the live backend with
`--param_run='loom-synthetic-8528a'`. Full output is preserved for review.
Every assertion the query file documents held on the first pass:

- Query 0 (schema preflight): the pinned v0.142.1 attribute/resource column
  names matched exactly, no reconciliation needed.
- Query 1 (totals): **37 rows, 37 unique spans, 8 traces** — matching the
  manifest exactly, with `rows == unique_spans` confirming no duplicate
  delivery on the first pass.
- Query 2 (full graph): all 8 traces' parent/child structure round-tripped by
  ID, including the deliberate same-timestamp `synthetic/alpha` /
  `synthetic/beta` issue-18 collision the fixture uses to prove parentage is
  never inferred from issue number.
- Query 3 (failure/duration grouping): correct per-repo/role/runtime/model
  aggregation, including the one row with an empty role/runtime/model (the
  fixture's deliberately unlabeled `loom.runtime.preflight` case) staying
  distinct rather than merging into a labeled bucket.
- Query 4 (repair chain): the exact five-span Judge → Doctor → Judge → Merge
  waterfall for trace `16f86ffb55adebac780ef7e1038c75ee`, with the rejected
  Judge attempt (`Error`) and the succeeding retry (`Ok`) both present.
- Query 5 (root-less traces): exactly one match —
  `90a543d371aafafa445e3fa0d4504114`, the fixture's deliberate two-child,
  no-`loom.sweep`-root in-progress/crashed case.
- Query 6 (log correlation): 14/14 logs matched to their span's trace/span ID,
  all `INFO`, spanning `preflight`/`builder`/`judge`/`doctor`/`merge`.
- Query 7 (gauges): `synthetic-zero` has both `loom.tokens.exhausted` and
  `loom.tokens.usage_fraction` at value `0`; `synthetic-unknown` has only
  `loom.tokens.exhausted` — no `usage_fraction` series at all. Absent stayed
  absent; zero stayed a stored zero. Metric timestamps confirmed millisecond
  truncation (`1790067600000`) against the RFC3339 `09:00:00Z` anchor.
- Query 9 (privacy sentinel): **zero** rows across logs, traces and metrics —
  the gateway's `keep_keys` allowlist dropped `prompt.content` before any
  signal reached storage.

A timed replay of the same fixture (duplicate delivery, same `run_id`) then
produced **74 rows / still 37 unique spans / 8 traces** on re-query — at-least-
once delivery counted honestly rather than silently deduplicated. Round-trip
`telemetry-export` wall time for 52 envelopes was 0.57s; the ClickHouse
`clickhouse-client` totals query via `docker compose exec` was 1.15s
(dominated by `exec` overhead, not query execution). A point-in-time
`docker stats` sample after ingestion: ClickHouse 1.45 GiB, ClickHouse Keeper
176 MiB, PostgreSQL 33.5 MiB, SigNoz app 43.5 MiB, ingester 46.4 MiB, gateway
collector 49.7 MiB — consistent with the independent sample in the section
above.

This closes the "written but not executed" gap for the shared fixture
manifest. It does **not** establish the real Loom canary (needs #8525's own
live-run acceptance) or the side-by-side #8529 comparison (needs both trials
up simultaneously, which this session did not attempt since ClickStack was not
deployed here) — both remain open below. Retention was not independently
re-applied on this second deployment; the prior section's seven-day DDL
verification stands and is not repeated here since it is orthogonal to fixture
query execution. UI Trace Explorer / correlated-log screenshots for this
specific run were not captured (no browser automation in this session); the
ad-hoc probe's authenticated UI screenshots above already establish that
acceptance row, and query 2/4/6 above establish the shared-fixture graph and
log correlation are equally queryable.

## Rendered-deployment contract (2026-09-22)

Every deployment property recorded above is an observation of one render at one
moment. `casting.yaml` is meant to be edited and re-rendered, so each of them can
be undone later by a `forge` run, an upstream default change or a hand-edit of the
generated output — with no error raised anywhere, on a host nobody is watching.
Two are security properties: the published-port surface (self-hosted SigNoz's OTLP
receiver is unauthenticated by design) and the `loom-signoz` namespacing that keeps
`down --volumes` away from the trial host's separately-owned SigNoz installation.

`loom-daemon/tests/signoz_deployment_contract.rs` converts eight of them from
one-time observations into CI-enforced invariants, read back out of the committed
`pours/deployment/compose.yaml`. It uses no Docker, network or credential, so
unlike the observations above it re-runs on every commit. The full table is in the
README's "Rendered-deployment contract" section. As with
`signoz_trial_artifacts.rs`, the authorities are derived rather than restated: the
shared-network alias comes from the gateway config's `otlp_http/signoz` endpoint,
the image digests from `casting.yaml`, and the README's memory budget is re-summed
from the rendered `mem_limit`s.

Each assertion was verified to fail on a deliberately mutated render before being
committed: an added `0.0.0.0:4318:4318` mapping on the ingester, a renamed network
alias, a hand-edited image digest, `tar -xzf` moved ahead of the `sha256sum
--check`, a literal `SIGNOZ_TOKENIZER_JWT_SECRET`, a volume renamed outside the
project prefix, a raised ClickHouse `mem_limit`, and a dropped log-rotation cap.
Every mutation failed exactly the intended test with an accurate message. All
mutations were reverted; the committed render is unchanged by this section.

One of those eight mutations was not enough. Review found the credential
assertion vacuous in almost every case it existed for. It scanned for the
secret's own variable name followed by `=` — a string that occurs exactly once
across all three files (`SIGNOZ_TOKENIZER_JWT_SECRET=` in the render), while the
files between them hold **15** sites that actually consume a credential. The
casting and the lock are YAML *mappings* and never spell `VAR=` at all, so a
password pasted into `casting.yaml` — the one file here that is *meant* to be
hand-edited — passed.

The scan is now keyed on the consumption site: a key named
`*password*`/`*secret*` in either the `KEY: value` or `KEY=value` form, plus the
`user:password@host` userinfo of any URL, which is how
`SIGNOZ_SQLSTORE_POSTGRES_DSN` carries the database password under a key that
names neither. Values are normalised first — Foundry's percent-escaped patch
form decoded, the YAML dumper's line wrapping folded back, and each *required*
interpolation collapsed to an opaque marker so a non-required spelling (`$VAR`,
`${VAR}`, `${VAR:-default}`) survives as itself and fails. That finds all 15
sites: 3 in the render, 4 in the casting, 8 in the lock.

Re-mutation-tested across 15 cases: a committed literal at 11 sites covering
every (file × secret × form) combination — mapping, env assignment, plain DSN
userinfo and percent-encoded DSN userinfo, in all three files — both
non-required interpolation spellings, a literal at one of the two exempted
ClickHouse settings, and a credential line deleted outright. The old assertion
caught 3 of the 15; the current one catches all 15. Because a regex scan's
realistic failure is finding *nothing* rather than finding the wrong thing, the
test also asserts a minimum site count per file and that each file consumes each
secret, so a re-render that changes the files' shape fails loudly instead of
silently enforcing nothing.

This closes the "verified once, by hand, unguarded afterwards" gap for the
deployment's trust boundary and supply chain. It is a static contract and
deliberately asserts nothing about a running backend: readiness, ingestion,
retention and query results remain the live sections above.

**Not run on this sweep host**: `docker` returns `permission denied while trying to
connect to the docker API at unix:///var/run/docker.sock`, and no trial volumes
exist here, so nothing in this section is a live observation and no live check was
repeated. That is also why the two open ledger rows below did not advance.

## Linux/amd64 deployment, executed live (2026-09-22)

Every section above was observed on macOS arm64 under Docker Desktop, which left
two claims unexercised: that the pinned multi-platform indexes actually resolve
and run on amd64, and that the histogram helper's amd64 branch works (the section
on deployment identity says so explicitly — "amd64 execution was not performed on
this host"). This section closes both on a third, independent deployment: a native
Linux x86_64 host (Ubuntu 24.04 noble, kernel 6.x, 8 vCPU, 15 GiB RAM, Docker
Engine 29.1.3, Compose v2.40.3), with no Docker Desktop VM in the path.

Foundry **v0.2.17**'s `foundry_linux_amd64.tar.gz` matched the README's recorded
SHA-256 (`51f41204…c0d886`) on independent download. `gauge` left
`casting.yaml.lock` byte-unchanged, and `forge` into a scratch directory produced
output `diff -rq`-identical to the committed `pours/` — the deterministic-render
row now holds across two architectures and three hosts. `docker compose config`
validated with the private env file and **failed closed without it**
(`required variable SIGNOZ_POSTGRES_PASSWORD is missing a value`).

All five pinned index digests resolved to `amd64/linux` and matched
`casting.yaml` exactly: SigNoz `49501b04…3ee8ff`, its collector `34ecb436…204f6`,
ClickHouse server `cacf32d6…dfd3a`, Keeper `525b8b0f…d8524`, PostgreSQL
`a3b7f434…be30d6`. The histogram helper ran its **amd64** path in the pinned
image and printed `histogram-quantile.tar.gz: OK` before extraction, so the
checksum-before-extract ordering is now executed, not merely pinned, on both
architectures.

**Cold bootstrap took 67 seconds** (21:27:02Z → 21:28:09Z) from a cold image
pull to all five services Compose-healthy, against roughly **15 minutes** for the
same render on the arm64 Docker Desktop VM. A subsequent full stop/start took
**30 seconds**. This is strong evidence that the earlier startup cost was a
property of that contended VM rather than of SigNoz, and it is the distinction
this trial is required to keep: it is a host-capacity observation, not a product
performance verdict, and nothing here is a comparison with ClickStack.

The trust boundary was verified by probe rather than by reading the render.
Exactly one host port is published across the whole project —
`127.0.0.1:18081` — and the host's own non-loopback address (`172.31.74.176`)
**refused** the UI, confirming the loopback bind rather than inferring it. OTLP
4317/4318, ClickHouse 8123/9000/9009, PostgreSQL 5432 and every Keeper port
remained container-internal and unpublished. Only the ingester joined
`loom-observability`, carrying the documented `signoz-otel-collector` alias.

### Compose readiness is not ingestion readiness

The most consequential finding of this run, and the reason the README gained a
"Register the first user *before* expecting ingestion" step. `up -d --wait`
reported all five services healthy, yet **the ingester's OTLP receivers were
never open**. The gateway's SigNoz exporter failed every attempt with
`dial tcp 172.18.0.2:4318: connect: connection refused` — DNS resolved, the port
did not exist — while the app logged, every 30 seconds:

```
failed to find or create agent … exception.message: "cannot create agent without orgId"
```

`/api/v1/version` reported `"setupCompleted": false`. The ingester's OpAMP client
cannot register an agent before an organization exists, so it never receives the
effective configuration that binds its receivers. Nothing about this is visible
in SigNoz — there is no partial data, no error surface, no empty-but-present
service — and `docker compose ps` shows healthy throughout.

The causal link was then proven rather than assumed: registering the first
org/user via `POST /api/v1/register` flipped `setupCompleted` to `true`, the
OpAMP errors stopped, the receiver bound, and the gateway's **already-queued**
batches drained on their own. No re-send was issued. Delivery health afterwards
read exactly `otelcol_exporter_sent_spans` **37**, `sent_log_records` **14** and
`sent_metric_points` **3** for `exporter="otlp_http/signoz"` — the full manifest,
recovered intact across the outage window.

That doubles as this trial's **backend outage and recovery** evidence: the
receiver was genuinely unavailable for roughly 40 minutes of retry backoff, and
the shared collector's sending queue preserved all three signals with zero loss.
The contrasting exporter in the same scrape makes the signal legible — ClickStack
was deliberately not deployed here, and its queue stayed pinned at 15 traces / 14
logs / 1 metric with **no** `sent` series at all. One healthy exporter and one
dead backend are trivially distinguishable in `otelcol_exporter_*`, and
indistinguishable from inside either product's UI.

After a full stop/start the organization persisted, `setupCompleted` stayed
`true`, and the app logged **zero** `cannot create agent without orgId` errors —
so the gate is first-run only, not a recurring restart hazard.

### Shared fixture manifest on amd64

`loom-daemon telemetry-fixture --run-id 8528amd64 --start-time
2026-09-22T21:00:00Z` (built from `9958dc7ad` with `--features otlp`) generated
the expected 52-envelope bundle — 37 spans, 14 logs, 3 metric data points, matching
the manifest's own `expected_distinct`. `telemetry-export` through the gateway at
`127.0.0.1:14318` acknowledged all 52 with zero rejected/dropped in 0.42s.

`fixture-queries.sql` then ran against the live backend with
`--param_run='loom-synthetic-8528amd64'`, completing the whole file in **1.79s**
(including `docker compose exec` overhead). Every assertion held on the first
pass, reproducing the arm64 results under different trace IDs:

- Query 0: the pinned v0.142.1 attribute/resource column names matched exactly.
- Query 1: **37 rows, 37 unique spans, 8 traces** — no duplicate delivery.
- Query 2: all 8 traces round-tripped by parent/child ID, including the
  deliberate same-timestamp `synthetic/alpha` / `synthetic/beta` collision.
- Query 3: per-repo/role/runtime/model grouping correct, with the deliberately
  unlabeled `loom.runtime.preflight` row staying its own bucket (empty role,
  runtime and model) instead of merging into a labeled one.
- Query 4: the exact five-span repair chain — builder `Ok` → judge attempt 1
  `rejected`/`Error` → doctor `Ok` → judge attempt 2 `approved`/`Ok` → merge `Ok`
  — with distinct span IDs per attempt.
- Query 5: exactly one root-less trace, `da14dd5676ae8643f74c8145a1c2f89e`, with
  2 children and 0 `loom.sweep` roots.
- Query 6: 14/14 logs matched to their span's trace/span ID.
- Query 7: `synthetic-zero` carried both `loom.tokens.exhausted` and
  `loom.tokens.usage_fraction` at a stored `0`; `synthetic-unknown` carried only
  `exhausted`, with no `usage_fraction` series. Absence stayed absent, zero
  stayed a measured zero. Metric timestamps confirmed millisecond truncation
  (`1790110800000`) against the RFC3339 anchor.
- Query 9: **zero** rows for the privacy sentinel across all three signals; the
  gateway's `keep_keys` allowlist dropped `prompt.content` before storage.

A timed duplicate replay under the same `run_id` produced **74 rows / still 37
unique spans / 8 traces**, and the gateway counters doubled to 74/28/6 — at-least-
once delivery counted honestly at both layers rather than silently deduplicated.

After the restart, the preserved store still read 74 rows / 37 unique spans / 8
traces and 28 log rows, and a *fresh* fixture (`run-id 8528amd64post`) indexed its
37 spans within 10 seconds — so restart persistence and receiver recovery both
hold on this architecture. Note that indexing lag is real: the first query
immediately after export returned 0 rows. A single empty query is not evidence of
loss.

Point-in-time `docker stats` after ingestion, on native Linux rather than a
Docker Desktop VM: ClickHouse 649.9 MiB / 2 GiB, ingester 105.6 MiB / 512 MiB,
SigNoz app 75.2 MiB / 768 MiB, Keeper 42.5 MiB / 256 MiB, PostgreSQL 31.4 MiB /
256 MiB, plus the gateway collector at 42.1 MiB / 512 MiB. Every service sat well
inside its rendered `mem_limit`. Active parts across the three signal databases
totaled **200,013 bytes** (traces 63.58 KiB / 324 rows, logs 38.99 KiB / 121 rows,
metrics 92.76 KiB / 1,763 rows) for three fixture deliveries. These are
small-fixture figures and are not a capacity or cost benchmark.

**Not done in this session**, and deliberately not claimed: retention was left at
the upstream defaults, so this deployment is *not* a seven-day parity
observation — `signoz_index_v3` was confirmed rendering at the upstream
`toIntervalSecond(1296000)` (15 days), which independently corroborates the
README's warning that a fresh render alone does not establish parity. The prior
sections' seven-day API + `retention.sql` + DDL verification stands on its own and
was not repeated. No authenticated UI screenshots were captured (no browser
automation on this host), and the UI view matrix for Loom's non-HTTP span kinds
remains open. The entire trial project was torn down with `down --volumes` at the
end of the session, and the host's unrelated containers were never touched.

## UI view matrix, verified via authenticated API probes (2026-09-25)

Every prior session recorded the UI view matrix as open because none had
browser access. This session did not either, but reached the same conclusion
by a different, still-live-backend method: the exact backend routes each
SigNoz product page calls (identified by reading the pinned
[v0.142.1 frontend source](https://github.com/SigNoz/signoz/tree/v0.142.1/frontend/src)),
called directly with a real session token from a fourth independent
deployment (same host as the "Linux/amd64 deployment" section above, a fresh
`docker compose ... up -d --wait` with an unmodified render — `diff -rq`
against the committed `pours/` was not repeated this time since architecture
parity is already established twice; org registration, once again, was
required before the ingester's receivers opened). This is real backend data,
not an inferred claim, but it is not a screenshot: the browser-only gap
itself is not closed by this session.

Login is `POST /api/v2/sessions/email_password` with `orgId` (returned from
`/api/v1/register`), not the `/api/v1/login` a stale doc might suggest — that
path returns the SPA shell, not JSON, and is easy to mistake for a working
but-empty response.

A fresh `telemetry-fixture`/`telemetry-export` run (`run-id 8528e`) delivered
the full 37/14/3 manifest (confirmed by the gateway's own
`otelcol_exporter_sent_*{exporter="otlp_http/signoz"}` counters and by
`fixture-queries.sql`, which passed every assertion identically to the prior
two live runs). Against that data:

- **Service List / APM overview — populates.** `POST /api/v2/services`
  (the `ServiceTraces` fallback path `frontend/src/api/metrics/getService.ts`
  calls; the alternate `ServiceMetrics` path behind the `USE_SPAN_METRICS`
  feature flag was not separately probed) returned
  `{"serviceName":"loom-daemon","numCalls":7,"numErrors":3,"errorRate":42.86,"p99":48.8s,"avgDuration":18.6s,"dataWarning":{"topLevelOps":["overflow_operation","loom.sweep"]}}`.
  `numCalls` is exactly 7 — the manifest's 8 traces minus the one deliberate
  root-less trace, confirming this view aggregates real root spans rather than
  guessing from any HTTP convention. Loom's spans carry no `http.method`,
  `rpc.system` or any other RED-metric semantic convention; this view works
  anyway because the ingester's own trace pipeline
  (`deployment/ingester/ingester.yaml`) runs every incoming span through
  `signozspanmetrics/delta` unconditionally, regardless of span kind — SigNoz
  computes its own RED metrics from root-span latency/status, it does not
  require the OTel HTTP/RPC conventions the Services page's name suggests.
- **Exceptions ("All Errors") — stays empty.** `POST /api/v1/countErrors`
  returned `0` and `POST /api/v1/listErrors` returned `null` against the same
  window. Root cause, confirmed by reading both sides: this view indexes span
  *events* named `exception` (`exception.type`/`exception.message`), and
  `loom-daemon`'s OTLP mapping
  (`loom-daemon/src/observability/otlp/mapping.rs`) never emits one — Loom
  surfaces a failed attempt as span `status=Error` plus a correlated
  `ERROR`-level log, which is what Trace Explorer's already-passing
  correlation (see the ad-hoc probe screenshots, and query 4/6 in the shared
  fixture section above) actually uses. The three status-`Error` root spans
  counted in `numErrors` above are exactly the ones this page cannot show.
- **Service Map — stays empty.** `POST /api/v1/dependency_graph` returned `[]`.
  `ingester.yaml` configures no service-graph/topology connector at all (only
  `signozspanmetrics/delta`), so this is expected independent of span kind — but
  the shared fixture is also single-service (`loom-daemon` calling itself), so a
  caller/callee edge would never be produced by *any* connector against this
  data. **This session could not separate the two causes and recorded the row as
  unattributable.** The next section resolves it: the fixture's single-service
  shape is not a fixture choice, it is the only shape Loom's exporter can
  produce, so the row is a permanent Loom-shape limitation and not a trial
  configuration gap.

### Resolving the Service Map confound: Loom's trace shape, not the trial's render (2026-09-30)

The row above was left unattributable because two candidate causes were present
at once — a render with no topology connector, and a single-service fixture. That
confound is decidable **without** the trial host, because one of the two is a
property of the emitting code rather than of the deployment. A topology view needs
one of exactly three things to draw an edge:

1. a parent/child span pair carrying two **different** `service.name` values
   (what SigNoz's own dependency graph is built from),
2. a CLIENT/SERVER (or PRODUCER/CONSUMER) span-kind pair, or
3. one span carrying a peer/virtual-node attribute such as `peer.service`, which
   is how the OTel `servicegraph` connector synthesizes an edge when the remote
   side never reports.

Loom's exporter can produce none of the three, and both facts are single unconditional
sites rather than per-call-site conventions:

- `loom-daemon/src/observability/otlp/traces.rs` sets `kind: SpanKind::Internal`
  for every span it builds. There is no branch: **no** span family — lifecycle,
  `loom.ci.*`, `loom.dispatch.*`, `loom.runtime.usage`, `loom.pool.hold` — can be
  any other kind.
- `resource_for_host` in `.../otlp/mapping.rs` sets `service.name` to the literal
  `loom-daemon`. The exporter groups `ResourceSpans` per host id, so a multi-host
  fleet produces many resources and many `service.instance.id` values — but a
  distinct instance is not a distinct **service**, which is the only axis a
  dependency graph reads.

Measured, not inferred, on this session's host: the shared fixture manifest
(#8578, `run-id topologyshape`) was pushed through the real `OtlpExporter` into a
loopback OTLP/HTTP sink in-process, and the captured wire payload — 37 spans
across 15 `/v1/traces` requests and 15 `ResourceSpans` — carried
`kind = 1` (`SPAN_KIND_INTERNAL`) on **every** span, exactly one distinct
`service.name`, and none of the twelve topology peer keys on any span, span event
or resource. No Docker, network, backend or credential was involved.

There is also a second, independent enforcement downstream: the neutral gateway's
`transform/privacy` is an allowlist, and its resource allowlist is exactly
`service.name` / `service.instance.id` / `service.version` / `host.id` while its
span and span-event allowlists contain no peer key. So even a future change that
started emitting `peer.service` would be stripped before either backend saw it.

**Conclusion for the acceptance ledger.** An empty Service Map is attributable,
and it is attributable to Loom: adding a topology connector to the render, or
pointing a multi-service fixture at the trial, cannot produce an edge *for Loom's
data*. This is the same class of answer as the Exceptions page (Loom emits no
`exception` span event) rather than the Service List/APM page (which populates
because `signozspanmetrics/delta` aggregates root spans unconditionally).

**What this is not.** It is not a claim that SigNoz's Service Map is broken, and
not a reason to change Loom: Loom's spans describe one process's own phases, so
there is no second service to draw. It is also not a browser observation — the
`[]` above came from the backend route, and no screenshot has been captured on
any session (that gap stays open, with the rest of #8946).

`loom-daemon/tests/signoz_topology_shape.rs` holds all five assertions, in
ordinary CI (no `--ignored`, no Docker), so the conclusion fails loudly rather
than going stale: the day a Loom span becomes CLIENT-kind, a second
`service.name` appears, a peer key is emitted or admitted by the gateway, or the
render gains a topology connector, the row above needs rewriting and the test
says so. Each of the five was confirmed to fail for its own intended reason
before being accepted.

### A same-session finding: a saturated, deliberately-absent second backend can mask a healthy one's delivery

While preparing this investigation, the shared gateway
(`defaults/observability/collector`) was pointed at this host's real, live
`~/.claude/projects` for `LOOM_CLAUDE_PROJECTS_DIR` — a mistake specific to
this shared 8-vCPU dispatch worker, which runs several concurrent agent
sessions writing to that directory continuously; a single-tenant trial host
would not reproduce this. The `file_log/claude` receiver ingested over 1,600
real log lines in under a minute, which alone filled the **disconnected**
`otlp_http/clickstack` exporter's `sending_queue` (`queue_size: 1000`) —
ClickStack was intentionally never deployed in this SigNoz-only trial, so
every one of its retries failed by design. Once that queue was full, the
OTLP receiver's fan-out `ConsumeLogs` began returning `503` to *any* new
client for the whole request — including this session's own
`telemetry-export` — even though the fan-out's SigNoz branch kept succeeding
and its own queue stayed at `0`. Because `sending_queue` is
`file_storage`-backed (by design, so a real backend outage survives a
restart — see "Backend outage and recovery" above), the stuck ClickStack
queue also survived `docker compose ... down`/`up -d` and kept failing new
requests until `LOOM_COLLECTOR_STATE_DIR` was pointed at a fresh directory.

Two things were checked and are worth stating plainly. First, privacy: the
`file_log/claude` receiver's own `retain`/`remove: field: body` operators ran
before the shared `transform/privacy` processor even saw the data, so no
transcript content left the receiver — a direct ClickHouse read of the
resulting `signoz_logs` rows shows only `loom.runtime`/`loom.session_id`
attributes and an empty `body`, matching the design in `config.yaml`, not
leaked prompt/tool content. Second, the collector container itself also went
into a `docker compose restart` loop (exit 137) under the load, consistent
with hitting its 512 MiB `mem_limit`. Filed as
[#8903](https://github.com/rjwalters/loom/issues/8903): the fan-out's
per-request status conflates independent destinations, there is no
documented way to reset one exporter's persisted queue without the other's,
and the `file_log/*` receivers have no volume safeguard against a busy real
session directory — none of this is SigNoz-specific; it is the shared
collector built in #8526.

The rest of this session's deployment used `LOOM_CLAUDE_PROJECTS_DIR`/
`LOOM_PI_SESSIONS_DIR`/`LOOM_CODEX_SESSIONS_DIR` pointed at empty scratch
directories instead, and the fresh state directory above, before the fixture
run reported earlier in this section. As with every prior session, the
complete trial project (including the gateway) was torn down with
`down --volumes` / `down` at the end, and the host's unrelated containers
were never touched.

## CI retro queries, executed live (2026-09-25, #8826)

**Data source: live capture, not the fixture manifest.** `loom-daemon
ci-telemetry --once` (0.19.375) polled the real `2amlogic` org into a scratch
workspace. Its journal went through `telemetry-export` into a scratch gateway
container running `defaults/observability/collector/config.yaml` byte-for-byte
(verified with `diff`), then into the running trial. The trial is the
2026-09-22 deployment's persistent volumes, restarted 2026-09-25 17:48 UTC.
Captured window: `ci.run`/`ci.job` completions from 2026-09-24 17:58 to
2026-09-25 17:47 UTC, **592 runs and 2,131 jobs across 19 repositories**.

`ci-queries.sql` ran in one pass through the bundled `clickhouse-client`, with
`--param_since='2026-09-18 00:00:00' --param_repo='' --param_bucket_hours=24
--param_window_hours=12 --param_top=10`. The 12-hour window is the largest that
gives section 2 two full windows over about 24 hours of data. It exited 0, and
every section returned rows:

| § | Rows | Sanity check against independent data |
|---|---|---|
| 0 | 10 series / 2 kinds | Each histogram lands as five series (`.bucket`, `.count`, `.sum`, `.min`, `.max`): 33 run and 97 job label sets. Records: 592 `ci.run`, 2,131 `ci.job` |
| 0b | 2 | **Metric points equal distinct records exactly: 592 = 592 runs, 2,131 = 2,131 jobs** |
| 1 | 127 | Per-job daily P50/P95/max, e.g. `gf180-bandgap` "T1 signoff re-grade (klt)" steady at 19–21 s |
| 2 | 10 | Top regression: `klayout-tools` "Tests (Python 3.11)", P95 294 s → 513 s (+219 s, +74.5 %), 15 prior / 20 current jobs |
| 3 | 45 | Totals **535 success / 55 failure / 2 cancelled / 0 unreported = 592**, identical per conclusion to the `ci.run` records' own `loom.ci.conclusion` |
| 4 | 10 | Longest job: `gf180-sar-adc` nightly "sim/selftest.sh stages 2-4 (real PDK)", 822 s, with run/job ids |
| 5 | 60 | 55 failed runs joined to their non-successful jobs, each with its `logs_explorer_filter`. Every row shows `chunks_present`/`chunk_count` **0 of 0**, correctly, because no `ci.job.log` record reached the trial (see below) |
| 6 | 10 | Slowest run: the same nightly, 827 s wall-clock, with that 822 s job making `longest_share` 0.99. `klayout-tools` CI runs show 17 jobs, 1.3–1.5 ks summed job time and a 0.75–0.95 share, so parallelism hides most of the job time |

**Section 5's log join is not proven against live chunks.** Log capture was
on (`logCaptureEnabled: true`), but no cycle in this session reached its
log-download stage. Two bounded `--once` cycles, each killed by a 15-minute
wall-clock `timeout`, spent all their time recording the org's run backlog.
The ledger ended at 1,349 runs and 4,037 jobs, all 4,037 job logs pending, 0
downloaded. The `job_logs` CTE's keys and map columns (`loom.ci.job_id`,
`loom.ci.chunk_index`, `loom.ci.chunk_count`, `loom.ci.truncated`) are checked
against the daemon's `CiJobLogRecord` rendering and the log `keep_keys` by the
drift guard below. Its non-zero path has not been observed on real chunks.

**A defect this run found and fixed before commit.** The first draft of
sections 1–3 de-duplicated the metric samples on `(fingerprint, unix_milli,
value)`, to absorb at-least-once redelivery. Against live data that returned
578 runs and 2,081–2,091 jobs, not 592 and 2,131. Raw `samples_v4` rows equal
the distinct record counts exactly, so no sample in this data was a
redelivery. All 14 missing runs belonged to groups of records sharing
`(repo, workflow, conclusion, completed second)`. A metric point carries no run
identity and durations are whole seconds, so two real runs finishing in the
same second are identical samples. The de-dup silently dropped 2.4 % of runs.
It also moved section 2's ranking: `klayout-tools` "Tests (Python 3.12)" read
+5 s with the de-dup and +45 s without it. Sections 1–3 now count every stored
sample, which is also what the SigNoz UI counts. Section 0b was added so a
redelivered batch shows up as metric points exceeding records, instead of being
either absorbed or silently corrected.

**Drift guard.** `loom-daemon/tests/signoz_trial_artifacts.rs` re-derives the
CI vocabulary from the gateway's per-context `keep_keys` (`log` and `datapoint`
separately) and from the daemon's own record rendering, including the SigNoz
map column each key's OTLP type lands in. It fails on any mismatch.

### Retention DDL observed (2026-09-25)

`retention.sql` (the 7-day logs/traces, 30-day metrics version) was re-run
against this trial, and all 16 `ON CLUSTER` statements reported status 0. The
effective local-table DDL from the README's check query, with Distributed
tables excluded:

| Database | Tables | Effective TTL |
|---|---|---|
| `signoz_metrics` | `samples_v4`, `samples_v4_agg_5m`/`_30m`, `time_series_v4`, `time_series_v4_6hrs`/`_1day`/`_1week`, `exp_hist` | `toIntervalSecond(2592000)` = **30 days** |
| `signoz_metrics` | `metadata`, `samples_v2`, 6 × `samples_v4_reduced_*`, `time_series_v4_reduced`, `time_series_v4_reduced_1day` | `toIntervalDay(30)` = **30 days** (`retention.sql`) |
| `signoz_metrics` | `samples_v4_buffer`, `time_series_v4_buffer` / `usage` | 25 h / 3 days — upstream buffer and accounting tables, not signal retention |
| `signoz_logs`, `signoz_traces` | `tag_attributes_v2` (both), `signoz_spans`, `signoz_index_v2`, `durationSort`, `top_level_operations` | **7 days** (`retention.sql`) |
| `signoz_logs` | `logs_v2`, `logs_v2_resource` | `toIntervalDay(_retention_days)`. The column default and every stored row (2,765) are **15** |
| `signoz_traces` | `signoz_index_v3`, `trace_summary`, `signoz_error_index_v2`, `dependency_graph_minutes_v2`, `usage_explorer`, `traces_v3_resource` | `toIntervalSecond(1296000)` = **15 days** |
| `signoz_logs`, `signoz_traces` | `logs_attribute_keys`, `logs_resource_keys`, `span_attributes_keys` | 15 days |

**Metrics ≥ 30 days is verified in effective DDL.** Every metric signal table
is at 30 days.

**Logs and traces are *not* at 7 days on this trial, and this is not claimed
as done.** The tables still at 15 days are the ones the SigNoz retention API
owns: the API changes `_retention_days` and the active trace tables. This
session could not apply that step. The org's only account (`trial@example.com`,
registered 2026-09-22) has no persisted password in the private state
directory, and the org has no API key. Resetting the password by writing to
the metastore would mean editing an auth store to get around its login, so it
was not done. The six CI saved views were not created for the same reason.
Both remain open in [#8946](https://github.com/rjwalters/loom/issues/8946),
which also asks the API step to confirm whether the three `*_keys` tables need
a `retention.sql` statement.

## Backup-restore rehearsal overlay (2026-09-28)

"Restart and recovery" above is explicit that what it verified was a normal
stop/start, **not** a volume-loss backup restoration — so the README's closing
instruction to *"test restoration into a separate project/network"* has been
unrehearsed advice for the whole trial. `restore-override.yaml` and the README's
"Backup-restore rehearsal" section are that separate project, made reviewable.

**Executed here: the merged render only.** From
`defaults/observability/signoz/`, on Docker Desktop 29.8.0 / Compose v5.5.1 with
a throwaway placeholder env file outside the checkout:

```console
docker compose --env-file <private> -f pours/deployment/compose.yaml -f restore-override.yaml config
```

It renders, and every isolation property the procedure depends on is present in
the output rather than merely intended: project `loom-signoz-restore`; all four
volumes at `loom-signoz-restore-*` names; all six pinned container names at
`loom-signoz-restore-*`; `loom-signoz-network` → `loom-signoz-restore-network`;
the external `loom-observability` gateway network → an `internal: true` bridge
named `loom-signoz-restore-quarantine`; the ingester at `deploy.replicas: 0`; and
exactly one published port, `127.0.0.1:18091:8080`, the live `18081` binding
having been replaced rather than appended (that replacement is what the
`!override` tag buys, and it needs Compose ≥ 2.24.4).

Rendering the procedure — rather than reading it — is what found three defects in
its first draft, each of which would have surfaced on the trial host as a failed
or dangerous rehearsal:

| Defect | Consequence if it had shipped |
| --- | --- |
| `loom-signoz-metastore-postgres-0` kept its live `container_name` | The rehearsal collides with the running trial's PostgreSQL container by name |
| Overlay passed as `-f ../../restore-override.yaml` | Fails outright from the documented working directory: `-f` resolves against the CWD, not the first compose file's directory |
| `exec -T loom-signoz-restore-telemetrystore-clickhouse-0-0` | `exec` addresses a **service**; the overlay renames containers, not services. The same line minus `-f restore-override.yaml` reads the **live** ClickHouse |

All three are now enforced statically by
`loom-daemon/tests/signoz_restore_contract.rs` (13 tests, no Docker/network/
credential), which re-derives the volume set, the pinned container names, the
external-network key and the published-port list from the *rendered* compose — so
a later `foundryctl forge` re-render that adds a fifth volume or a second port
cannot silently escape the overlay. Each of the three was confirmed to fail the
intended test and only that test, by reintroducing the defect and re-running.

**Not executed: the rehearsal itself.** No backup tarball, trial-host volume or
private env file is reachable from a worktree, and the trial project is stopped
with volumes preserved on a host that an earlier session recorded as unable to
hold two stacks at once. Nothing above is presented as an observation of restored
data: the snapshot, restore, cross-project query diff, account/dashboard login and
teardown steps are all unrun. [#9279](https://github.com/rjwalters/loom/issues/9279)
owns that run and flips the ledger row below when it happens.

## Backup-restore rehearsal: found executed, interrupted, and torn down (2026-09-30)

The section above is accurate for the worktree it was written from. It is not
accurate for the trial host as a whole: this pass found a `loom-signoz-restore`
Compose project already running, five days old. Its containers' own
`com.docker.compose.project.config_files` labels point at
`/tmp/loom8528/deploy/compose.yaml` plus `/tmp/loom8528/restore-override.yaml`
— a worktree that no longer exists — rendered by Compose **v2.40.3**, a plugin
this host does not have today (`docker: unknown command: docker compose`; no
`~/.docker/cli-plugins/docker-compose`, no standalone `docker-compose`). Some
earlier `#8528` session ran the real rehearsal from a worktree that carried its
own Compose plugin, then the worktree and session ended without the run being
written up or torn down. [#9279](https://github.com/rjwalters/loom/issues/9279)
had no worktree and no running process attached to it at discovery, so this was
not another sweep's in-progress work.

**What actually restored.** `loom-signoz-restore-metastore-postgres-0` and
`loom-signoz-restore-telemetrystore-clickhouse-0-0` both came up healthy and had
been running for 5 days (created/started 2026-09-25T16:28:5{2,8}Z) when found.
Their data predates that start, which is only possible if it arrived via a
volume-level restore rather than being seeded fresh in place:

- PostgreSQL's `migration` table records `migrated_at` at **2026-09-25
  15:52:41** — 36 minutes before this container's own creation. The restored
  `organizations` table holds exactly one row, named `loom-8528`, created
  `2026-09-25 15:52:52.896024`, with one matching user
  (`trial8528@example.invalid`).
- ClickHouse's `signoz_traces.signoz_index_v3` holds 37 rows spanning
  `2026-09-25 16:06:43`–`16:07:23`, every one `resource_string_service$$name =
  loom-daemon`: `loom.phase`×14, `loom.role_attempt`×12, `loom.sweep`×7,
  `loom.runtime.preflight`×2, `loom.tool`×1, `loom.runtime.run`×1 — a real
  sweep's own spans, not a synthetic fixture row.

Both tables' data is consistent with the README's documented tar-per-volume
procedure (the pinned `postgres:16` tarballer, `--numeric-owner`) and with
nothing else: no in-place seeding explains rows timestamped before the
container holding them existed.

**What did not finish.** `loom-signoz-restore-signoz-0` and
`loom-signoz-restore-telemetrystore-migrator` were both still `Created` —
compose never started them. The README's restore step brings up
`loom-signoz-signoz-0` specifically to pull in that whole dependency chain
(metastore, keeper, ClickHouse, migrator, user-scripts); here it stopped after
the datastore layer and the one-shot user-scripts container came up, and never
reached the app or the migrator. That means no browser/UI login happened, no
`fixture-queries.sql` cross-project diff was run or captured, and the
documented teardown (`down --volumes --dry-run`, then for real) never ran
either. The rehearsal was interrupted partway, not completed.

**Cost of leaving it running.** `docker stats` at discovery showed
`loom-signoz-restore-telemetrystore-clickhouse-0-0` at **124.52% CPU**, and
`system.part_log` showed 127,377 failed `metric_log` background merges against
477 successful ones — the same unpatched `MEMORY_LIMIT_EXCEEDED`
self-telemetry failure already characterized in "Co-resident 2.5-day soak"
below, not a new finding, just that same bug reproducing unattended on a
second project for five days on a shared 8-vCPU dispatch host.

**Disposition.** The genuinely new information above — that a volume-level
tarball round-trip preserves both PostgreSQL's org/user metadata and
ClickHouse's trace data — is now captured here. Nothing else in the abandoned
project was worth keeping: this pass tore the `loom-signoz-restore-*` project
down with plain `docker rm -f`/`network rm`/`volume rm` restricted to that
exact name prefix (the `docker compose` plugin that created it is no longer on
this host, so the README's own `down --volumes --dry-run` step could not be
run as documented) and filed a follow-up issue covering the missing `docker
compose` plugin (it blocks anyone, including a future #9279 session, from
running the documented procedure on this host at all) and the fact that a
datastore-layer restore ran to completion outside of any tracked session. The
acceptance-ledger row below stays **Open**: login verification, the query
diff and a clean, documented teardown are all still unexecuted, and #9279
still owns deciding whether this partial run is sufficient evidence for the
datastore layer or should be repeated end-to-end.

## Co-resident 2.5-day soak: footprint, and a self-telemetry merge failure (2026-09-28)

**What was observed.** A long-running `loom-signoz` project on the primary
dispatch host, measured read-only: `docker stats`/`inspect`/`system df` and
`SELECT`s against ClickHouse `system.*` tables. Nothing was restarted or
re-rendered, and no telemetry was sent. The host is Docker Desktop 29.8.0 on a
Linux aarch64 VM with 24 CPUs and 46.96 GiB. The project started at
2026-09-25T17:48Z and ran all five `casting.yaml` digests exactly, with 0
restarts and no OOM kill. It ran next to `loom-clickstack` (up since
2026-09-24T19:06Z), the neutral gateway and a scratch CI gateway (#8826).

The earlier sessions above could not keep both backends healthy on an 8 GB VM.
Here they coexisted for more than two days. That is a statement about host
capacity, not a product comparison.

**Footprint.** These are three `docker stats` samples taken 2 s apart from
06:41:21Z, before any probe below. `docker stats` memory includes page cache.

| Service | CPU | Memory / `mem_limit` | Block I/O read / write since start |
| --- | --- | --- | --- |
| ClickHouse | **55–123 %** | 1.65–1.86 GiB / 2 GiB | 369 GB / 92.6 GB |
| Ingester | 0.04 % | 234 MiB / 512 MiB | 34.9 GB / 29.4 MB |
| SigNoz app | 0.01 % | 139 MiB / 768 MiB | 54.6 GB / 22.9 MB |
| Keeper | 0.7–1.0 % | 74 MiB / 256 MiB | 849 GB / 6.1 GB |
| PostgreSQL | 0.00 % | 67 MiB / 256 MiB | 5.58 GB / 40.4 MB |
| *ClickStack, for reference* | 4–44 % | 2.49 GiB / 3 GiB | 618 GB / 150 GB |
| *Neutral gateway* | 0.01 % | 45 MiB / 512 MiB | 3.47 GB / 2.47 MB |

ClickHouse's own accounting is lower than the `docker stats` figure: cgroup
memory used 1.02 GiB, resident 1.03 GiB, jemalloc resident 1.45 GiB, and a
server cap (`max_server_memory_usage`) of 1.80 GiB, which is 0.9 of the 2 GiB
cgroup. The volumes held: ClickHouse data 586.6 MB, PostgreSQL 68.35 MB, Keeper
22.82 MB and user scripts 1.5 MB. ClickStack's volumes held 784.4 MB of
telemetry and 340.5 MB of metadata.

**Self-telemetry dominates the disk.** Inside ClickHouse, active parts in
`system` took **482 MiB**. `trace_log` alone was 340 MiB, `metric_log` 83 MiB
and `text_log` 29 MiB. Every Loom signal database together took about
**2.5 MiB**: metrics 1.47 MiB, metadata 0.36 MiB, logs 0.36 MiB and traces
0.36 MiB. ClickStack shows the same shape, with 532.6 MiB of `system` against
6.29 MiB of `default`. For both backends, at this trial's Loom volume, disk
sizing is a question about ClickHouse's own logs, not about Loom's.

**Query and insert latency.** This comes from `system.query_log`, which keeps
only the current day under its 1-day TTL. Between 00:00Z and 06:42Z there were
326 initial `SELECT`s, mostly the app's own metadata queries, at p50 5 ms, p95
101 ms and max 5,829 ms. The 2,212 inserts ran at p50 4 ms and p95 45 ms. These
were measured under the memory pressure described next, and they are not a
benchmark.

### `system.metric_log` stopped expiring under the rendered 2 GiB cap

ClickHouse spent the CPU above retrying one background merge on its own
**`system.metric_log`**. That table is 1,552 columns wide by default, and it
touches no Loom table. `system.part_log` recorded the retries, and the failure
rate grew every hour:

| Hour (UTC, 2026-09-28) | 00 | 01 | 02 | 03 | 04 | 05 | 06 (partial) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Failed `metric_log` merges | 3,398 | 3,583 | 4,492 | 5,112 | 5,604 | 7,723 | 7,136 |
| Successful `metric_log` merges | 24 | 24 | 22 | 23 | 23 | 23 | 17 |

Each failure is `MEMORY_LIMIT_EXCEEDED`: the merge "would use 1.41 GiB" against
1.80 GiB of RSS. The other system tables show 1 to 4 failures in the same
window. The cumulative `QueryMemoryLimitExceeded` event counter stood at
570,199.

The consequence is retention, not only CPU. ClickHouse applies a TTL during
merges, and the merge that failed is the one spanning the table's roughly 54
parts. The ClickHouse data volume predates this container: it was created
2026-09-22T09:13Z by an earlier start of the same project, and the container
was recreated on 2026-09-25. The oldest active `metric_log` parts hold
2026-09-22 rows, modified at **2026-09-22 18:56** and 19:26 UTC. At 07:30Z on
2026-09-28 they were more than 5 days into a **1-day** TTL. Memory pressure from the retry loop also killed an
ordinary read-only diagnostic `GROUP BY` on `system.trace_log` during this
pass (`OvercommitTracker`).

ClickStack does not show the failure. Its bundled ClickHouse is 26.8.7 under a
3 GiB cap, and in the same window its `metric_log` recorded 9,516 successful
and 2 failed merges, 7 active parts, with the oldest part at 2026-09-27 09:54.
This is a sizing defect of the SigNoz render, not a finding about ClickHouse
in general.

**Reproduced on the pinned image, off the live stack.** The same
`clickhouse-server:25.12.5@sha256:cacf32d6…` image ran in throwaway,
volume-less containers under the same 2 GiB cap. Merges were held while
one-second flushes accumulated parts, then one `OPTIMIZE … FINAL` merged 100
inputs:

| Schema | Rows merged | Inputs | Peak merge memory |
| --- | --- | --- | --- |
| Upstream wide `metric_log` (1,552 columns) | 144 | 100 | **749.65 MiB** |
| `transposed_with_wide_view` (6 columns) | 278,640 | 100 | **38.74 MiB** |

Wide-table merge memory tracks inputs × columns, almost independent of rows.
750 MiB on top of the live server's roughly 1 GiB baseline breaks the 1.80 GiB
cap, which is the failure observed live. With few inputs (65 rows, 2 parts)
the same wide table peaked at 52 MiB, which explains why the small merges
succeed.

**Remedy (this increment).** `casting.yaml` gains a declarative patch on the
rendered ClickHouse config. It `test`s the upstream `metric_log` TTL and then
adds `schema_type: transposed_with_wide_view`. The pinned Foundry v0.2.17
darwin/arm64 archive was checksum-verified. The unmodified casting reproduced
the committed `pours/` byte for byte before the edit. The re-render changed
exactly one line of `config-0-0.yaml` plus the lock, and a second render was
identical. Changing the `test` value made `forge` exit 5 with no output, so an
upstream TTL change cannot slip past the patch unreviewed.

Four checks ran on the pinned image. `docker compose … config --quiet` accepts
the render. The rendered `config-0-0.yaml` itself starts ClickHouse. It
produces `system.metric_log` as a `SystemMetricLogView` backed by
`transposed_metric_log`, which keeps `TTL event_date + toIntervalDay(1)`.
Restarting a volume that already held the wide table renamed that table to
`metric_log_0` and kept its data. The README therefore documents a
force-recreate of the ClickHouse service followed by `DROP TABLE
system.metric_log_0`. The test
`signoz_deployment_contract::clickhouse_self_telemetry_stays_expirable_under_the_rendered_memory_cap`
enforces the override, a `DELETE` TTL on every rendered `system.*_log`, and
that README step.

**Re-verified at commit time (07:30Z).** The live stack still showed the
failure. In the preceding hour it had 9,318 failed `metric_log` merges against
22 successful ones, 54 active parts and 1,552 columns. On the same pinned
Foundry archive, the committed casting still re-rendered the committed
`pours/` and lock byte for byte, and the patched render again differed by the
single `schema_type` line and was deterministic. A tampered `test` value
again made `forge` exit 5. A throwaway pinned-image container, given the
rendered `metric_log` block (TTL plus `schema_type`), created
`system.metric_log` as a `SystemMetricLogView` over a 6-column
`transposed_metric_log` with `TTL event_date + toIntervalDay(1)`. The
merge-memory reproduction in the table above was not re-run.

**Not done here.** The live project was **not** re-rendered or restarted. It
is a shared deployment that is still running, and applying the change is the
README's documented operator step. Its effect over a multi-day soak therefore
remains unobserved on a live stack. After applying it, the README's
`part_log` failed-merge query is the check that should return no rows.

## Measured usage: the ClickStack parity gap, and how it was closed

ClickStack's README has carried a **Loom measured usage** saved view since its
trial landed. The SigNoz side had no counterpart at all: `fixture-queries.sql` 7
asks the absence-vs-measured-zero question of the `loom.tokens.*` **gauges**
(subscription utilization), and nothing asked it of the `loom.runtime.usage`
**spans** (#8908, #9204, #9303) — the one signal family that carries tokens and
dollars. The two products were therefore not being asked the same question about
cost, which is a scope-item-4 parity gap, not a missing nicety.

`usage-queries.sql` closes it: sections 0–6 covering arrival preflight, spend and
tokens by model, the by-repo/role/runtime/model parity view, usage coverage
(unknown vs measured zero), unpriced models, rate-card provenance, and cache
composition. Four properties of the family had to be established before the SQL
could be trusted, and each was read out of the emitting code rather than assumed:

- **Every span attribute is exported as a string.** `otlp/traces.rs` renders the
  whole attribute map with `kv_string`, with no numeric branch, so the token
  counts and the USD estimate land in `attributes_string` — the opposite of the
  CI *log* records, where `loom.ci.run_id` genuinely is an int. A reader moving
  between `ci-queries.sql` and this file crosses that boundary.
- **`loom.repo` is never on a usage span.** No caller of `model_usage_spans`
  puts it in `common` (checked across `runtime_usage.rs`, `runtime_usage/record.rs`
  and `role_tick_telemetry/usage.rs`), and the gateway's resource allowlist keeps
  only `service.name` / `service.version` / `service.instance.id` / `host.id`.
  Repo attribution is a join across the trace, or it is nothing.
- **Scope is not additive.** A daemon-dispatched sweep emits both an `execution`
  span and `attempt` spans over overlapping windows.
- **An unpriced model carries no cost attributes at all**, by the deliberate
  `Option`-returning lookup in `runtime_usage/cost.rs` — so `sum()` skips it and
  a spend total is a lower bound unless the query says otherwise.

**Executed, not asserted.** `loom-daemon/tests/signoz_usage_queries.rs` runs the
committed file verbatim — whole file in one pass, then statement by statement —
in the pinned ClickHouse 25.12.5 the telemetry store itself runs
(`clickhouse local`, no server, no volume), over a fixture built so each trap
fails loudly. Observed on the pinned engine:

| Property | Observation |
| --- | --- |
| At-least-once delivery | the execution span delivered twice collapses to **1** in every section |
| Scope resolution | the both-scopes sweep totals **165** tokens, not the 330 a naive sum reports |
| Unpriced model | tokens **999**, dollars **NULL** (never `0.0`), `unpriced_spans` **1** |
| Unknown vs measured zero | Judge attempt `usage_unknown` **1**; Doctor attempt `measured_zero_only` **1**; never merged |
| Repo by trace join | `org/alpha` / `org/beta` resolved from the root span, `repos_in_trace` **1** |
| Wrong container (negative control) | `attributes_number` subscript returns **0** for every row **without erroring** — the silent-empty failure mode, demonstrated rather than claimed |

The static half (`signoz_trial_artifacts.rs`, 5 new tests) derives the permitted
counter/cost/pricing vocabulary by calling `counter_attributes()` and
`Pricing::attributes()`, so a rename in the emitting code fails ordinary CI
instead of silently emptying a saved view.

**Not done here.** No run against a live SigNoz over real canary data: the
fixture is synthetic and the engine is `clickhouse local`, which establishes the
SQL's behaviour against the pinned ClickHouse but not the trial deployment's
ingest path. That needs the trial host, the same gap #8525 and #9279 name.

## Closing the drift guard's last gap: queue-dwell, quota-utilization, the alert (2026-09-30)

`fixture-queries.sql`, `ci-queries.sql` and `usage-queries.sql` all gained a
static CI contract as they landed — a saved query that subscripts an attribute
the gateway strips, or names a metric no emitter produces, returns zero rows
forever rather than failing, which is indistinguishable from "the backend lost
the data". `queue-dwell.sql` (#8856), `quota-utilization.sql` (#9005) and
`alerts/queue-starvation.json` (#8856) — all scope-item-4 "host/token gauges"
artifacts in this same directory — had no such guard at all: a rename of
`loom.queue.starved`, `loom.tokens.usage_fraction_weekly` or any of their
`state`/`reason`/`provider`/`account` labels would have gone unnoticed by
ordinary CI.

Three new tests in `signoz_trial_artifacts.rs` close the gap, each derived
rather than restated: `queue_dwell_queries_match_the_ops_metric_vocabulary`
against the public `MetricName` enum (`loom-daemon/src/telemetry/ops.rs`);
`quota_utilization_queries_match_the_tokens_snapshot_vocabulary` against the
`TokensSnapshot` OTLP mapping arm's own literal metric names, parsed out of
`mapping.rs` the same way `sweep_facts_gateway_survival.rs` (#9586) already
treats that source file as an authority; and
`queue_starvation_alert_matches_the_ops_metric_vocabulary` against the alert
JSON's embedded `query` string. `queue-dwell.sql` was also added to the
existing `saved_queries_only_reference_forwarded_attribute_and_resource_keys`
file list, covering query 5's span-attribute/resource reads.

Each of the three was confirmed to fail for its own reason before being
committed: a typo'd `loom.queue.starved` in `queue-dwell.sql`, a typo'd
`loom.tokens.exhausted` in `quota-utilization.sql`, and a typo'd
`loom.queue.starved` inside the alert's embedded query each produced the
intended panic message naming the exact stale/unknown literal. No Docker,
network, backend or credential is used, so this runs in ordinary CI on any
host — including this one, which has neither Docker Compose nor a running
trial to observe.

## Acceptance ledger

| Check | Status |
| --- | --- |
| Pinned Foundry render and configuration | Passed, including deterministic second render (reconfirmed independently above); digest pinning, casting/render agreement and the README version table are now CI-enforced |
| Architecture support (arm64 **and** amd64) | **Passed** — arm64 on Docker Desktop above; amd64 executed live on native Linux, all five index digests resolving to `amd64/linux` and the histogram helper's amd64 branch running its checksum-before-extract path |
| Private receiver exposure and project isolation | Passed on the trial host, continuously enforced against the committed render (see "Rendered-deployment contract"), and additionally probed live on amd64 — the host's non-loopback address refuses the UI, and no other port is published |
| Keeper, PostgreSQL and ClickHouse readiness | Passed on the trial VM, and on native Linux/amd64 in 67s cold / 30s restart |
| Schema migrations and app readiness | Passed; receiver storage proof remains separate. **Compose-healthy is not ingestion-ready** — the receiver stays closed until an organization is registered; see "Compose readiness is not ingestion readiness" |
| Backend outage and recovery through the shared collector | **Passed** — the ingester's receiver was genuinely unavailable until org registration; the gateway's queue preserved and drained all 37/14/3 signals with zero loss and no re-send, while the absent ClickStack exporter stayed visibly stuck in the same scrape |
| Three fixture signals with matching IDs/values | Passed for the ad-hoc probe; metric timestamp precision conversion documented |
| Actual Trace Explorer and correlated logs | Passed in authenticated UI; sanitized screenshots linked above |
| Seven-day effective retention | API, overrides and actual DDL verified; metadata/grace exceptions documented |
| Restart persistence and shared receiver recovery | Passed for signals, account and effective TTL; fresh three-signal replay indexed |
| Backup restoration rehearsed into a separate project | **Open** — the isolated `loom-signoz-restore` overlay, its documented procedure and a 13-test static isolation contract all landed and the merged render was verified (see "Backup-restore rehearsal overlay"). A real tarball restore *was* found executed on this trial host by an earlier, undocumented session (see "Backup-restore rehearsal: found executed, interrupted, and torn down"): both PostgreSQL's org/user metadata and 37 real ClickHouse trace spans survived the volume round-trip. But the run stopped at the datastore layer — no UI login, no `fixture-queries.sql` diff, no documented teardown — and sat abandoned, burning CPU, for 5 days before this pass found and tore it down (follow-up: [#9762](https://github.com/rjwalters/loom/issues/9762)); [#9279](https://github.com/rjwalters/loom/issues/9279) still owns a complete, documented pass |
| Saved query artifacts for the shared fixture manifest | **Passed** — executed live above; matches the generated manifest exactly |
| Shared fixture manifest observed in SigNoz | **Passed** — see "Shared fixture manifest, executed live" above: 37/14/3 signals, exact totals, graph, grouping, root-less detection, absence-vs-zero and privacy-sentinel queries all verified |
| Real Loom canary / real Judge-Doctor repair trace | Open — the instrumentation slices landed (#8577/#8579), but #8525 itself stays open for its own live-run acceptance, and the run needs the trial host; see #8529 |
| Repeated latency/footprint comparison | Open — shared evaluation #8529. A single **co-resident** point-in-time footprint (CPU, memory, volume and per-database disk for both backends) and a same-day query/insert latency distribution are now recorded (see "Co-resident 2.5-day soak"), but no controlled, repeated, same-workload comparison has been run |
| ClickHouse self-telemetry expires under the rendered 2 GiB cap | **Fixed in the render, not yet observed on a live soak.** The wide upstream `metric_log` failed tens of thousands of TTL merges a day, and its parts outlived their 1-day TTL. The transposed schema, reproduced at about 19x lower peak merge memory on the pinned image, is CI-enforced. A live re-render plus a multi-day failed-merge check is outstanding |
| CI retro queries (`ci-queries.sql`, #8826) | **Passed on live capture** — every section non-empty; metric-path counts and conclusion split reconcile exactly to the records (592 runs / 2,131 jobs); see "CI retro queries, executed live". Section 5's chunk join is unobserved on real `ci.job.log` data (none reached the trial) |
| CI metrics retention ≥ 30 days (#8826) | **Passed in effective DDL** — every metric signal table at 30 days |
| CI logs/traces at 7 days on the current trial | **Open** — API-owned tables still at the upstream 15 days; needs the org login ([#8946](https://github.com/rjwalters/loom/issues/8946)) |
| Six CI saved views in the trial org | **Open** — recreation steps written in the README, not yet executed in the UI ([#8946](https://github.com/rjwalters/loom/issues/8946)) |
| Measured usage parity with ClickStack's "Loom measured usage" view | **Passed against the pinned ClickHouse, open against the live trial.** `usage-queries.sql` (sections 0–6) plus four README saved-view rows close the parity gap; `signoz_usage_queries.rs` executes the committed file verbatim on ClickHouse 25.12.5 and observes scope resolution, at-least-once dedupe, NULL-not-zero dollars for an unpriced model, unknown-vs-measured-zero, the repo-by-trace join, and the wrong-container silent zero (see "Measured usage" above). No run over real canary data on the trial deployment — same gap as [#8525](https://github.com/rjwalters/loom/issues/8525) |
| UI view matrix for Loom's non-HTTP span kinds | **Answered for Service List/APM and Exceptions, via authenticated backend API probes rather than a browser** (see "UI view matrix" above) — Service List/APM populates (`loom-daemon`, correct call/error counts) because RED metrics come from unconditional root-span aggregation, not an HTTP convention; Exceptions stays empty because Loom never emits an OTel `exception` span event. Service Map returned empty and, since 2026-09-30, **is attributable** — see "Resolving the Service Map confound": every Loom span is `SPAN_KIND_INTERNAL` and every resource carries the one `service.name`, both at single unconditional exporter sites and both measured on the real wire payload, so none of the three preconditions for a topology edge can be met; the gateway's allowlist strips every peer key as a second layer. Adding a connector or a multi-service fixture cannot change the answer for Loom's data. Enforced by `signoz_topology_shape.rs` in ordinary CI. No screenshot has been captured on any session |

Synthetic fixture success establishes transport/schema/query behavior, not a
real Loom lifecycle. Keep #8528 open until the real-canary and #8529
comparison rows are recorded.
