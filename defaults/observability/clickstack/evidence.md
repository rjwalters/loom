# ClickStack trial evidence

## Reproduction identity

Validation on 2026-09-21 used the pinned ClickStack all-in-one 2.39.1
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

## Dependency status update (2026-09-23, issue #8527)

At the time of this pass, #8524 (durable trace context and OTLP **span**
export in Rust) and #8526 (the neutral collector fan-out this deployment
sits behind) had both merged. #8525 (sweep-phase/role-attempt instrumentation,
including repair cycles) had its implementation land in #8579, but the issue
itself remains `loom:operator-only`: what is left there is acceptance
*evidence* — an authorized paid GLM canary and backend-visible links for a
real Loom execution — not missing code. #8529 (the shared SigNoz/ClickStack
comparison) is still `loom:blocked` with no authoritative fixture yet.

Inspecting the merged `loom-daemon` source (`loom-daemon/src/observability/lifecycle.rs`,
`loom-daemon/src/sweep_registry/dispatch.rs`, `loom-daemon/src/worker_spawn/mod.rs`)
confirms the real production code path — not a hand-built fixture — already
creates a `loom.sweep` root span at real sweep dispatch
(`lifecycle::prepare_execution`, called from `sweep_registry::dispatch`),
samples real `loom.phase` transitions off the live checkpoint
(`lifecycle::phase_transition`, called from `sweep_registry/outcome_journal.rs`),
and opens a real `loom.role_attempt` span for every worker spawn
(`lifecycle::worker_attempt`, called from `worker_spawn::run`). Real daemon
spans carry `service.name = loom-daemon` (`observability/otlp/mapping.rs`),
distinct from the `loom-trial-fixture` service name used below, so a genuine
trace is queryable separately from this evidence's synthetic fixture once one
has been sent through this deployment.

Two in-tree Rust integration tests already exercise that real code path end
to end (not the `telemetry::fixture` generator used for the rest of this
evidence file): `loom-daemon/tests/successful_sweep_waterfall.rs` (a strictly
nested successful-sweep waterfall driven by the real `sweep-checkpoint` CLI)
and `loom-daemon/tests/lifecycle_traces.rs::actual_checkpoint_cli_preserves_rapid_judge_doctor_repair_waterfall`
(a rejected-Judge → Doctor → successful-Judge repair sequence, driven by the
same real CLI, asserting distinct span IDs and correct Ok/Error status on
each Judge attempt). Both passed in this pass's worktree
(`cargo test -p loom-daemon --features otlp --test lifecycle_traces --test
successful_sweep_waterfall`, single-threaded to avoid an unrelated flake this
pass's heavily contended host produced under parallel execution) against
unmodified `main`. Both only assert against the in-process durable trace
journal drained via `lifecycle::backfill` — neither test starts a receiver or
makes a network call, so they prove the span/parentage/status shape, not
delivery to a live backend.

What this does **not** establish is the two acceptance boxes #8527 itself
still needs: (1) that same real trace, actually delivered through the neutral
collector into **this** live ClickStack deployment and queried back out of
ClickHouse, and (2) a **genuinely** operator-authorized canary — the standalone
mechanism for producing one without a full sweep dispatch is
`loom-daemon telemetry-live --execute` (`loom-daemon/src/cli/telemetry_live.rs`),
which spawns real Pi/OpenCode processes against the `zai-flash` model profile
and therefore requires a real `ZAI_API_KEY` under operator control — the same
credential gate that keeps #8525 itself `loom:operator-only`. This Builder
pass has no such credential, so it could not produce either. A full sweep
dispatch would create a real root span too, but spinning one up purely to
harvest telemetry evidence is out of scope here. Actually assembling a
credential-free reproduction (open a real root span, drive it through
`sweep-checkpoint`, and POST the drained queue with `telemetry-export`) is
described, piece by piece with exact source locations, in `README.md`'s "Real
trace and repair-waterfall verification" section — this pass verified each
piece individually but did not wire them into one tested recipe.

Separately, this pass observed the shared host running sustained `uptime`
load averages of 12–29 on an 8-core machine (many concurrent Loom worktrees,
plus unrelated tenants) while investigating. Starting this deployment's
3 GiB/4-CPU container stack under that contention would very likely reproduce
(or worsen) the multi-minute readiness delays already recorded below, and adds
load to a host other concurrent Builder/Judge/Doctor sessions depend on. This
pass therefore did not start ClickStack, and left the two open acceptance
items for a session run when either an operator supplies canary credentials,
or the host has headroom for a dedicated live-verification pass — see the
reproduction recipe in `README.md`'s "Real trace and repair-waterfall
verification" section.

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

## Machine-private session credentials

The credential audit confirmed that the upstream entry script unconditionally
overwrites `EXPRESS_SESSION_SECRET` with a public constant. Setting only a Compose
environment value is ineffective. The derived-image Dockerfile asserts exactly
one expected assignment before removing it with JSON exec-form tools; its base
remains the pinned official digest. The image built successfully on Linux arm64.
Compose now refuses to run without the independent machine-private session key,
and `.dockerignore` excludes all local context files from the image build.

A boolean-only inspection found exactly one API process and confirmed that its
actual environment value matched the private machine key. App and collector
health passed. The old signed cookie received **401** from `/api/sources` after
rotation. Stored fixture data survived: **36 trace rows / 4 unique spans,
6 logs and 5 gauges with value 3**. Additional rows reflect deliberate replays and
outage/retry delivery, not new trace identities or a throughput measurement.
Signing in again with the existing account produced a new session and an
authenticated **200** from the same protected sources endpoint; no account or
source recreation was needed.

Private key, env, password, cookie and browser-state files live outside every
checkout with mode 0600. Filename-only scans of known trial secret/token/cookie
values found no matches in repository source or managed worktrees. Concurrent
local backend startup caused probe timeouts on this occupied host without OOM
flags; final checks therefore ran sequentially with persistent volumes retained.

| Remaining check | Status |
| --- | --- |
| Log-to-trace waterfall and correlated log rows | Passed in the actual HyperDX browser UI |
| Bootstrap-key rotation and restart persistence | Passed against the actual receiver and persisted tables/source configuration |
| Real (non-fixture) production span/waterfall code path | Proven in-tree against the local durable trace journal (`successful_sweep_waterfall.rs`, `lifecycle_traces.rs`); **not yet delivered to or queried from this live deployment** — see "Dependency status update" above |
| Real Loom canary and real Judge/Doctor repair trace, stored and queried in **this** deployment | #8524's code is merged and #8525's implementation landed, but producing one needs either a full sweep dispatch or `telemetry-live --execute`, which requires an operator-held `ZAI_API_KEY` — same gate as #8525's own remaining acceptance |
| Repeated query and ingest-to-visible latency comparison | Shared evaluation #8529 |

The synthetic repair-shaped trace proves transport and schema only. It is not
evidence that Loom emitted real lifecycle spans — that now exists in-tree
(see above) but has not yet been routed through this live deployment. Keep
#8527 open until the two credential/dispatch-gated checks above are recorded.
The shared Rust fixture from #8529 is authoritative for the final
side-by-side comparison.
