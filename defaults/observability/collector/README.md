# Loom telemetry gateway

This opt-in trial sends each OTLP log, metric and span to **both** ClickStack
(HyperDX UI) and SigNoz. The gateway speaks OTLP HTTP to each product's own
Collector; their ClickHouse schemas remain independent. It does not accept
Loom's native Cloudflare JSON envelopes.

## Start and validate

Use the sibling ClickStack and SigNoz recipes first. All three deployments join
the external Docker network `loom-observability`. Create it once with
`docker network create loom-observability` if it does not exist. The backend DNS
names are `clickstack-collector:4318` and `signoz-otel-collector:4318`.

Keep state, keys and the environment file outside tracked directories, for
example under `$HOME/.local/state/loom-observability-trial/`. Provision a new
random Loom ingestion key in a mode-0600 file. This is a telemetry credential;
**never use ZAI_API_KEY or another inference credential**. Copy ClickStack's raw
ingestion API key into another mode-0600 file. The keys must differ. Do not put
keys in command arguments or run `docker compose config` with secret-valued env
substitution. This recipe passes paths and mounts files, not secret values.

Set these path/identity variables in a private environment file:

```dotenv
LOOM_COLLECTOR_UID=1000
LOOM_COLLECTOR_GID=1000
LOOM_COLLECTOR_STATE_DIR=/absolute/private/trial/collector-state
LOOM_COLLECTOR_INGEST_KEY_FILE=/absolute/private/trial/loom-ingest.key
LOOM_CLICKSTACK_INGEST_KEY_FILE=/absolute/private/trial/clickstack-ingest.key
```

Use the host user's actual numeric UID/GID; create the state directory owned by
that identity (0700) and ensure it can read the mounted keys. The image has no
shell. Compose does not create missing secret files. Validate and start from
this directory, replacing the environment-file path:

```console
docker compose --env-file /absolute/private/trial/gateway.env --profile trial run --rm collector validate --config=/etc/otelcol/config.yaml
docker compose --env-file /absolute/private/trial/gateway.env --profile trial up -d
```

There are no services in the default profile, so this recipe never starts during
a normal Loom install. The pinned contrib 0.161.0 image includes OTLP HTTP,
bearertokenauth, file_storage, transform, memory_limiter and health_check. Updating
the digest requires rerunning the Rust Docker contract tests.

Host ingress is `http://127.0.0.1:14318`; authenticate with `Authorization:
Bearer <Loom key>`. A daemon in another container must use gateway service DNS
and container port 4318, not host loopback. ClickStack egress independently uses
its **raw** `authorization` key through a separate file-backed authenticator
with an empty scheme. It trims LF/CRLF key-file endings; direct file-provider
interpolation into headers does not. Both authenticators wait for readable key
files and fail startup after a five-second retry budget. SigNoz community egress is unauthenticated on the
private Docker network. Managed SigNoz requires a separate TLS endpoint and
`signoz-ingestion-key` secret; change a local config copy and validate it. Never
forward incoming client authorization to either backend.

## Canary and rollback

First check that your `loom-daemon` was built with the `otlp` feature. Set these
values in `.loom-local/local.json` or the equivalent `LOOM_OBSERVABILITY_*`
environment variables for the intended canary daemon:

```json
{
  "observability": {
    "enabled": true,
    "exporter": "otlp",
    "endpoint": "http://127.0.0.1:14318",
    "ingestKeyFile": "/absolute/private/trial/loom-ingest.key"
  }
}
```

The daemon selects **one** exporter. Record its previous configuration first;
this selection replaces the native HTTPS sink for that canary. It does not
preserve a second native dashboard feed. Restart the canary as its normal
service manager requires, inspect export health, then query both backend stores
for the same trace ID, log identity and metric sample. HTTP 200 alone does not
prove indexing or UI correlation. Real Loom traces additionally require the
trace context/instrumentation implementation; synthetic spans are transport
validation only.

To roll back, restore the saved daemon settings and restart it. Stop the gateway
with `docker compose --env-file /absolute/private/trial/gateway.env --profile
trial stop`. `down` removes only this Compose project's container; bind-mounted
queue state remains. Keep the product stores and evidence. Do not delete state,
run global Docker cleanup, or remove the shared network while another trial
uses it.

## Delivery limits and diagnostics

Ingress requests are capped at 1 MiB. Loom's default 50-record batches are the
batching boundary: there is deliberately no asynchronous batch processor ahead
of disk queues, which would acknowledge volatile in-memory data. Each destination
has three persistent queues, one per signal, each holding **1,000 requests** and
two active consumers. Stores are separate files/directories, fsync is enabled,
and retries configure a 1-second initial / 10-second maximum base backoff
(Collector jitter varies actual waits), with a 600-second retry limit. Graceful stop
has 30 seconds; abrupt restart resumes persisted items. Delivery is at least
once: a lost acknowledgement or one full destination queue can cause upstream
retries to duplicate already accepted data at the other destination.

Supported initial outage envelope: at most one request/second/**signal**, each
at most 64 KiB decoded, for a five-minute single-backend outage. This is a sizing
budget, not an infinite isolation guarantee. Queue capacity and 600-second retry
limit provide headroom for the 300 queued requests; validate representative
production throughput before expanding it. At that load allow at least 512 MiB
free disk per backend plus operational headroom. The worst configured logical
capacity at 1-MiB ingress is roughly 3 GiB per backend before protobuf expansion,
indexes and filesystem overhead; provision **8 GiB** for trial queue storage and
alert well before full. Queue capacity is not a disk quota. Collector memory is
limited to 384 MiB (96-MiB spike reserve) inside a 512-MiB container; container CPU
is capped at one. Retained bbolt allocations may need offline compaction after
large incidents; never delete live queue files to recover disk space.

Inspect these independently of either backend:

- `http://127.0.0.1:13133/`: process/pipeline health, **not** proof of delivery.
- `http://127.0.0.1:18888/metrics`: `otelcol_receiver_accepted_*`,
  `otelcol_receiver_refused_*`, `otelcol_exporter_sent_*`,
  `otelcol_exporter_send_failed_*`, `otelcol_exporter_enqueue_failed_*`,
  `otelcol_exporter_queue_size`, `otelcol_exporter_queue_capacity`. Filter exporter
  labels `otlp_http/clickstack` and `otlp_http/signoz`; inspect per-signal counters.
- `docker compose ... logs collector`: retry exhaustion, permanent rejection,
  queue/storage errors. Info logging avoids payload dumps; never enable OTTL
  debug logging with private data.
- Host disk usage/free-space and container memory: persistent queues cannot
  protect against a full filesystem, deleted volume or exhausted host memory.

A stopped backend should grow only its own queue while the other receives new
records. Once queues fill or retry deadlines expire, losses/rejections are
visible and upstream can retry; no indefinite cross-destination isolation is
promised. Wrong backend credentials are typically permanent 401/403 failures
and can discard queued records; fix credentials before enabling a real canary.
An absent exporter endpoint fails Collector configuration validation, but an
unreachable configured backend starts with queues/retries so outages are
recoverable. Both exporter IDs are explicitly required by all three pipelines.

## Privacy and remote deployment

This is a trusted private operational store, not the public dashboard projection.
The gateway's shared allowlist removes unknown resource, record, span-event and
metric attributes before both exporters. Free-text `loom.detail` is excluded.
Only low-cardinality metric labels are permitted; trace IDs and issue numbers
remain in logs/spans. Source-side Loom privacy rules must sanitize bodies, span
names and nested values; this collector is **not** a general arbitrary-log
redactor. Do not add filelog, shell output, prompts or model completions.

All published ports bind host loopback; the Docker network is private to the
trial, but other attached containers are trusted. For remote ingress, use TLS
with a private CA/mTLS or authenticated reverse proxy and firewall policy; do
not merely replace 127.0.0.1 with 0.0.0.0. Health and metrics endpoints need the
same network protection. Local bearer auth without TLS is only for loopback and
the trusted trial network. Back up private stores and restrict access in both
products; operational correlation IDs can identify repositories.

## Executable contract and evidence

From the repository root:

```console
CARGO_BUILD_JOBS=2 cargo test -p loom-daemon --test collector_fanout -- --ignored --nocapture --test-threads=1
```

For this configuration-only contract, the test also builds directly with the
Rust standard library (Docker and curl must be installed), avoiding a cold daemon
build:

```console
rustc --edition 2021 --test loom-daemon/tests/collector_fanout.rs -o /tmp/loom-collector-fanout-tests
/tmp/loom-collector-fanout-tests --ignored --nocapture --test-threads=1
```

The Rust test starts uniquely named containers/network with the pinned real Collector
and two independent OTLP sinks. It checks all three signals, identities/times,
authentication, allowlist behavior, single-destination outages and abrupt gateway
restart. It also fills a private 1-MiB tmpfs to exercise real storage-write
errors, saturates a bounded queue, expires retries and rejects bad downstream
credentials without logging their values. It cleans up only its own containers/network, including on assertion
failure. Synthetic sink evidence does not replace the ClickStack/SigNoz product
query and UI acceptance in issues #8527, #8528 and #8529.

Upstream contracts: [Collector fan-out architecture](https://opentelemetry.io/docs/collector/architecture/),
[persistent queues and retries](https://github.com/open-telemetry/opentelemetry-collector/blob/v0.161.0/exporter/exporterhelper/README.md),
[bearer authenticator](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/v0.161.0/extension/bearertokenauthextension/README.md),
[file storage](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/v0.161.0/extension/storage/filestorage/README.md).
