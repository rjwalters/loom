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
LOOM_CODEX_SESSIONS_DIR=/absolute/home/of/the/interactive/user/.codex/sessions
LOOM_PI_SESSIONS_DIR=/absolute/home/of/the/interactive/user/.pi/agent/sessions
LOOM_CLAUDE_PROJECTS_DIR=/absolute/home/of/the/interactive/user/.claude/projects
```

The three `*_SESSIONS_DIR`/`*_PROJECTS_DIR` variables gate the collector's
read-only session-store bind mounts (#8686). Point each at exactly that one
directory of the user whose interactive sessions you want ingested — never at
`$HOME` itself (SSH keys, browser profiles and other unrelated secrets live
there too). All three are required (`:?`), like the other variables above, so
an environment file that predates them fails `docker compose up` outright
with a named-variable error. **Rolling out on a host that already runs this
trial**: add the three lines to its private env file before the next
`docker compose ... up -d`, or that restart will refuse to start. A missing or
mistakenly empty session directory is safe — the corresponding `file_log/*`
receiver stays idle on an empty glob (verified against the pinned image) —
but the *variable* must still resolve.

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

## Interactive-session filelog receivers

`file_log/codex`, `file_log/pi` and `file_log/claude` tail the on-disk session
stores that interactive runtime sessions write directly, never through the
daemon — see #8664 for why that gap exists. They feed the `logs` pipeline
alongside `otlp` and pass through the same `transform/privacy` allowlist, so
they land in ClickStack/SigNoz the same way daemon-emitted logs do. Unlike
`otlp`, they only ever produce log records; there is no metrics or traces
receiver for interactive sessions here (Claude Code's native OTLP export,
#8668, is the live-metrics complement).

Each receiver's `operators` pipeline: parses the JSONL line's raw text into
`attributes` (`json_parser`, `on_error: send_quiet` — a malformed line still
becomes a mostly-empty record with `loom.runtime` set, it never stalls or
drops the rest of the file), moves a small, named set of fields into `loom.*`
attributes, then **retains only that set and removes `body`**. `retain` is an
explicit allowlist of attribute keys, not a denylist of the ones we happened
to name — anything `json_parser` produced that no `move` operator claimed is
dropped, including raw prompt/response/tool-output content. Clearing `body`
matters independently: `transform/privacy`'s `keep_keys` only inspects
`attributes`, so a filelog receiver that forgot to clear `body` would leak
the entire raw JSON line straight through the allowlist. Both steps were
verified against the real pinned Collector image (see
[Executable contract and evidence](#executable-contract-and-evidence)).

New attributes this issue adds to the shared allowlist (#8669): `loom.session_id`
and `loom.agent_id` / `loom.parent_agent_id` (subagent lineage — an
`agent_change` record's `parentAgentId` is the *immediate* parent at that
record's level, not the root of the chain; walking `agent_id` -> matching
`parent_agent_id` across records reconstructs the full chain regardless of
depth). `loom.runtime`, `loom.provider`, `loom.model` and `loom.duration_sec`
are reused from the existing schema.

Issue #8760 (G3 part 2 / G4 of #8714) adds two more record kinds and six more
allowlisted attributes: `session.analysis`'s `loom.cost_usd`,
`loom.retry_loops`, `loom.longest_tool_call.tool`,
`loom.longest_tool_call.duration_ms`, `loom.anomalies` (all derived from
counts/ids/allowlisted tool names — never transcript content, same as
`session.summary`); and `daemon.event`'s `loom.topic` /
`loom.payload` (the latter is the already-reviewed, small, operator-facing
`daemon.drain.*` / `daemon.capacity.advisory` / `daemon.preflight.advisory` /
`epic.issue.*` event-bus payload, carried whole as one JSON string rather than
re-typed per topic — see `crate::event_bus`'s frozen taxonomy).

| Source | Path tailed (in-container) | `loom.*` fields populated |
| --- | --- | --- |
| Codex | `/var/lib/loom-sessions/codex/**/*.jsonl` | `runtime` (static `"codex"`), `session_id` (from a `session_meta` record), `model` (from a `turn_context` record) — both paths still unverified, no sample existed on the verifying host |
| pi | `/var/lib/loom-sessions/pi/**/*.jsonl` | `runtime` (static `"pi"`), `session_id` (any record carrying `sessionId` — absent from the verified sample), `provider`/`model` (from a `model_change` record — verified 2026-09-23), `agent_id`/`parent_agent_id` (from an `agent_change` record — unverified, no such record in the sample) |
| Claude transcripts | `/var/lib/loom-sessions/claude/**/*.jsonl` | `runtime` (static `"claude"`), `session_id` (verified 2026-09-23), `duration_sec` (`durationMs / 1000` — field absent from all 22,668 real transcripts sampled; guarded no-op until a format carries it) |

**`compose.yaml` bind-mounts each of those three fixed in-container paths
read-only from a host directory named by a required env var (#8686).** They
were deliberately left as fixed in-container paths — not `${env:HOME}/...` —
so wiring a real host directory in is a least-privilege bind of exactly
`~/.codex/sessions`, `~/.pi/agent/sessions` or `~/.claude/projects`, never the
whole home directory (SSH keys, browser profiles and other unrelated secrets
live there too). Each mount is `:ro` and gated behind
`LOOM_CODEX_SESSIONS_DIR` / `LOOM_PI_SESSIONS_DIR` /
`LOOM_CLAUDE_PROJECTS_DIR` (see ["Start and validate"](#start-and-validate)),
so a receiver can read its runtime's session store but never write it, and an
env file that omits one fails `docker compose up` loudly instead of silently
dropping a tail. With the mounts in place the three receivers ingest each
mounted store retroactively from the first line (`start_at: beginning`).

**Field-path verification against real session files (2026-09-23, #8686).**
The paths above were originally assumptions from #8664's field-name hints
(#8669). Verified against real on-disk files on a fleet host — structure
only; no file content is reproduced anywhere:

- **Claude transcripts**: `sessionId` confirmed present on records across
  22,668 real transcript files. `durationMs` was **not present in any of
  them** (0/22,668, key-name scan) — no per-record duration field exists in
  the current format, so `loom.duration_sec` for Claude is derived only if a
  future format adds one; the guarded operator is retained as a no-op on
  current data, not evidence that durations exist.
- **pi**: `model_change` records confirmed carrying top-level `provider` and
  `modelId` exactly as the move operators expect. The available sample (4
  minimal test sessions on that host) contained no `sessionId` field and no
  `agent_change` records at all, so `loom.session_id` and
  `loom.agent_id`/`loom.parent_agent_id` remain **unverified by sample** —
  their guards make them no-ops when the fields are absent, and real
  multi-agent interactive sessions may yet emit them.
- **Codex**: no `~/.codex/sessions` store existed on the verifying host, so
  the `payload.id`/`payload.model` paths remain **entirely unverified**. Capture
  a real Codex session file before relying on its `loom.*` fields.

If a real schema differs from what is verified above, update the affected
move operator's `from:` path and the `retain` list together — never widen
`retain` beyond the table above without a matching addition to
`transform/privacy`'s allowlist. Claude transcripts additionally carry a
`toolUseResult` field and full message bodies; neither is referenced by any
operator here on purpose, since both hold raw tool output / model text
`retain` must never admit.

Because `start_at: beginning` is set (retroactive pickup is the whole point,
per #8664's acceptance criterion 4/5), the very first time a receiver sees a
file it reads the entire thing, not just new lines — on a host with months of
session history that is a real first-run cost. `file_storage/filelog`
checkpoints each receiver's read position so this only happens once per file,
not on every collector restart.

## Privacy and remote deployment

This is a trusted private operational store, not the public dashboard projection.
The gateway's shared allowlist removes unknown resource, record, span-event and
metric attributes before both exporters. Free-text `loom.detail` is excluded.
Only low-cardinality metric labels are permitted; trace IDs and issue numbers
remain in logs/spans. Source-side Loom privacy rules must sanitize bodies, span
names and nested values; this collector is **not** a general arbitrary-log
redactor, and does not accept raw shell output, prompts or model completions.

The one narrow exception is the `file_log/codex`, `file_log/pi` and
`file_log/claude` receivers documented in
["Interactive-session filelog receivers"](#interactive-session-filelog-receivers)
below (#8669): they tail structured JSONL session stores, but never forward
the raw parsed line. Each receiver's own `operators` pipeline moves ONLY a
handful of named fields into `loom.*` attributes and then discards the parsed
body (`remove: body`) before the record leaves the receiver — this is a
second, receiver-level enforcement layer, independent of and prior to the
`transform/privacy` allowlist below. Do not add a filelog receiver, or extend
one of these three, without that same discipline: an operator pipeline that
forwards a raw parsed field (or the body) unfiltered defeats the allowlist
the same way a directly-instrumented free-text body would.

All published ports bind host loopback; the Docker network is private to the
trial, but other attached containers are trusted. For remote ingress, use TLS
with a private CA/mTLS or authenticated reverse proxy and firewall policy; do
not merely replace 127.0.0.1 with 0.0.0.0. Health and metrics endpoints need the
same network protection. Local bearer auth without TLS is only for loopback and
the trusted trial network. Back up private stores and restrict access in both
products; operational correlation IDs can identify repositories.

## Known gap: OpenCode interactive sessions (issue #8670)

**Interactive, human-launched `opencode` sessions do not reach this collector,
by decision, not by oversight.** Everything below was verified against OpenCode
1.18.31 on the telemetry-activation host on 2026-09-22; re-check it before
acting on it.

Only the *interactive* half is missing. Daemon-dispatched OpenCode work is
already on the wire: issue #8507 folds OpenCode session tokens into
`sweep.outcome` / `role_tick.outcome`'s `tokens_by_model`, and
`loom.tokens_by_model`, `loom.models_used`, `loom.runtime`, `loom.provider` and
`loom.model` are already allowlisted in `config.yaml`'s `keep_keys(...)`. A
sweep or role tick that ran on OpenCode is queryable by runtime, model and
duration today.

OpenCode keeps sessions in SQLite (`$XDG_DATA_HOME/opencode/opencode.db`), not
the JSONL session stores the other runtimes use, so the filelog approach #8669
takes for Codex/pi/Claude cannot read it at all. Whatever reaches this collector
for OpenCode must therefore be normalized to `loom.*` attributes *before* it is
sent, which "Privacy and remote deployment" above requires of every source
regardless. Three options were weighed:

- **A native OpenCode exporter** is not this repo's to write. OpenCode is an
  external upstream consumed as the pinned `opencode-ai` npm package under
  `~/.loom/opt/opencode-<ver>/`, with `OPENCODE_DISABLE_AUTOUPDATE=1` baked into
  the worker image; an upstream feature would not reach the fleet until a
  deliberate pin bump. If it ever ships, wiring it is the #8668 shape — an env
  block and a doc, not code here.
- **A Loom-side periodic extract** is tractable and is the chosen design *when
  it is warranted* — see below.
- **Documenting the gap** is what shipped, because there is currently nothing to
  extract: on a host with OpenCode installed since 2026-09-20,
  `~/.local/share/opencode/` held only `log/` and `repos/` — no `opencode.db`.
  Every session row that exists belongs to the Loom-managed store, i.e. work
  already covered by #8507.

### The extract design, if the triggers below fire

`session` carries a stable `id`, `parent_id` (the subagent boundary), `agent`,
a `model` JSON object (`id` + `providerID`), `directory`, and both
`time_created` and `time_updated`. The non-obvious constraint is that **session
rows are mutable running totals, not append-only events** — all four rows in the
store read on 2026-09-22 had `time_updated > time_created`. So:

- dedup **last-value-wins keyed on `session.id`**, watermarked on
  `time_updated`; a `time_created` cursor would freeze each session at its
  first-seen counters and silently undercount every turn that followed.
- do **not** reach for the `event(aggregate_id, seq, …)` table, whose
  `UNIQUE(aggregate_id, seq)` index is the textbook monotonic watermark: its
  `data` column is raw prompt and completion text, which this pipeline forbids.
- reuse `loom-daemon/src/opencode_usage.rs` and keep its invariant — one query
  naming `session` alone, opened read-only — because the same database file also
  holds `credential` and `account` tables, and `session` itself carries
  free-text `title`, `metadata` and `summary_diffs` that must never be
  projected.
- extend that module's discovery to `$XDG_DATA_HOME/opencode/opencode.db`; it
  scans only the Loom-managed `~/.loom/opt/opencode-<ver>/` stores today.

**Re-open #8670 when both hold**: (1) an `opencode.db` appears at
`$XDG_DATA_HOME/opencode/` on a fleet host carrying sessions not attributable to
a Loom launch — real interactive usage exists; and (2) issue #8669's
`loom.session_id` / `loom.agent_id` / `loom.parent_agent_id` allowlist entries
have merged, so the extract has a settled attribute schema to match rather than
a parallel one to invent.

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

The interactive-session filelog receivers (#8669) have their own opt-in
contract, run the same way:

```console
CARGO_BUILD_JOBS=2 cargo test -p loom-daemon --test collector_filelog -- --ignored --nocapture
```

It runs the production config verbatim (only the two OTLP-HTTP exporters are
swapped for a local `file` sink), mounts synthetic Codex/pi/Claude session
fixtures — including a malformed line and a three-level `agent_change`
chain — at the three fixed `/var/lib/loom-sessions/<source>` paths, and
asserts every source-specific sentinel string is absent from the exported
output while the allowlisted `loom.*` fields (including multi-level
`parent_agent_id` lineage and the Claude `durationMs -> duration_sec`
conversion) are present. A second, non-Docker test in the same file
statically checks that no filelog receiver's `retain` list, nor
`transform/privacy`'s allowlist, admits a key outside the reviewed set —
that one runs under plain `cargo test -p loom-daemon`.

Upstream contracts: [Collector fan-out architecture](https://opentelemetry.io/docs/collector/architecture/),
[persistent queues and retries](https://github.com/open-telemetry/opentelemetry-collector/blob/v0.161.0/exporter/exporterhelper/README.md),
[bearer authenticator](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/v0.161.0/extension/bearertokenauthextension/README.md),
[file storage](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/v0.161.0/extension/storage/filestorage/README.md).
