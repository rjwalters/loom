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
All component images pin multi-platform index digests in the casting: SigNoz
**v0.142.1**, its collector **v0.144.10**, ClickHouse and Keeper **25.12.5**,
and PostgreSQL **16**. Each index includes Linux amd64 and arm64. Foundry's
histogram helper init step separately downloads the upstream **v0.0.1** release;
the generated command does not verify that archive's digest. Review that
upstream dependency before adopting this local trial for a shared environment.

## Start and trust boundary

Docker Engine 20.10+ and Compose v2 are required. Upstream requests at least
4 GiB assigned to Docker **for SigNoz alone**; existing workloads, ClickStack and
the neutral collector require additional headroom. The steady-state service caps
total 3.75 GiB (ClickHouse 2 GiB, app 768 MiB, collector 512 MiB, PostgreSQL and
Keeper 256 MiB each), plus 768 MiB for transient initialization/migration.
These are trial limits, not capacity claims. Check `evidence.md` for observations.

Before rendering Compose configuration or starting services, create a private
mode-0600 env file outside the checkout containing
`SIGNOZ_TOKENIZER_JWT_SECRET=<independent random session-signing secret>`.
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
PostgreSQL account uses upstream's local `signoz` credentials; it is not an
Internet-facing credential or a production hardening recipe. No provider key,
including `ZAI_API_KEY`, belongs in this deployment.

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml up -d --wait --wait-timeout 1800
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml ps
curl --fail http://127.0.0.1:18081/api/v1/health
```

The migration job must complete before the app starts, and the ingester waits
for app health before contacting its OpAMP service. Readiness
budgets allow a busy development VM; a timeout still needs investigation.
Inspect `docker compose ... logs` for the failing service, with credentials and
workload content removed before sharing. Register the first local user at
<http://localhost:18081>. Account passwords/session tokens are distinct from
ingestion credentials. The ingester has no public host port.

Keep deployment copies and volumes outside Loom-managed worktrees for a running
trial: those worktrees are removed on merge. Copy the complete rendered directory
tree to a private stable directory before starting it; all bind paths are relative
to the rendered Compose file. Never mix paths from two renders.

## Repeatable investigations

Send the shared three-signal Rust fixture through the neutral gateway, then
verify IDs and values in ClickHouse and the actual UI. HTTP 200 at the gateway
alone does not prove either backend indexed the data. Use the same fixture
manifest/time window for both products, and compare unique `(trace_id, span_id)`
alongside row counts because replay can duplicate data.

| Saved view | Procedure |
| --- | --- |
| Loom issue waterfall | Trace Explorer: filter the observed trace ID, open the trace, inspect parent IDs and failed Judge → Doctor → successful Judge attempts |
| Failed attempts | Trace Explorer: error status, group by observed `loom.repo`, `loom.role`, `loom.runtime` and `loom.model`; keep missing labels missing |
| Phase duration | Trace Explorer: duration of the observed phase name, grouped by role/runtime/model, with the same time range as ClickStack |
| Correlated logs | Logs Explorer: exact trace ID and span ID; follow the trace link and inspect related logs from the selected span |
| Host/token gauges | Metrics Explorer: actual `loom.host.*` / `loom.tokens.*` names and units; an absent series is not a measured zero |
| Delivery health | Query the neutral collector's queue, failure and drop series if explicitly collected; backend readiness is insufficient |
| In-progress sweep | Compare partial child spans before the root completes, then query again after completion; do not infer success from a missing root/end span |

Save these searches/dashboards through the installed UI and retain sanitized
exports where supported. These precise steps avoid asserting that mutable
organization-specific dashboard IDs are portable. Loom uses operational spans,
not fabricated HTTP requests. Trace Explorer is the acceptance surface; an empty
HTTP service-map/APM page does not establish a missing trace. Record actual
Community-edition limitations and missing usage separately from measured zeros.

## Retention and operation

Set **seven days for logs, traces and metrics** in General Settings → Retention.
The upstream default for metrics is 30 days, so a fresh render alone does not
establish parity. Verify effective table DDL in ClickHouse after changing the
setting, including derived tables; record it in `evidence.md`. TTL deletion uses
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
