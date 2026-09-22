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
Every mutation failed exactly the intended test with an accurate message, and none
of the eight is vacuous. All mutations were reverted; the committed render is
unchanged by this section.

This closes the "verified once, by hand, unguarded afterwards" gap for the
deployment's trust boundary and supply chain. It is a static contract and
deliberately asserts nothing about a running backend: readiness, ingestion,
retention and query results remain the live sections above.

**Not run on this sweep host**: `docker` returns `permission denied while trying to
connect to the docker API at unix:///var/run/docker.sock`, and no trial volumes
exist here, so nothing in this section is a live observation and no live check was
repeated. That is also why the two open ledger rows below did not advance.

## Acceptance ledger

| Check | Status |
| --- | --- |
| Pinned Foundry render and configuration | Passed, including deterministic second render (reconfirmed independently above); digest pinning, casting/render agreement and the README version table are now CI-enforced |
| Private receiver exposure and project isolation | Passed on the trial host, and now continuously enforced against the committed render — see "Rendered-deployment contract" |
| Keeper, PostgreSQL and ClickHouse readiness | Passed on the trial VM |
| Schema migrations and app readiness | Passed; receiver storage proof remains separate |
| Three fixture signals with matching IDs/values | Passed for the ad-hoc probe; metric timestamp precision conversion documented |
| Actual Trace Explorer and correlated logs | Passed in authenticated UI; sanitized screenshots linked above |
| Seven-day effective retention | API, overrides and actual DDL verified; metadata/grace exceptions documented |
| Restart persistence and shared receiver recovery | Passed for signals, account and effective TTL; fresh three-signal replay indexed |
| Saved query artifacts for the shared fixture manifest | **Passed** — executed live above; matches the generated manifest exactly |
| Shared fixture manifest observed in SigNoz | **Passed** — see "Shared fixture manifest, executed live" above: 37/14/3 signals, exact totals, graph, grouping, root-less detection, absence-vs-zero and privacy-sentinel queries all verified |
| Real Loom canary / real Judge-Doctor repair trace | Open — the instrumentation slices landed (#8577/#8579), but #8525 itself stays open for its own live-run acceptance, and the run needs the trial host; see #8529 |
| Repeated latency/footprint comparison | Open — shared evaluation #8529; this session did not deploy ClickStack alongside SigNoz, so no simultaneous comparison was attempted |

Synthetic fixture success establishes transport/schema/query behavior, not a
real Loom lifecycle. Keep #8528 open until the real-canary and #8529
comparison rows are recorded.
