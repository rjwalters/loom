# Loom SigNoz trial

This is the SigNoz destination for the same neutral collector and fixture used
by [ClickStack](../clickstack/README.md). It follows the supported
[Foundry Docker workflow](https://signoz.io/docs/install/docker/), rather than
the deprecated legacy installer. No Loom shell wrapper is added. Foundry's
generated Compose retains upstream container bootstrap/migration commands.
Real lifecycle acceptance depends on #8524/#8525 and the shared #8529 evaluation.

## Pin and render

Use **Foundry v0.2.17**, downloaded manually from its
[release assets](https://github.com/SigNoz/foundry/releases/tag/v0.2.17).
Verify the matching SHA-256 before extracting and invoking `bin/foundryctl`:

| Archive | SHA-256 |
| --- | --- |
| `foundry_darwin_arm64.tar.gz` | `5664c5cf33531dc35bc7f951d331379f23aad1f944417a918d0922d3f77424c4` |
| `foundry_darwin_amd64.tar.gz` | `cef8039e245095741d27b559d4de30ef8b6120b61e2e48e0dfeefd136c970da8` |
| `foundry_linux_arm64.tar.gz` | `1c2ad64cf8794d355eba947a202e1b2bc6fd660bb943735e26d789e00f52fd3a` |
| `foundry_linux_amd64.tar.gz` | `51f41204b8048cd1f7e278fb5d2ba5d82d2ee8fb619bfe9330e2f8ceffc0d886` |

Run from this directory, with the verified binary on PATH:

```console
foundryctl --no-ledger --no-updater gauge -f casting.yaml
foundryctl --no-ledger --no-updater forge -f casting.yaml -p pours
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml config --quiet
git diff -- casting.yaml.lock pours
```

`casting.yaml` is the source of truth; edit its component specs or JSON patches,
then render. `casting.yaml.lock` and `pours/` are committed reviewable outputs.
Do not hand-edit generated configuration or run `cast` before inspecting it.
After any render, run `cargo test --test signoz_deployment_contract`: it re-reads
the committed `pours/` output and fails if a regenerated deployment stops matching
what this README promises — see [Rendered-deployment contract](#rendered-deployment-contract).
All component images pin multi-platform index digests in the casting: SigNoz
**v0.142.1**, its collector **v0.144.10**, ClickHouse and Keeper **25.12.5**,
and PostgreSQL **16**. Each index includes Linux amd64 and arm64. The upstream
histogram helper **v0.0.1** init command is patched declaratively to verify SHA-256
before extraction and reject unsupported architectures. Independent downloads
matched the [upstream checksum manifest](https://github.com/SigNoz/signoz/releases/download/histogram-quantile%2Fv0.0.1/histogram-quantile_0.0.1_checksums.txt):
Linux arm64 `e5605ebffa82a450ebbcdf6cf19dad546e1e40d52dbf3e03cfd2d2d5b7394211`,
Linux amd64 `33997073eb6d82b7be4f27f9fc8ec9e28a395e0f20674d5d54bb2fa28a75488d`.

## Start and trust boundary

Docker Engine 20.10+ and Compose v2 are required. Upstream requests at least
4 GiB assigned to Docker **for SigNoz alone**; existing workloads, ClickStack and
the neutral collector require additional headroom. The steady-state service caps
total 3.75 GiB (ClickHouse 2 GiB, app 768 MiB, collector 512 MiB, PostgreSQL and
Keeper 256 MiB each), plus 768 MiB for transient initialization/migration.
These are trial limits, not capacity claims. Check `evidence.md` for observations.

Before rendering Compose configuration or starting services, create a private
mode-0600 env file outside every checkout containing
`SIGNOZ_TOKENIZER_JWT_SECRET=<independent random session-signing secret>` and
`SIGNOZ_POSTGRES_PASSWORD=<independent random hexadecimal database password>`.
Use a hexadecimal database password so its raw interpolation is URL-safe.
Foundry escapes userinfo placeholders; an asserted declarative patch restores
Compose interpolation in the generated application DSN. Neither the casting,
lock nor rendered files contain actual credentials. Missing either value fails
Compose validation. Never place private env files, browser state or token/session
responses in a repository, including ignored paths or managed worktrees.
Keep this stable across restarts and include it in private backups. Missing it
fails Compose interpolation rather than starting with an empty signing secret.
Full `docker compose config` would expose it: use `config --quiet`.
Docker administrators can inspect the app environment. This key is separate
from provider keys, ingestion auth and the UI account password. Rotating it
invalidates existing sessions; schedule rotation and sign in again afterward.

Create the external `loom-observability` network once if absent. The project,
containers, private network and persistent volumes all use the `loom-signoz`
prefix. They do not reuse another SigNoz installation. Only the app is published:
**127.0.0.1:18081**. OTLP, ClickHouse, PostgreSQL, Keeper and OpAMP remain private.
Only the ingester joins the shared trial network, with alias
**signoz-otel-collector**, HTTP **4318**. Configure the neutral gateway's SigNoz
exporter for `http://signoz-otel-collector:4318` without an ingestion header.

This follows self-hosted SigNoz's unauthenticated private receiver convention.
The neutral gateway authenticates Loom. Trust containers attached to the shared
network and administrators of the host/Docker socket. The generated private
PostgreSQL account is named `signoz` and requires the external password above.
For an existing volume, changing the env file alone does not rotate PostgreSQL:
change the owned database role password first through a private administrative
connection, then update the app and database together with the matching env file.
Keep password values out of command arguments, output and SQL/query logs.
This is not a production hardening recipe. No provider key,
including `ZAI_API_KEY`, belongs in this deployment.

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml up -d --wait --wait-timeout 1800
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml ps
curl --fail http://127.0.0.1:18081/api/v1/health
```

The migration job must complete before the app starts, and the ingester waits
for app health before contacting its OpAMP service. Readiness
budgets allow a busy development VM; a timeout still needs investigation.
The explicit `opamp.yaml` override is required: this pinned Foundry render
otherwise selected PostgreSQL's hostname for the app's port 4320. Verify that
regeneration retains the correct app endpoint, not just valid YAML.
Inspect `docker compose ... logs` for the failing service, with credentials and
workload content removed before sharing. Account passwords/session tokens are
distinct from ingestion credentials. The ingester has no public host port.

### Register the first user *before* expecting ingestion

**Compose readiness does not mean the receiver is open.** On a fresh deployment,
`up -d --wait` reports every service healthy — the ingester included — while its
OTLP receivers are still closed. The ingester's OpAMP client cannot register
until an organization exists, so the app rejects it every 30s with
`cannot create agent without orgId`, and it never applies the effective config
that binds 4317/4318. Traffic sent in this window is refused at TCP connect
(`connect: connection refused`), so it is **not** visible anywhere in SigNoz;
only the neutral gateway's own `otelcol_exporter_*` series show it. Register the
first local user at <http://localhost:18081>, or `POST /api/v1/register` with
`{name, orgName, email, password}` (passwords must be 12+ characters with upper,
lower, digit and symbol). Confirm, and only then send telemetry:

```console
curl --fail http://127.0.0.1:18081/api/v1/version   # expect "setupCompleted":true
```

This gate is **first-run only**: once the organization exists it survives
restarts, and the ingester re-registers with no further errors. It is also
recoverable rather than lossy — the gateway's sending queue holds the refused
batches and drains them intact once the receiver opens, as long as the backlog
fits that persistent queue (`queue_size: 1000` requests, with
`block_on_overflow: false`, so an overflow **drops** the excess rather than
back-pressuring the producer). Register before pointing real fleet traffic at
the gateway. Treat `setupCompleted` as the real ingestion-readiness signal; a
container-running or Compose-healthy result is not one.

Keep deployment copies and volumes outside Loom-managed worktrees for a running
trial: those worktrees are removed on merge. Copy the complete rendered directory
tree to a private stable directory before starting it; all bind paths are relative
to the rendered Compose file. Never mix paths from two renders.

## Rendered-deployment contract

Everything above was established once, by hand, on a trial host — but all of it
is a property of *generated* files, so a later `forge` run, an upstream default
change or a hand-edit can undo any of it silently. `cargo test --test
signoz_deployment_contract` re-derives these from the committed `pours/` output
in ordinary CI, with no Docker, network or credential:

| Guarded property | Why a silent regression matters |
| --- | --- |
| Exactly one published host port, loopback-bound, and never a private container port | Self-hosted SigNoz's OTLP receiver is unauthenticated; publishing 4317/4318, or binding the UI to `0.0.0.0`, turns the trial host into open ingress |
| Only the ingester joins `loom-observability`, aliased to the hostname the gateway's `otlp_http/signoz` exporter actually resolves, and that network stays `external: true` | An alias or network rename breaks delivery in a way visible only in the gateway's own metrics, never in SigNoz |
| Project name, container names, volume names and the private network all stay under the `loom-signoz` prefix | This is what keeps `down --volumes` from reaching the host's separately-owned SigNoz installation |
| Every site that consumes a credential — env key, and the `user:password@host` userinfo of the Postgres DSN — holds a `${VAR:?…}` required interpolation, in the casting, the lock and the render alike | A committed literal is a leaked secret; a non-required interpolation starts the stack with an empty signing secret |
| Every image is digest-pinned and byte-identical to the casting, and its version tag is still the one this README lists | Catches a floating tag, a hand-edited render, and a stale version table |
| The histogram helper's SHA-256 check runs *before* `tar -xzf`, pins both architectures, refuses unknown ones, and matches the digests above | It is the one component fetched at start-up rather than pinned by digest, so ordering is the whole integrity property |
| The documented 3.75 GiB steady-state / 768 MiB transient budget is the sum of the rendered `mem_limit`s, and every service caps logs at three 10 MiB files | Sizing figures an operator provisions a host against, and the disk claim below |

It deliberately asserts nothing about a *running* deployment: readiness, storage
and retention stay `evidence.md`'s job.

## Repeatable investigations

Send the shared three-signal Rust fixture through the neutral gateway, then
verify IDs and values in ClickHouse and the actual UI. HTTP 200 at the gateway
alone does not prove either backend indexed the data. Use the same fixture
manifest/time window for both products, and compare unique `(trace_id, span_id)`
alongside row counts because replay can duplicate data.

### Workloads

Three inputs exist, and they are not interchangeable. Only the third establishes
a real Loom lifecycle; the first two establish transport and schema behaviour.

| Input | Command | Establishes |
| --- | --- | --- |
| Ad-hoc three-signal probe | the original single-trace check behind `queries.sql` | Deployment proof already recorded in `evidence.md` |
| Shared fixture manifest | `loom-daemon telemetry-fixture --run-id <id> --start-time <ts> --output <dir>` then `loom-daemon telemetry-export --input <dir>/envelopes.jsonl --endpoint <gateway> --key-file <private key>` | Identity/parentage/absence parity with ClickStack, per `fixture-queries.sql` |
| Real Loom canary | `loom-daemon telemetry-live-canary …` (plans by default; `--execute` runs paid attempts) | Actual instrumented execution — still not a full issue lifecycle |

The shared fixture is the artifact the cross-backend comparison (#8529) consumes.
It sets `host.id` and `service.instance.id` to `loom-synthetic-<run-id>`, so bind
that value once and isolate a trial without guessing a time window:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery --param_run='loom-synthetic-<run-id>' < fixture-queries.sql
```

Its version-1 manifest expects **37 distinct spans, 14 correlated logs and 3
metric data points**, and it deliberately carries a privacy sentinel under a
prohibited `prompt.content` attribute that the gateway must drop. Both the
expected counts and that assertion are covered by `fixture-queries.sql`.
Generation and the full expected-distinction table are documented in
[telemetry fixtures](../../docs/telemetry-fixtures.md); the span-name enumeration
and attribute vocabulary are documented in [execution traces](../../docs/tracing.md).
`loom-daemon/tests/signoz_trial_artifacts.rs` fails in ordinary CI if either saved
query file drifts from that vocabulary or from the gateway's `keep_keys` allowlist,
because such a query returns zero rows rather than an error — which is
indistinguishable from the backend having lost the data. The same test re-derives
the counts quoted above and in `fixture-queries.sql` from the generated manifest,
so a later fixture version cannot leave a stale total here for an observation to
be compared against.

### CI retro queries

`ci-queries.sql` is the standing build/CI retro (#8826): six numbered sections
over what `loom-daemon ci-telemetry` captures — duration trend, regression
spotlight, outcome mix, top slow jobs, failed run → logs, and run waterfall.
Sections 1–3 read the `loom.ci.*.duration_ms` histograms (kept 30 days);
4–6 read the `ci.run` / `ci.job` / `ci.job.log` records (kept 7 days). Bind
all five parameters once:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery --param_since='2026-09-01 00:00:00' --param_repo='' --param_bucket_hours=24 --param_window_hours=168 --param_top=20 < ci-queries.sql
```

The same `signoz_trial_artifacts.rs` guards it more strictly than the fixture
queries: every log attribute it reads must be in the gateway's **log**
`keep_keys` *and* be read from the SigNoz map column matching the type the
daemon sends it as (`loom.ci.run_id` is an int, so `attributes_number`), every
metric label must be a CI histogram label the **datapoint** allowlist keeps,
and every metric series must be a `.sum` / `.count` series of the two CI
histograms. Policy and pipeline: [CI observability](../../docs/ci-observability.md).

### Saved views

| Saved view | Procedure |
| --- | --- |
| Loom issue waterfall | Trace Explorer: filter the observed trace ID, open the trace, inspect parent IDs and failed Judge → Doctor → successful Judge attempts (`fixture-queries.sql` 2 and 4) |
| Failed attempts | Trace Explorer: error status on `loom.role_attempt`, group by observed `loom.repo`, `loom.role`, `loom.runtime` and `loom.model`; keep missing labels missing (`fixture-queries.sql` 3 and 8) |
| Phase duration | Trace Explorer: duration of the observed `loom.phase` / `loom.role_attempt` spans, grouped by role/runtime/model, with the same time range as ClickStack (`fixture-queries.sql` 3) |
| Correlated logs | Logs Explorer: exact trace ID and span ID; follow the trace link and inspect related logs from the selected span (`fixture-queries.sql` 6) |
| Host/token gauges | Metrics Explorer: the actual emitted names and units — the shared fixture emits `loom.tokens.usage_fraction` and `loom.tokens.exhausted` only, labelled by `account`. An absent series is not a measured zero: the fixture's `synthetic-unknown` account intentionally has no `usage_fraction` point while `synthetic-zero` has `0.0` (`fixture-queries.sql` 7) |
| Delivery health | Scrape the neutral gateway's own Prometheus endpoint (`config.yaml` publishes `detailed` telemetry on port 8888) and read `otelcol_exporter_*` series filtered to `exporter="otlp_http/signoz"` — queue size, sent, send-failed and enqueue-failed. These series are **not** exported into SigNoz through the OTLP pipeline, so they are unavailable in the UI and must be captured beside it. Backend readiness is not delivery evidence |
| In-progress sweep | Compare partial child spans before the root completes, then query again after completion; do not infer success from a missing root/end span (`fixture-queries.sql` 5, which lists every trace with children but no `loom.sweep` root) |
| CI duration trend | Dashboards → New dashboard `Loom CI` → Time series panel. Metric `loom.ci.job.duration_ms`, aggregation **P50**, a second query on the same metric with **P95** and a third with **Max**; group by `repo`, `workflow`, `job`; optional filter `repo = '<owner/name>'`; time range 30 days. UI percentiles interpolate within the histogram's 1s…6h bucket bounds; the SQL is exact (`ci-queries.sql` 1) |
| CI regression spotlight | Same dashboard → Table panel. Metric `loom.ci.job.duration_ms`, aggregation **P95**, group by `repo`, `workflow`, `job`, time range = the current window; duplicate the panel with the time shift/compare set to one window earlier and sort by the difference. The ranked delta and the ≥2-runs-in-both-windows rule live in the SQL (`ci-queries.sql` 2) |
| CI outcome mix | Same dashboard → Stacked bar panel. Metric `loom.ci.run.duration_ms`, aggregation **Count** (one data point per run), group by `workflow`, `conclusion`; keep `cancelled` as its own series — never filter it out (`ci-queries.sql` 3) |
| CI top slow jobs | Logs Explorer: filter `body = 'ci.job'`, add columns `loom.repo`, `loom.ci.workflow`, `loom.ci.job`, `loom.ci.duration_ms`, `loom.ci.run_id`, `loom.ci.job_id`; sort by `loom.ci.duration_ms` descending; save as view `CI top slow jobs` (`ci-queries.sql` 4) |
| CI failed run → logs | Logs Explorer: filter `body = 'ci.run' AND loom.ci.conclusion IN ('failure', 'timed_out', 'startup_failure')`, save as `CI failed runs`; take a `loom.ci.run_id`, filter `body = 'ci.job' AND loom.ci.run_id = <id> AND loom.ci.conclusion != 'success'` for its failing jobs, then `loom.ci.job_id = <job id> AND loom.ci.chunk_index EXISTS` sorted by `loom.ci.chunk_index` ascending for the job's log, in order (`ci-queries.sql` 5) |
| CI run waterfall | Trace Explorer: filter `name = 'loom.ci.run'`, sort by duration descending, open a run; its `loom.ci.job` children are the waterfall, and the longest child against the root's duration is the run/longest-job comparison (`ci-queries.sql` 6) |

The six CI rows are **recreation steps, not yet observed**: no session has had
an authenticated UI (or API) credential for the trial org since they were
written, so none of the six has been created there yet
([#8946](https://github.com/rjwalters/loom/issues/8946)). Their SQL
counterparts in `ci-queries.sql` are the executed, verified form — see
`evidence.md`'s "CI retro queries, executed live" section — and remain the
acceptance surface until someone with the trial org's login creates the saved
views and records it.

Save these searches/dashboards through the installed UI and retain sanitized
exports where supported. These precise steps avoid asserting that mutable
organization-specific dashboard IDs are portable. Loom uses operational spans,
not fabricated HTTP requests. Trace Explorer is the acceptance surface; an empty
HTTP service-map/APM page does not establish a missing trace. Record actual
Community-edition limitations and missing usage separately from measured zeros.

`fixture-queries.sql` was executed live on 2026-09-22 against a real trial
deployment; every assertion held on the first pass, including totals, the
repair-chain graph, root-less-trace detection, absence-vs-zero gauges and the
privacy-sentinel drop. See `evidence.md`'s "Shared fixture manifest, executed
live" section for the full results. Query 0 remains a schema preflight in case
a future SigNoz pin renames these columns.

## Cycle-time analytics (Issue #8665)

`cycle-time-extract.sql` is this backend's half of the shared cycle-time
artifact set — one view, `loom_analytics.raw_ship_outcome`, mapping
`signoz_logs.distributed_logs_v2` rows onto the normalized ship columns that
[`../cycle-time-rollup.sql`](../cycle-time-rollup.sql) ingests and
[`../cycle-time-queries.sql`](../cycle-time-queries.sql) answers CT1–CT8 from.
The questions and their definitions are in
[`../cycle-time-questions.md`](../cycle-time-questions.md); ClickStack uses the
same three shared files with only its own extraction view swapped in, which is
what makes the two backends comparable under #8529.

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < cycle-time-extract.sql
```

**Not yet executed live here.** The ClickStack side is verified end to end in CI
against a real pinned ClickHouse; this view's column contract is checked by
`loom-daemon/tests/cycle_time_artifacts.rs`, but no number produced by it has
been compared against the ClickStack answers on the same fixture yet. Treat it
as unproven until that comparison is recorded in `evidence.md`, exactly as the
trial treats every other unexecuted claim.

## Retention and operation

Set **seven days for logs and traces and 30 days for metrics** in General
Settings → Retention (#8826). Metrics outlive raw logs/traces on purpose: the
CI duration/outcome trends in `ci-queries.sql` 1–3 are the retro asset, and
the standing policy and its rationale are in
[CI observability → Retention](../../docs/ci-observability.md#retention).
The upstream defaults are 15 days for logs and traces and 30 for metrics, so a
fresh render alone does not establish the split. Verify effective table DDL in
ClickHouse after changing the setting, including derived tables; record it in
`evidence.md`. The pinned API updates active signal tables and standard
rollups, but leaves some metadata, reduced-metric and legacy tables at 15 or
30 days (one month for `top_level_operations`). For this isolated trial, apply
the reviewed `retention.sql` after the API settings — it sets those existing
TTLs to 7 days for logs/traces and 30 days for metrics, and restores 30 days
on a trial that ran its earlier all-seven-day version — using the private
bundled client:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < retention.sql
```

Re-run `queries.sql` after every upgrade or retention-setting change. Resource
fingerprint tables retain the upstream **30-minute grace beyond seven days**;
shorter buffer/usage TTLs remain unchanged. Schema migration records, metric
reduction configuration and legacy metadata indexes have no signal TTL. This
is a seven-day log/trace and 30-day metric trial, not a claim that all
metadata is erased at either boundary. Confirm the split from effective DDL,
not from the settings page:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --query "SELECT database, name, extract(create_table_query, 'TTL (.*?)(SETTINGS|\$)') FROM system.tables WHERE database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics') AND create_table_query LIKE '%TTL%' AND engine NOT LIKE 'Distributed%' ORDER BY database, name"
```

`signoz_logs.logs_v2` keys its TTL on a per-row `_retention_days` column rather
than a literal interval, so read that column's `default_expression` from
`system.columns` too (a column default applies to rows as they are inserted, so
check the stored values of older rows as well before trusting the split).
TTL deletion uses
background merges and is not an exact deletion deadline or a disk quota.
Accounts, dashboards and settings in PostgreSQL persist independently.
Container stdout/stderr rotate separately at three 10 MiB files per service;
ClickHouse's own system tables and metadata also consume storage.

Use the same `--env-file` and `-f` arguments for every Compose command.
`docker compose ... stop` preserves all data.
Restart with the same rendered files and `up -d --wait --wait-timeout 1800`;
verify an old trace, gauge and saved view before accepting restart persistence.
For backup, stop this project and snapshot its four named volumes together:
PostgreSQL data, Keeper coordination, ClickHouse data and histogram user scripts.
Retain the casting, lock and rendered configuration. Test restoration into a
separate project/network before relying on the backup. Never stop or remove
another deployment's containers/volumes.

Upgrade by changing explicit pins, rendering, inspecting the diff and testing
schema migration/restore against a copy of trial data. Database migrations may
prevent rollback by simply selecting an older image. To deliberately wipe only
this disposable trial, stop it and use `docker compose ... down --volumes`
with those same arguments; this permanently removes its
stored signals, accounts and saved views. Do not use that command for routine stop.

For a later [SigNoz Cloud trial](https://signoz.io/docs/ingestion/signoz-cloud/overview/),
replace only the gateway exporter endpoint with the regional TLS OTLP endpoint
and its `signoz-ingestion-key` header, sourced from a private secret. Keep query
API keys separate from ingestion keys. Loom's neutral endpoint and trace identity
do not change; Cloud is optional and is not an acceptance prerequisite.
