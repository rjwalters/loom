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

A point-in-time sample after ingestion measured approximately **1.24 GiB** across
the five steady-state SigNoz containers (ClickHouse 1.03 GiB, app 105.7 MiB,
Keeper 50.87 MiB, PostgreSQL 30.16 MiB and ingester 27.7 MiB). Active parts in the
three signal databases totaled **77,373 bytes**. These small-fixture observations
exclude PostgreSQL, system tables, images and total volume usage; they are not a
capacity or comparative cost benchmark. The host was heavily contended.

## Acceptance ledger

| Check | Status |
| --- | --- |
| Pinned Foundry render and configuration | Passed, including deterministic second render |
| Keeper, PostgreSQL and ClickHouse readiness | Passed on the trial VM |
| Schema migrations and app readiness | Passed; receiver storage proof remains separate |
| Three fixture signals with matching IDs/values | Passed; metric timestamp precision conversion documented |
| Actual Trace Explorer and correlated logs | Passed in authenticated UI; sanitized screenshots linked above |
| Seven-day effective retention | API, overrides and actual DDL verified; metadata/grace exceptions documented |
| Restart persistence and shared receiver recovery | Pending live check |
| Real Loom canary / real Judge-Doctor repair trace | Requires #8524/#8525 and #8529 |
| Repeated latency/footprint comparison | Shared evaluation #8529 |

Synthetic fixture success will establish transport/schema behavior only. It
cannot substitute for a real Loom lifecycle or independent correctness judgment.
Keep #8528 open until the remaining criteria are recorded.
