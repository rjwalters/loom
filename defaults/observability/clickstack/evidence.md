# ClickStack trial evidence

## Reproduction identity

- Source issue: #8527, parent #8522.
- Image: ClickStack all-in-one 2.39.1, multiarch index digest recorded in Compose.
- Validation host: Docker Desktop Linux arm64 VM, 8 CPUs / 7.65 GiB total RAM,
  with existing unrelated workloads left running.
- Bundled versions queried during initial boot: ClickHouse 26.8.7.19;
  MongoDB 4.0.5. This is the upstream local-testing image, not a production recipe.
- Proposed retention: 168h; verify each actual table DDL after ingestion.

## Verification ledger

| Check | Result |
| --- | --- |
| Digest resolves to amd64 and arm64 manifests | Passed, registry inspect |
| Compose interpolation with private key | Passed, `config --quiet` |
| Missing key refuses startup configuration | Passed, required-variable error |
| Cold startup at 2 CPU limit | Failed: bundled OpAMP supervisor bootstrap timed out; app startup alone must not be treated as ingestion readiness |
| Collector and application readiness at 4 CPU limit | Pending |
| Synthetic manifest stored with parentage, log IDs and gauge unit | Pending |
| UI log-to-trace and span-to-log navigation | Pending |
| Wrong-key rejection and rotation | Pending |
| Restart persistence | Pending |
| Real Loom canary and repair trace | Requires #8524/#8525 and shared evaluation #8529 |

The synthetic manifest is transport-only: trace
`85270000000000000000000000000001`, four spans (sweep, rejected Judge, successful
Doctor, successful Judge), one ERROR log attached to the rejected Judge, one
`loom.host.synthetic_capacity` gauge with value 3 and unit `{slot}`. All use
`service.name=loom-trial-fixture`; optional usage measurements are absent. No
private repository, prompt or credential content is included. The shared Rust
fixture under #8529 is authoritative for the side-by-side comparison.

## Measurement protocol

Measure receiver-send to first stored query result using a fresh unique trace ID,
then record UI-visible time separately. Query latency needs repeated observations
with sample count and concurrent workload recorded; do not report one request as
p95. Record container CPU/memory and ClickHouse `system.parts.bytes_on_disk` after
the same workload in each backend. `queries.sql` includes duplicates and storage
queries. Collect gateway queue/failure/drop counters at the same time.

Until stored and UI evidence is filled in, this document is an honest deployment
ledger, not a successful backend comparison or proof of real Loom tracing.
