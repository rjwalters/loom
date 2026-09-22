# Fleet gateway collector (SigNoz + ClickHouse dual fanout)

The OTLP landing zone every telemetry source on this host pushes into:
loom-daemon (OTLP exporter), Claude Code (native OTLP), and filelog tails of
the Codex / pi / Claude Code session stores. It authenticates ingress with
the loom ingest key, strips everything outside the `loom.*` allowlist
(privacy transform — conversation content never leaves the host), and fans
every signal out to **two** backends:

- SigNoz Cloud (`otlp_http/signoz`)
- Managed ClickHouse, `loom_otel` database (`clickhouse/managed`)

Upstream context: rjwalters/loom#8522 (trial epic), #8576 (managed-cloud
fanout), #8664 (interactive runtime telemetry), #8665 (cycle-time analytics).

## Layout

| File | Role |
|---|---|
| `config.yaml` | The collector config — receivers (auth'd OTLP + 3 filelog tails), privacy transform allowlist, dual exporters, durable per-sink queues |
| `compose.yaml` | Hardened single-service deployment (read-only rootfs, cap-drop ALL, non-root, localhost-only ports, pinned image digest) |
| `gateway.env.example` | Documents every required host-specific value — values live ONLY in the private `gateway.env` |
| `deploy.sh` | Repo → private sync + compose apply, with backup and `--diff` |

## IaC contract

This directory is the **source of truth**. The deployed copy lives at
`~/.loom/observability/cloud/gateway/` (the conventional operator-private
location — machine-generated config and secrets stay outside Git, per the
same policy the daemon's own ingest key follows) and is what your host's
service manager / LaunchAgent reconciles. Nothing here is host-specific:
endpoints, paths, and the host identity are all `${env:}` placeholders fed
by the private `gateway.env`; credentials are mounted as Docker secrets
from private key files and never appear in any committed file.

To change collector behavior, edit `config.yaml` **here**, then:

```bash
examples/observability-gateway/deploy.sh --diff   # preview vs deployed copy
examples/observability-gateway/deploy.sh          # backup, sync, apply
curl --fail http://127.0.0.1:13133/               # health gate
```

`deploy.sh` refuses to run without a complete `gateway.env`, backs up the
previous config on every deploy, and uses the exact same compose invocation
your reconcile service does, so the two never diverge.

## Endpoints (all localhost-bound)

| Port | Purpose |
|---|---|
| 14318 | OTLP/HTTP ingress (`Authorization: Bearer <loom-ingest-key>`) — daemon + Claude Code export here |
| 13133 | Health check |
| 18888 | Prometheus self-metrics (`otelcol_*` counters) |

## Privacy model

The `transform/privacy` processor keep-keys every signal down to
`service.*`/`host.id` resources plus the allowlisted `loom.*` attributes.
The filelog receivers additionally drop the raw log body after parsing, so
only structured metadata (runtime, model, session/agent ids, event type,
durations) reaches the sinks. Extending the allowlist is a deliberate,
reviewed change to this file — not something a receiver-side operator can
leak around.

## Verification (canary)

```bash
printf '{"type":"canary","sessionId":"canary-<ts>","message":{"model":"m","role":"assistant"}}\n' \
  > "$LOOM_CLAUDE_PROJECTS_DIR/canary-telemetry.jsonl"
sleep 25
curl -s http://127.0.0.1:18888/metrics | grep accepted_log_records
rm "$LOOM_CLAUDE_PROJECTS_DIR/canary-telemetry.jsonl"
# then query ClickHouse: SELECT * FROM loom_otel.otel_logs
#   WHERE LogAttributes['loom.session_id'] = 'canary-<ts>'
```

Both exporter counters (`clickhouse/managed` and `otlp_http/signoz`) must
tick identically.

## Known limits

- filelog tails are `start_at: end` — live-only by design; retroactive
  analysis reads the stores themselves (rjwalters/loom#8664 tracks the
  backfill question).
- ClickHouse tables carry a 168h TTL set at schema creation — trend analysis
  needs the rollup story in rjwalters/loom#8665.
- opencode has no tail-able session store (SQLite); tracked in #8664.
