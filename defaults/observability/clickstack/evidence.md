# ClickStack trial evidence

## Reproduction identity

Validation on 2026-09-21 used the Compose-pinned ClickStack all-in-one 2.39.1
multiarch digest on Docker Desktop Linux arm64, with 8 VM CPUs and 7.65 GiB RAM.
Existing unrelated workloads were left running. Queried bundled versions:
ClickHouse **26.8.7.19**, HyperDX **2.39.1**, collector **0.155.0**, MongoDB **4.0.5**.
This is an upstream local-testing distribution, not a production recommendation.

The first cold start exposed the bundled supervisor's three-second bootstrap
limit: the app could start while its collector had exited. The committed
120-second supervisor deadline fixed that failure. Under substantial unrelated
CPU load the application took roughly **14 minutes** to become ready. Five-second
health probes also timed out despite eventual successful requests, so the final
recipe permits 30-second probes and a 15-minute startup grace period. These are
host-specific observations, not a backend benchmark.

## Stored fixture and authentication

The fixture was sent through the neutral gateway, not directly into ClickHouse.
It uses `service.name=loom-trial-fixture`, trace ID
`85270000000000000000000000000001`, timestamp `1790026033610216000` ns for the log
and gauge, and no private workload content.

| Stored result | Observed value |
| --- | --- |
| Trace rows / unique spans | 12 / 4 after deliberate resubmissions and queue recovery |
| Root | `0000000000000001`, `loom.sweep`, `Ok` |
| Rejected Judge | `0000000000000002`, parent `0000000000000001`, `Error` |
| Doctor repair | `0000000000000003`, parent `0000000000000001`, `Ok` |
| Successful Judge | `0000000000000004`, parent `0000000000000001`, `Ok` |
| Logs | 3 rows with the same trace ID and rejected-Judge span ID; fixture body preserved |
| Severity | Input `ERROR` normalized by ClickStack to searchable `error` |
| Gauge | 2 rows, `loom.host.synthetic_capacity`, value `3`, unit `{slot}` |
| Optional usage | No usage measurement or fabricated zero emitted |
| Wrong ingestion key | Actual ClickStack receiver returned HTTP 401; only status retained |

The gateway's original direct file-to-header substitution rejected normal
LF-terminated key files before sending any request. The independent review
reproduced this with the real ClickStack trial. The dedicated file-backed
ClickStack authenticator in #8526 fixed it; the same original LF-ended key then
successfully delivered all three signals. This is why synthetic sink tests now
include newline-terminated credentials.

`system.tables.create_table_query` confirmed seven-day TTLs on logs, traces,
gauge/sum/histogram/summary/exponential-histogram tables and the trace-ID helper
table. The associated materialized view has no independent storage/TTL.
A point-in-time sample measured **900.3 MiB** for ClickStack and **67.84 MiB** for
the neutral gateway; active fixture-table parts occupied **142,757 bytes**. These
small-fixture observations do not establish sustained capacity or cost.

## HyperDX evidence and remaining acceptance

First-account registration provisioned Logs, Traces and Metrics sources. The
authenticated Sources API confirmed `TraceId`/`SpanId` mappings, `ParentSpanId`,
and bidirectional log/trace source references. In the browser, selecting Logs,
Last 1 hour and Run showed the three stored fixture logs. Opening a log displayed
its trace/span IDs and a **View Trace** action. Following that action opened the
five-second waterfall with rejected Judge, Doctor and successful Judge spans,
and all three correlated log rows. The selected rejected span showed its exact
parent/span IDs and Error status. The [sanitized screenshot](evidence/trace-waterfall.png)
contains only the synthetic fixture. HyperDX displays the replayed duplicate spans
rather than deduplicating them; compare unique trace/span IDs separately. The UI reported a single-query
elapsed time of **3 seconds**; this is one observation, not a p95 result.

Container recreation with a newly generated bootstrap ingestion key preserved
all 12 trace rows (4 unique spans), 3 fixture logs, both fixture gauge rows at
value 3, the registered UI account, and the original source IDs. Against the
restarted receiver, the previous key returned HTTP **401** and the new key
returned **200** for an empty OTLP request. Both application and collector
readiness passed; Docker marked the container healthy with the revised probe
allowance. The gateway was deliberately stopped during rotation and restarted
against the new file-backed key, preserving its queue volume.
One further three-signal submission through that gateway returned HTTP 200 and
was indexed: totals became 16 trace rows / 4 unique spans, 4 fixture logs, and
3 fixture gauge rows with value 3. This verifies delivery with the rotated key,
in addition to receiver authentication and storage persistence.

The pinned UI also displayed a transient `Expected string, received null` notice
while changing sources; it did not prevent the observed log query/details.

| Remaining check | Status |
| --- | --- |
| Log-to-trace waterfall and correlated log rows | Passed in the actual HyperDX browser UI |
| Bootstrap-key rotation and restart persistence | Passed against the actual receiver and persisted tables/source configuration |
| Real Loom canary and real Judge/Doctor repair trace | Requires #8524/#8525 and #8529 |
| Repeated query and ingest-to-visible latency comparison | Shared evaluation #8529 |

The synthetic repair-shaped trace proves transport and schema only. It is not
evidence that Loom emitted real lifecycle spans. Keep #8527 open until these
remaining checks are recorded. The shared Rust fixture from #8529 is authoritative
for the final side-by-side comparison.
