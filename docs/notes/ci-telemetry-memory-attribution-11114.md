# CI telemetry daemon memory attribution (#11114, parent #10420)

Dated 2026-10-09. This is an investigation report. It claims **no remediation**.
Measured on one Linux host only; no macOS comparison was possible.

## Verdict

- The historical symptom (about 2.2 GB baseline plus 2.2 GB for CI telemetry) was **not reproduced** on current code in the workload below. Streamed export (#11045/#11047) holds: a 562 MB, 1.2 M-line journal costs 365 KB of peak live heap and about +0.8 MB daemon RSS.
- The measured incremental cost of enabling CI telemetry plus the observability exporter on an idle daemon is about **+16.7 MB RSS / +10.8 MB anonymous** (fixed, independent of journal size).
- One actionable defect was found, but it is CPU/IO/allocator churn, not retained heap: `DurableQueue` rewrites and fsyncs its whole file on every push (#11137).
- The conclusion has real limits (see "Limits"). The poll-cycle path (artifact spans, job drafts, JUnit span materialisation) was **not exercised**; it remains an unresolved hypothesis for the original captain symptom.

## Environment and build

| item | value |
|---|---|
| host | AWS Linux 7.0.0-1014-aws, Intel Xeon Platinum 8488C, 16 vCPU, 126 GB RAM, no swap (shared dispatch worker) |
| source commit | `4f03f19bb` (v0.19.963) for the allocation harness |
| daemon binary | `cargo build --release`, default features, built from the same tree at `b426c9ecd` (v0.19.960, version string embedded in the binary); the 3 later commits touch only `observability/lifecycle.rs`, not `ci_telemetry/*`, `observability/queue.rs` or `sender.rs` |
| allocator | glibc malloc (system allocator; `ldd` shows libc/libm/libgcc_s only), `MALLOC_ARENA_MAX` unset |
| isolation | `env -i`, `HOME`/`LOOM_WORKSPACE`/`LOOM_SOCKET_PATH` under `/tmp/loom-11114-exp/<run>/`, `LOOM_GH_BIN` = stub printing `[]`, `LOOM_NO_RESTORE=1`, `LOOM_HOST_ID=scratch`; no production daemon touched, no credentials |
| export sink | local Python HTTP sink on `127.0.0.1:18099` that discards bodies; dummy ingest key |

Effective nonsecret config for the "on" runs (`.loom/config.json` in the scratch workspace):

```json
{"version":"2","fleet":{"captain":"scratch"},
 "autonomous":{"ciTelemetry":{"enabled":true,"owners":["example"],"intervalSecs":120}},
 "observability":{"enabled":true,"endpoint":"http://127.0.0.1:18099/ingest","flushIntervalSecs":30}}
```

"Off" is `{"version":"2"}`. Queue capacity is the default 2000; ETA was not toggled (the parent already showed it had no effect, and it is outside this path).

## Part 1: live daemon, telemetry off vs on (same binary, 720 s windows)

Sampled every 5 s from `/proc/<pid>/smaps_rollup` and `status`. The daemon is measured separately from its descendants; children RSS was 0 in every sample (no child processes spawned).

| run | journal | peak RSS | steady RSS (last 60 s) | steady PSS | steady anon | VmHWM |
|---|---|---|---|---|---|---|
| s0 telemetry off | none | 36.0 MB | 36.0 MB | 33.5 MB | 4.5 MB | 36.0 MB |
| s1 on, empty journal | none | 52.7 MB | 52.7 MB | 50.1 MB | 15.3 MB | 59.2 MB |
| s2 on, 562.2 MB journal (1.2 M `ci.job` lines) | 562,200,000 B | 53.5 MB | 53.5 MB | 50.9 MB | 15.9 MB | 53.5 MB |

Reading: telemetry on costs about +16.7 MB RSS, mostly anonymous (exporter runtime and client, poller thread). A 562 MB journal adds about 0.8 MB on top. RSS plateaus within roughly 6 minutes (slow +5 MB creep over the first 360 s, then flat).

Limit of this part: in s2 the export pass did not finish inside the 720 s window (the daemon was stopped mid-pass; the queue file held its 2000-record capacity). The reason is the per-offer cost in Part 3, so peak daemon RSS during a *completed* pass was not observed live; Part 2 covers a completed pass at the library level.

## Part 2: allocation evidence for the export path

A scratch crate (outside the repo, depending on `loom-daemon` by path, release profile) installed a counting `#[global_allocator]` (live bytes, peak live, total allocated, call count, backtrace of the largest allocation) and drove the real `ci_telemetry::export::backfill`. Profiling overhead: atomic counters per allocation plus a one-off backtrace per new largest allocation; wall times below include it, so treat them as upper bounds. No external heap profiler (heaptrack, massif, dhat) is installed on this host and none was installed.

| scenario | offered | wall | total alloc | alloc calls | peak live heap | end live heap |
|---|---|---|---|---|---|---|
| empty journal, no-op sink | 0 | 0 s | 0.44 MB | 153 | 363 KB | 299 KB |
| 562 MB journal, no-op sink | 1,200,000 | 2.8 s | 1.30 GB | 13.2 M | 365 KB | 299 KB |

Reading: streamed export is bounded. About 1.1 KB and 11 allocations are churned per record (line buffer, `from_utf8_lossy`, envelope parse), all freed immediately; the largest single allocation is the 96 KB line buffer. Process RSS stayed at 5.7 MB. The end-of-pass cursor was `{"byte_offset":0,"exported":1200000}` and the journal was rotated to `ci-telemetry.jsonl.1`, as designed.

## Part 3: the queue behind the export

Same harness, real `DurableQueue` (capacity 2000) behind the export (`offer_durable` per record):

| offered | wall | total alloc | alloc calls | peak live heap | queue len / dropped |
|---|---|---|---|---|---|
| 1,000 | 6.2 s | 0.55 GB | 1.5 M | 1.7 MB | 1000 / 0 |
| 5,000 | 48.8 s | 7.9 GB | 24 M | 3.1 MB | 2000 / 3000 |
| 20,000 | 194 s | 36.5 GB | 114 M | 3.1 MB | 2000 / 18,000 |

Dominant live allocation: the queue's `VecDeque<TelemetryEnvelope>` (the largest single allocation is its 2048-slot buffer of 424-byte envelopes, 868 KB, call path `export::backfill` -> `scan_lines` -> `DurableQueue::push_inner` -> `VecDeque::grow`). Live heap is capped at about 3 MB by the capacity bound.

Measured: every push re-serializes the whole queue, rewrites a temp file and renames it, and a durable push also fsyncs the file and its directory. That is about 9.7 ms and 1.8 MB of transient allocation per record at full queue, extrapolating to about 3.3 hours and about 2 TB of allocator churn to drain 1.2 M records. Filed as #11137.

Also observed (already owned by #11115, not re-filed): draining a backlog larger than the queue evicts the oldest records (98.5 % of the 20,000 offered were dropped).

## Unresolved hypotheses and what was not measured

1. **Poll-cycle memory** (`ci_telemetry/poll.rs` job drafts, `artifact_spans.rs` per-run envelopes, JUnit span materialisation of tens of thousands of spans per pass on 111 repos): not exercised, because the `gh` stub returned an empty list. The original +2.2 GB "ci_telemetry on" delta may live here; this investigation neither confirms nor refutes it.
2. **Allocator-retained pages**: the churn above (large transient buffers, hundreds of GB total) could fragment glibc arenas in the multi-threaded daemon. The harness is single-threaded with an instrumented allocator and cannot show it. The parent's `MALLOC_ARENA_MAX=2` result (worse) is not evidence either way.
3. **The ~2.2 GB Linux baseline with telemetry off**: not reproduced; the scratch daemon idles at 36 MB. It plausibly depends on workspace size (111 repos, registries, sweep state, restored sessions), which `LOOM_NO_RESTORE=1` and an empty workspace deliberately avoid. Not attributed.
4. **Mac vs Linux**: single platform available, no comparison made.
5. **Workload realism**: records were synthetic `ci.job` lines; windows were 12 minutes, not hours; sender batch snapshots (`sender.rs`) were exercised only against a draining sink.

## Reproduction

Scripts lived under `/tmp/loom-11114-exp` (daemon runs: `run.sh`, journal generator, discarding sink) and `/tmp/loom-11114-prof` (allocator harness); they are scratch and intentionally not committed. Journal generator: one JSON line per job with `schema_version` 8, `kind: ci.job`, 468 bytes each, 5 jobs per run id.

## Follow-ups

- #11137: `DurableQueue` rewrites and fsyncs the whole queue file on every push (filed, linked to #10420).
- Already tracked elsewhere: #11115 (admission/drops), #11062 (cursor crash safety; note the cursor is only saved at the end of a pass, so a stopped long pass restarts from the beginning).
