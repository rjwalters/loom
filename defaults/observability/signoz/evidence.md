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

## Acceptance ledger

| Check | Status |
| --- | --- |
| Pinned Foundry render and configuration | Passed, including deterministic second render |
| Keeper, PostgreSQL and ClickHouse readiness | Passed on the trial VM |
| Schema migrations, app and ingester readiness | In progress |
| Three fixture signals with matching IDs/values | Pending actual query |
| Actual Trace Explorer and correlated logs | Pending browser evidence |
| Seven-day effective retention for all signals | Pending settings change and table DDL verification |
| Restart persistence and shared receiver recovery | Pending live check |
| Real Loom canary / real Judge-Doctor repair trace | Requires #8524/#8525 and #8529 |
| Repeated latency/footprint comparison | Shared evaluation #8529 |

Synthetic fixture success will establish transport/schema behavior only. It
cannot substitute for a real Loom lifecycle or independent correctness judgment.
Keep #8528 open until the remaining criteria are recorded.
