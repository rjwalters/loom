# Loom ClickStack trial

This local trial is one destination of the shared collector in #8526. ClickStack
bundles ClickHouse, HyperDX, an OpenTelemetry collector, and MongoDB. HyperDX is
the UI for this destination, not a third backend. Real Loom traces and repair
waterfalls depend on #8524/#8525; a synthetic fixture proves transport and schema,
not Loom instrumentation.

## Pinned distribution and host budget

`compose.yaml` pins the official all-in-one **2.39.1** multi-platform index to
`sha256:b3ff85e093f8dd9da458a2d0c770c6dbafe776adca755c7b91de33528c3b9553`.
The index has Linux amd64 and arm64 images. Docker Desktop on macOS runs these
inside its Linux VM. The recipe caps the container at 3 GiB RAM and four CPUs;
reserve that headroom in addition to existing workloads and the other trial.
This is a trial limit, not a production sizing recommendation. See `evidence.md`
for measured footprint and limitations.

`supervisor.yaml` extends the bundled collector bootstrap deadline from the
upstream three-second default to 120 seconds; a busy VM otherwise leaves the UI
up while its collector has exited. Compose health checks both the application and
collector. Allow up to ten minutes on a contended development VM; investigate
`/var/log/otel-collector.log` if readiness fails. The override is specific to this
pinned all-in-one layout and must be rechecked when upgrading the image.

Upstream supports the all-in-one distribution for demos and local testing. Its
components share one resource limit and the bundled startup script fixes the
session secret. Keep it local on a trusted machine. Production or a shared server
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

Create a private env file outside the checkout, mode `0600`, containing
`CLICKSTACK_INGESTION_API_KEY=<random independent secret>`. Also save the same raw
secret (without `Bearer `) to a separate private key file. Point the neutral
collector's `LOOM_CLICKSTACK_INGEST_KEY_FILE` at that key file. These are ingestion
credentials, never `ZAI_API_KEY` or UI passwords. Set `CLICKSTACK_RETENTION=168h`
in the env file if you want to make the default seven-day retention explicit.

```console
docker compose --env-file /absolute/private/clickstack.env config --quiet
docker compose --env-file /absolute/private/clickstack.env up -d --wait
docker compose --env-file /absolute/private/clickstack.env ps
curl --fail http://localhost:18080/api/health
docker compose --env-file /absolute/private/clickstack.env exec -T clickstack clickhouse-client --query "SELECT 1"
```

Use `config --quiet`: full rendered Compose configuration contains the key.
Docker administrators can inspect container environment variables. This upstream
bootstrap does not support an ingestion-key file setting. Do not put the env file
in source control or attach it to an issue.

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
