# Loom ClickStack trial

This local trial is one destination of the shared collector in #8526. ClickStack
bundles ClickHouse, HyperDX, an OpenTelemetry collector, and MongoDB. HyperDX is
the UI for this destination, not a third backend. #8524 and #8525 have merged:
`loom-daemon` now creates real `loom.sweep`/`loom.phase`/`loom.role_attempt`
spans at real sweep dispatch and worker spawn, proven in-tree
(`loom-daemon/tests/successful_sweep_waterfall.rs`,
`loom-daemon/tests/lifecycle_traces.rs`). Routing one of those real traces
through **this** deployment and querying it back out of ClickHouse is still
open — see "Real trace and repair-waterfall verification" below for the
credential-free recipe (`loom-daemon/tests/clickstack_trial_canary.rs`,
compiled and clippy-checked but not yet run against a live receiver) and the
fully-authorized `telemetry-live --execute` alternative, which needs an
operator-held `ZAI_API_KEY` — the same credential gate #8525 itself is
waiting on. Until either runs against a live receiver, the fixture below
proves transport and schema, not Loom instrumentation.

## Pinned distribution and host budget

`Dockerfile` pins the official all-in-one **2.39.1** multi-platform base index to
`sha256:b3ff85e093f8dd9da458a2d0c770c6dbafe776adca755c7b91de33528c3b9553`.
The index has Linux amd64 and arm64 images. Docker Desktop on macOS runs these
inside its Linux VM. The recipe caps the container at 3 GiB RAM and four CPUs;
reserve that headroom in addition to existing workloads and the other trial.
This is a trial limit, not a production sizing recommendation. See `evidence.md`
for measured footprint and limitations.

`supervisor.yaml` extends the bundled collector bootstrap deadline from the
upstream three-second default to 120 seconds; a busy VM otherwise leaves the UI
up while its collector has exited. Compose health checks both the application and
collector. Allow up to twenty minutes on a contended development VM; investigate
`/var/log/otel-collector.log` if readiness fails. The override is specific to this
pinned all-in-one layout and must be rechecked when upgrading the image.

Upstream supports the all-in-one distribution for demos and local testing. Its
components share one resource limit. The small derived-image Dockerfile asserts
the pinned startup script's single fixed-session-secret assignment and removes
it; otherwise that script silently overrides a private environment value.
The API must receive the required external session secret. Keep the trial local
on a trusted machine. Production or a shared server
needs the upstream separated deployment and its authentication hardening.
The application UI and Docker socket require a trusted host. ClickHouse is not
published to the host; use the bundled client through Compose exec for read-only
validation. Other containers on the shared trial network are also trusted. Never publish these ports or attach untrusted workloads.

## Start

Run commands from this directory. Docker Compose v2 is required. Create the
external `loom-observability` network once if it does not already exist:

```console
docker network create loom-observability
```

Create a private env file outside every checkout, mode `0600`, containing
`CLICKSTACK_INGESTION_API_KEY=<random independent secret>` and
`CLICKSTACK_SESSION_SECRET=<different random session-signing secret>`.
Never store credentials, browser state or token/session responses in a repository,
including ignored paths and managed worktrees. Also save the raw ingestion
secret (without `Bearer `) to a separate private key file. Point the neutral
collector's `LOOM_CLICKSTACK_INGEST_KEY_FILE` at that key file. These are ingestion
credentials, never `ZAI_API_KEY` or UI passwords. Set `CLICKSTACK_RETENTION=168h`
in the env file if you want to make the default seven-day retention explicit.

```console
docker compose --env-file /absolute/private/clickstack.env config --quiet
docker compose --env-file /absolute/private/clickstack.env up -d --build --wait
docker compose --env-file /absolute/private/clickstack.env ps
curl --fail http://localhost:18080/api/health
docker compose --env-file /absolute/private/clickstack.env exec -T clickstack clickhouse-client --query "SELECT 1"
```

Use `config --quiet`: full rendered Compose configuration contains the key.
Docker administrators can inspect container environment variables. This upstream
bootstrap does not support an ingestion-key file setting. Do not put the env file
in source control or attach it to an issue.

Keep the session secret stable across restarts. Rotating it invalidates existing
browser cookies and requires signing in again; it does not rotate ingestion auth.
Copy the Dockerfile and `.dockerignore` with the Compose deployment before building outside a managed
worktree. The derived image remains based on the exact upstream digest above.

The shared collector sends OTLP HTTP to `http://clickstack-collector:4318` using
raw `authorization`. Ingest is not published to the host. The UI is
<http://localhost:18080>. Create the first local account there; the bootstrap key
allows ingestion before registration. Registration creates correlated Logs,
Traces and Metrics data sources. The account password, session cookie and query
access are separate from the ingestion credential. The all-in-one distribution
allows only one initial team registration.

Health means the app responds, not that ingestion and indexing work. Send the
shared fixture through the **neutral collector**, then run `queries.sql` and
inspect the UI before calling the destination ready.

## Sources and repeatable investigation views

The pinned image provisions these sources when the first team is registered.
Check Settings → Sources after signup; preserve the generated source IDs when
editing them. Legacy tables are selected explicitly in Compose.

| Source | Table | Required mapping and links |
| --- | --- | --- |
| Logs | `default.otel_logs` | timestamp `Timestamp`; body `Body`; severity `SeverityText`; service `ServiceName`; attributes `LogAttributes` and `ResourceAttributes`; IDs `TraceId`/`SpanId`; trace source `Traces` |
| Traces | `default.otel_traces` | timestamp `Timestamp`; duration `Duration` in nanoseconds (precision 9); name `SpanName`; parent `ParentSpanId`; IDs `TraceId`/`SpanId`; status `StatusCode`/`StatusMessage`; attributes `SpanAttributes` and `ResourceAttributes`; log source `Logs` |
| Metrics | `default.otel_metrics_*` | time `TimeUnix`; gauge/sum/histogram/summary/exponential-histogram table mappings; resource attributes `ResourceAttributes`; log/trace source links |

Create these named saved searches using the SQL WHERE input. Select the proper
source and time window before saving; ordinary search text has different syntax.
For a fixture use `ServiceName = 'loom-trial-fixture'`; for a live canary use its
actual `ServiceName` and observed attribute keys. Unknown runtime/model/usage
fields must stay missing rather than receiving guessed defaults.

| Saved view | Source | SQL WHERE / operation |
| --- | --- | --- |
| Loom failed attempts | Traces | `StatusCode = 'Error'`; show `SpanName`, `SpanAttributes`, and duration; filter observed repo/role/runtime/model attributes as needed |
| Loom issue waterfall | Traces | `TraceId = '<selected trace ID>'`; open the trace and inspect parent/child nesting and failed/successful Judge/Doctor attempts |
| Loom correlated logs | Logs | `TraceId = '<selected trace ID>'`; select a log row and follow its trace link, then navigate from a span back to logs |
| Loom phase durations | Traces | Filter observed phase name, then chart duration; group by observed failure class and role attributes |
| Loom capacity gauges | Metrics | Select actual `loom.host.*` / `loom.tokens.*` metric names and units; preserve absent optional measurements |
| Collector delivery health | Metrics | Use collector exported queue/drop/failure measurements if enabled; receiver HTTP success alone is insufficient |
| Loom measured usage | Logs/Traces | Filter only records carrying actual usage attributes; display missing values separately; never infer dollars from token-pool percentages |

The recipe records UI steps instead of checking in mutable team/source IDs or
claiming a dashboard API payload is portable between versions. Export searches
and dashboards through the installed UI where available, inspect them for secrets
and workload text, and preserve sanitized exports with the evaluation evidence.
`queries.sql` provides reproducible read-only schema, parentage, gauge, duplicate
and storage checks independently of UI exports.

## Real trace and repair-waterfall verification

This closes the two acceptance items `evidence.md`'s "Dependency status
update" section leaves open. It needs a `loom-daemon` binary built with
`--features otlp` and this deployment running with the shared collector from
`../collector` in front of it (`LOOM_CLICKSTACK_INGEST_KEY_FILE` pointed at
this project's ingestion key).

**Real canary, without a paid model call.** `loom-daemon/src/observability/lifecycle.rs`
is the production code a real sweep dispatch and worker spawn already use, and it
is not gated on an LLM call. But its span-opening entry points
(`lifecycle::begin`/`prepare_execution`) are only called from real sweep dispatch
(`sweep_registry/dispatch.rs`), the paid canary tool below, and the in-tree tests —
there is currently no standalone CLI subcommand that opens a root span on its
own. `loom-daemon/tests/lifecycle_traces.rs::actual_checkpoint_cli_preserves_rapid_judge_doctor_repair_waterfall`
and `loom-daemon/tests/successful_sweep_waterfall.rs` drive that real code
(`lifecycle::begin`, the real `sweep-checkpoint` CLI for every phase/attempt,
`lifecycle::finish_execution`) end to end and assert a correctly nested,
correctly parented waterfall — including a Judge-rejected → Doctor →
Judge-succeeded repair sequence with distinct Ok/Error span IDs — but only
against an in-process durable-queue backfill, not a network export; both test
files passed against unmodified `main` in this pass. The daemon's real export
path (`observability::{sender,exporter,otlp}`, started by `observability::spawn_task`
whenever a real `loom-daemon` process runs with tracing enabled) drains that
same durable queue to the configured OTLP endpoint continuously, and
`loom-daemon telemetry-export --input <queue.jsonl> --endpoint <collector URL>
--key-file <collector key file>` (`loom-daemon/src/cli/telemetry_export.rs`) does
the equivalent one-shot POST for an already-assembled envelope batch.

`loom-daemon/tests/clickstack_trial_canary.rs` wires those three pieces (a
`begin`-based root span, the same rejected-Judge → Doctor → successful-Judge
checkpoint CLI sequence as the two tests above, and a real
`telemetry-export` POST) into one reproducible, credential-free recipe —
`#[ignore]`d so CI never runs it against a receiver, and compiled/linted like
any other in-tree test (`cargo clippy --package loom-daemon --features otlp
--all-targets`) so it cannot silently rot. Run it once this deployment (or
the neutral collector in front of it) is up:

```console
LOOM_TRIAL_COLLECTOR_ENDPOINT=http://localhost:<collector port> \
LOOM_TRIAL_COLLECTOR_KEY_FILE=/absolute/private/clickstack-ingest.key \
LOOM_TRIAL_EVIDENCE_DIR=/absolute/private/new-evidence-dir \
cargo test -p loom-daemon --features otlp --test clickstack_trial_canary \
  -- --ignored --nocapture
```

It asserts locally (13 real spans, one trace ID, one Error-status Judge
attempt and one Ok-status Judge attempt, real `telemetry-export` success
against the receiver) and writes `envelopes.jsonl` (the exact records POSTed)
and `expected-spans.json` (trace/span IDs, parentage, status and timestamps
to check against `queries.sql`'s `ServiceName = 'loom-daemon'` query) into a
new, private evidence directory. This pass built and clippy-linted the test
against unmodified `main` (both clean) but did not run it against a live
receiver — the shared host was under sustained contention for the whole
investigation (see the host-contention note in `evidence.md`), and standing
up this deployment under that load risks an unreliable validation, matching
the prior pass's decision not to start it either. Running the command above
against an actually-live deployment, and recording its evidence-directory
output alongside this file, is what remains for the "real Loom canary"/"repair
trace" acceptance boxes.

**Fully authorized canary.** `loom-daemon telemetry-live --execute --output
<new private dir> --endpoint <collector URL> --key-file <collector key file>
--guard-dir defaults/hooks --zshrc <file with a literal ZAI_API_KEY=... line>`
(`loom-daemon/src/cli/telemetry_live.rs`) spawns real Pi and OpenCode
processes against the bundled `zai-flash` model profile, producing a real
`loom.sweep` root span from an actual (small, isolated, read-only) model
invocation and exporting it through the same collector. It requires a real
`ZAI_API_KEY` under operator control — the credential/authorization gate
#8525 itself is waiting on for its own acceptance evidence — and is a paid,
rate-limited call; do not run it without that authorization.

## Cycle-time analytics (Issue #8665)

The saved views above answer "what is happening now". The standing answer to
"what took long to ship, and where did the time go?" is a separate, committed
artifact set rooted at [`../cycle-time-questions.md`](../cycle-time-questions.md)
— eight canonical questions (CT1–CT8), with definitions, backed by SQL rather
than by a UI export.

Install it here once, after the sources exist:

```console
docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
  clickhouse-client --multiquery < cycle-time-extract.sql
docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
  clickhouse-client --param_since='2026-09-15 00:00:00' \
    --param_until='2026-09-22 00:00:00' --queries-file ../cycle-time-rollup.sql
```

`cycle-time-extract.sql` is the only backend-specific piece: it maps this
deployment's `default.otel_logs` rows onto the normalized ship columns. The
durable table, its hourly refresh and all eight questions are shared with
SigNoz, so both backends answer from one definition.

**This is also the answer to the seven-day TTL below.** Rather than raising
`HYPERDX_OTEL_EXPORTER_TABLES_TTL` — which does not migrate existing tables and
would apply to the trace tables too — the rollup keeps one row per shipped sweep
for 400 days in its own `loom_analytics` database. The reasoning, the rejected
alternative, and the drift-reconciliation query (CT8) are in the questions doc.
`loom-daemon/tests/cycle_time_clickhouse.rs` executes the committed artifacts
against a real pinned ClickHouse in CI, including the case where every raw row
has expired.

## Retention, persistence and key rotation

The collector table TTL defaults to `168h` through
`HYPERDX_OTEL_EXPORTER_TABLES_TTL`. TTL expiry uses ClickHouse background merges,
so it is not a precise deletion deadline. Schema creation sets the TTL on new
tables; changing the env value does **not** migrate existing tables. Inspect
`system.tables.create_table_query` for every base/derived signal table. For an
existing volume use the upstream TTL procedure and verify the resulting DDL;
do not promise that an env edit changes retention retroactively. MongoDB stores
users, source definitions and saved views independently of signal TTL. Docker
logs rotate separately (three 10 MiB files); inspect ClickHouse server-log volume
usage as well. TTL alone is not a disk quota.

The `loom-clickstack` project keeps three named volumes: `metadata`, `telemetry`
and `server_logs`. They are separate from SigNoz. A non-destructive stop is:

```console
docker compose --env-file /absolute/private/clickstack.env stop
```

Restart with `up -d --wait`, rerun the same trace query, and confirm the saved UI
views and first account still exist. `down` also preserves named volumes; **do
not use `down -v`** unless intentionally deleting this trial's stored data. For
backup, stop only this project, export its three named volumes using your normal
Docker-volume backup tool, and protect the archive like the telemetry itself.
Record image digest and Compose version alongside it. A restore must include
MongoDB metadata and ClickHouse data, not just exported dashboard JSON.

To rotate the bootstrap key, update the private env and gateway key files,
recreate this project's container and restart the shared collector so both
reload credentials. During the mismatch, verify the gateway reports rejected
exports and that its bounded persistent queue behaves as documented. The
team-generated ingestion key is independent and remains valid until separately
rotated in HyperDX; changing the bootstrap key does not revoke it. Never disable
collector authentication to make a trial pass.

## Managed destination later

Keep Loom pointed at the neutral collector. Replace only that collector's
ClickStack endpoint and authentication settings with the managed deployment's
supplied values and TLS configuration. Do not reuse the local bootstrap key or
assume this local ClickHouse schema/config is a managed-service provisioning API.

## Upstream references

- [All-in-one distribution and persistence](https://clickhouse.com/docs/clickstack/deployment/all-in-one)
- [Collector configuration and authentication](https://clickhouse.com/docs/clickstack/ingesting-data/collector)
- [Source mappings](https://github.com/hyperdxio/hyperdx/blob/main/docker/hyperdx/entry.local.base.sh)
- [Supervisor bootstrap configuration](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/v0.155.0/cmd/opampsupervisor/specification/README.md)
- [TTL management](https://clickhouse.com/docs/clickstack/managing/ttl)
- [Pinned source release](https://github.com/hyperdxio/hyperdx/releases/tag/%40hyperdx%2Fapp%402.39.1)
