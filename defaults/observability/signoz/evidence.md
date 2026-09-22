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

**Not executed against a live backend.** The change that added it ran on a sweep
host with no Docker access and the trial deployment stopped, so its column names
follow the pinned v0.142.1 schema and are confirmed by its own query 0. Nothing in
this section is an observation.

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

## Acceptance ledger

| Check | Status |
| --- | --- |
| Pinned Foundry render and configuration | Passed, including deterministic second render |
| Keeper, PostgreSQL and ClickHouse readiness | Passed on the trial VM |
| Schema migrations and app readiness | Passed; receiver storage proof remains separate |
| Three fixture signals with matching IDs/values | Passed for the ad-hoc probe; metric timestamp precision conversion documented |
| Actual Trace Explorer and correlated logs | Passed in authenticated UI; sanitized screenshots linked above |
| Seven-day effective retention | API, overrides and actual DDL verified; metadata/grace exceptions documented |
| Restart persistence and shared receiver recovery | Passed for signals, account and effective TTL; fresh three-signal replay indexed |
| Saved query artifacts for the shared fixture manifest | Written and vocabulary-verified in CI; **not** executed on a backend |
| Shared fixture manifest observed in SigNoz | Open — needs the trial host; see #8529 |
| Real Loom canary / real Judge-Doctor repair trace | Open — the instrumentation slices landed (#8577/#8579), but #8525 itself stays open for its own live-run acceptance, and the run needs the trial host; see #8529 |
| Repeated latency/footprint comparison | Open — shared evaluation #8529, and the trial host could not hold both backends up at once |

Synthetic fixture success will establish transport/schema behavior only. It
cannot substitute for a real Loom lifecycle or independent correctness judgment.
Keep #8528 open until the remaining criteria are recorded.
