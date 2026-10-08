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
**v0.142.1**, its collector **v0.144.10**, ClickHouse **25.12.11**, Keeper **25.12.5**,
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
| Every ClickHouse `system.*_log` keeps a `DELETE` TTL, and `metric_log` uses the transposed schema, set by the casting's declarative patch | The wide upstream `metric_log`'s TTL merge does not fit the 2 GiB cap, so it silently stops expiring (see "ClickHouse self-telemetry" below) |

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

### Queue dwell and starvation queries

`queue-dwell.sql` (#8856) answers "is ready work waiting, and why?" from the
`loom.queue.*` `metric.points` family plus, for query 5, the
`loom.dispatch.disposition` / `loom.dispatch.admission` spans (#9222): (1)
starved issues per host/state over the last 24h — the same signal
`alerts/queue-starvation.json` alerts on; (2) why they are held, by
disposition reason; (3) the oldest waiting issue per host/state, hourly peak
over 7 days; (4) mean dispatch wait per host per day over 30 days; (5) the
latest disposition/admission trail for one `owner/repo#issue`. Run it the same
way as the fixture queries above:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < queue-dwell.sql
```

`alerts/queue-starvation.json` is a saved SigNoz alert rule (Alerts → New
alert → Import), not a `.sql` file — import it directly rather than
re-transcribing its embedded query. Both it and `queue-dwell.sql` are
vocabulary-guarded by `signoz_trial_artifacts.rs` exactly like the fixture
queries above: a `metric_name` or label the emitting code no longer produces,
or the gateway's DATAPOINT `keep_keys` allowlist no longer forwards, fails
ordinary CI instead of the saved view quietly going empty.

The alert additionally has its own execution proof,
`loom-daemon/tests/signoz_queue_starvation_alert.rs`, which substitutes
SigNoz's `{{.start_timestamp_ms}}` / `{{.end_timestamp_ms}}` the way its rule
evaluator does and then applies the committed `op` / `matchType` / `target` to
the engine's own output. A wrong alert fails differently from a wrong
dashboard: an empty dashboard is *visibly* empty, whereas an alert whose query
returns nothing — or rows its threshold can never cross — is **silently**
healthy forever, because no page is exactly what a working queue looks like.
What the run establishes:

- **The threshold separates sustained starvation from a working queue.** A host
  above zero in every minute fires; a host reporting a measured zero in every
  minute does not; and a host that starves and clears alternately does not
  either, because the committed `matchType` demands the breach hold for the
  whole 15-minute window. The test evaluates both of SigNoz's point-wise match
  semantics over the same output, so switching that field is visible as a
  change of operational contract rather than a tweak.
- **The zero rows stay in the result.** Unlike `queue-dwell.sql` query 1, the
  alert has no `HAVING starved > 0` — SigNoz needs the zero-valued points to
  see a series recover — so a measured zero is dropped by the *threshold*, and
  nothing in the SQL would stop a loosened `target` from paging on every
  reporting host.
- **The window is half-open.** The point at exactly `end_ms` is excluded, so two
  consecutive overlapping evaluations cannot both count the same boundary
  point, and a spike one minute before `start_ms` does not re-alert.
- **`GROUP BY ts, host` is what makes the annotation's "see the alert's host
  label" true.** Dropping `host` collapses every host into one unattributable
  series whose per-bucket `max()` is the worst host's — still firing, but with
  nowhere to point and a healthy host hidden rather than visibly healthy.

Each of those is run with the committed JSON mutated as a counterfactual, and
the rule's own cadence is checked too (`frequency` ≤ `evalWindow`, so no minute
of starvation falls in a gap between evaluations; neither the rule nor its
query ships `disabled`). What it does **not** establish: no rule evaluator ran
and no notification was delivered — the live fire-and-resolve check is
[#9006](https://github.com/rjwalters/loom/issues/9006).

All five queries are additionally **executed verbatim** against the pinned
ClickHouse the telemetry store runs, by
`loom-daemon/tests/signoz_queue_quota_queries.rs`. That run is what establishes
the two properties this family is easiest to get wrong, both named in the file's
own header:

- **`time_series_v4` holds one row per series per hour**, so `USING
  (fingerprint)` multiplies every data point by that series' hour-row count.
  Queries 1–3 take `max()` and absorb it; query 4 sums and must join a
  de-duplicated fingerprint set. The test runs the naive join as a
  counterfactual and observes it double query 4's wait-seconds total.
- **A measured zero is not starvation.** A host reporting
  `loom.queue.starved` = 0 is dropped by `HAVING starved > 0` rather than
  reported as a starved host with a zero; and a `loom.queue.dispatch_wait`
  with no `.samples` companion series yields a NULL mean through
  `nullIf(dispatches, 0)`, never 0 and never a division error.

Query 5's span reads are covered too, including the documented fallback: a
pre-#9673 halted row has no `loom.queue.halt_cause` key at all, the column
reads `''` rather than erroring, and joining to the parent
`loom.dispatch.tick` recovers `halted_main_red`. Not yet executed against a
live SigNoz over real canary data; see `evidence.md`'s acceptance ledger.

### Quota utilization queries

`quota-utilization.sql` (#9005) answers "are our subscriptions saturated or
idle?" from the per-account `tokens.snapshot` gauges: (1) per-account 5-hour
and weekly utilization, hourly peak; (2) idle headroom at each detected weekly
reset (the final utilization before the window rolled over, and `1 -` that);
(3) the fraction of subscription capacity each provider used over the last
week, with `coverage = 'unknown'` and NULL fractions for a provider that has
no utilization source. Run it the same way as the queue-dwell queries above,
and note the same `signoz_trial_artifacts.rs` vocabulary guard applies:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < quota-utilization.sql
```

All three queries are **executed verbatim** against the pinned ClickHouse the
telemetry store runs, by `loom-daemon/tests/signoz_queue_quota_queries.rs` —
which replaces an earlier ad-hoc `clickhouse-local` session whose fixture and
output were never committed, so nothing re-ran it. The run is what establishes
the file's absent-is-not-zero claims as observations:

- An account with a weekly reading but no 5-hour reading comes back
  `util_5h = NULL`. The plain `maxIf()` the header warns about answers `0.0`
  instead — "used none of its 5h window", a measurement never taken.
- A pre-reset reading above 1.0 clamps to **zero** idle headroom via
  `least(prev_value, 1)`; without the clamp it reads `-0.05`.
- A provider whose accounts report only `loom.tokens.exhausted` (no
  utilization source at all) still appears, with `accounts_measured = 0`,
  `coverage = 'unknown'` and NULL fractions. The query 3 `pool` LEFT JOIN is
  load-bearing for this: an inner join drops the provider's row entirely, and
  a `0`/`1.0` there would read as a completely idle subscription with free
  capacity to dispatch into — the exact inversion of an exhausted account.

Not yet executed against a live SigNoz over real canary data.

### GitHub shadow-spend reconciliation

`github-shadow.sql` (#10343) answers "how much of what GitHub billed each App
installation's bucket did Loom attribute?": (0) a preflight that the two
metric families exist and how SigNoz stored `loom.forge.calls` (its `sumIf`
is only right while `temporality` reads Delta); (1) GitHub's own bill per
`(account, owner, resource)` hour — Σ positive increments of
`github.ratelimit.used` readings keyed by their quota window (the paired
`github.ratelimit.reset`, resets within 2 s merged), each window charged its
high-water mark once — a stale reading from another host or (on a
pre-#10571 daemon's data) an interleaved second bucket never re-charges; (2) the requests `loom.forge.calls` attributed to that bucket, `ok`
(surely charged) and `ok`+`error` (an upper bound), with 304s and the free
probe excluded; (3) the shadow band `shadow_low`/`shadow_high` per
bucket-hour, NULL when GitHub reported no spend, plus `agent_share`; (4) the `invoke github` span
cross-check by `github.account`, `github.cred_owner`, `github.resource` and
`github.billing`; (5) the agent slice (#10607): agent sessions' `gh` calls
(`agent != '-'`) by role and served/passthrough. Run it like the queries above:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < github-shadow.sql
```

Its metric names, metric labels and span attributes are guarded by
`signoz_trial_artifacts.rs`, and all five queries are **executed verbatim**
against the pinned ClickHouse by `signoz_github_shadow_queries.rs` (stale
host readings, a genuine reset, 1 s reset jitter, two interleaved windows, an
owner-less legacy point). Queries 0–4 have also been run read-only against the
live store (PR #10565); the one-hour 10 % reconciliation is #10343's Slice 3.

### Pass activity

`pass-queries.sql` (#10752) answers "what did the daemon's hold-cleanup pass
do?" from the `pass.summary` / `pass.verdict` logs and the `invoke github`
spans stamped with `github.caller`: (1) per mechanism and repo over 24 h, the
passes run and refused, blocks released and re-parked, skips by reason, write
cap hits and the GitHub calls by operation, in one statement; (2) the newest
verdict per artifact, which says why each one is still held; (3) every
`loom:blocked` removal the loom-ui webhook export saw, matched to the pass
verdict that made it (empty when no pass did). Run it like the queries above:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < pass-queries.sql
```

Its keys are guarded by `telemetry/kinds/pass_tests.rs`, and the OTLP mapping
tests check each key is read from the attribute map its type lands in. All
three statements were run read-only against the live store on 2026-10-07,
before any daemon shipped the records: (1) and (2) returned no rows, and (3) matched 109 removals in 24 h,
none of them attributed yet.

### Measured usage queries

`usage-queries.sql` (#8528) is the SigNoz half of ClickStack's "Loom measured
usage" view: what a run's tokens and dollars actually were, from the
`loom.runtime.usage` spans (#8908, #9204, #9303) rather than from the token
gauges. **The two are different questions and must not be mixed** — a
`loom.tokens.usage_fraction` gauge says how much of a subscription's weekly
window is left (`quota-utilization.sql`), and a pool percentage never becomes
dollars. Section 0 is the arrival preflight; sections 1–6 are the views in the
table below. Bind all three parameters once:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery --param_since='2026-09-01 00:00:00' --param_repo='' --param_top=50 < usage-queries.sql
```

Five properties of this family make it easy to query wrongly, each returning a
plausible number instead of an error. The file's header states them and
`loom-daemon/tests/signoz_usage_queries.rs` **demonstrates** each one by
executing the committed SQL verbatim against the same pinned ClickHouse
(25.12.5) the telemetry store runs:

- **Every span attribute is a string.** The trace mapper renders the whole
  attribute map with `kv_string`, so the token counts and the USD estimate land
  in `attributes_string` — unlike the CI *log* records, where `loom.ci.run_id`
  genuinely is an int in `attributes_number`. The test's negative control reads
  a counter from `attributes_number` and observes the failure mode: 0 on every
  row, no error.
- **Scope is not additive.** A daemon-dispatched sweep can carry both an
  `execution` span and `attempt` spans whose counters overlap; total from
  `execution` when present, else from `attempt`, never both.
- **An unpriced model carries no cost attributes at all**, so `sum()` skips it
  and a spend total is a lower bound. Every section that sums dollars reports
  `unpriced_spans` beside it, and section 4 names the models.
- **Absence is not zero.** A unit whose usage could not be determined has no
  usage span; one measured at nothing has a span whose counter is `"0"`.
- **`loom.repo` is not on the usage span** (no caller puts it there, and it is
  not a resource attribute either), so repo attribution is a join across the
  trace.

`signoz_trial_artifacts.rs` additionally fails in ordinary CI if any counter,
cost, pricing or scope key drifts from `counter_attributes()` /
`Pricing::attributes()`, if a span name drifts from `SpanName`, or if any key is
read from a container other than `attributes_string`.

Executed against the pinned ClickHouse with a synthetic fixture, not yet against
a live SigNoz over real canary data.

### CI retro queries

`ci-queries.sql` is the standing build/CI retro (#8826): numbered sections over
what `loom-daemon ci-telemetry` captures — duration trend, regression
spotlight, outcome mix, top slow jobs, failed run → logs, run waterfall, the
per-issue ship breakdown, CI time per trigger reason, and (#9089) job queue
wait, shard imbalance, step timings, slowest suites, suite-level rebalance and
dependency wait.
Sections 1–3 read the `loom.ci.*.duration_ms` histograms (trial: 30 days);
4–10, 14 and 15 read the `ci.run` / `ci.job` / `ci.job.log` records (trial: 7
days); 11–13 read `signoz_traces.signoz_index_v3` (trial: 7 days; live: 10 years). Bind all five parameters
once:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery --param_since='2026-09-01 00:00:00' --param_repo='' --param_bucket_hours=24 --param_window_hours=168 --param_top=20 < ci-queries.sql
```

The same `signoz_trial_artifacts.rs` guards it more strictly than the fixture
queries: every log attribute it reads must be in the gateway's **log**
`keep_keys` *and* be read from the SigNoz map column matching the type the
daemon sends it as (`loom.ci.run_id` is an int, so `attributes_number`), every
metric label must be a CI histogram label the **datapoint** allowlist keeps,
and every metric series must be a `.sum` / `.count` series of the two CI
histograms. The trace-reading sections (11–13) are guarded separately: their
attributes must be in the gateway's **span** `keep_keys`, and must be read from
`attributes_string` only — every OTLP span attribute is a string regardless of
the type it names, so a subscript into `attributes_number` there would silently
return 0 on every row instead of erroring. Policy and pipeline:
[CI observability](../../docs/ci-observability.md).

### ETA accuracy queries

`eta-queries.sql` (#9289) scores the ETA heuristics against what actually
happened, from the `eta.estimate` / `eta.outcome` **log** records. Section 0 is
the preflight; Q1 is MAE / 25–75 coverage / bias; Q2 is the mean pinball loss
that decides a promotion; Q3 ranks the recorded features by how well each
tracks the error; Q4-Q7 (#10233) are the late-surprise rate on the common
decidable subset, the stability of the predicted landing instant, interval
convergence by actual lead, and the time-weighted answer rate (proven by
`loom-daemon/tests/signoz_eta_accuracy_views.rs`). The model is [`eta.md`](../../docs/eta.md).
Every Loom log record, the `eta.*` kinds included, carries `loom.kind` (its
record kind, #9881); before #10899 the `eta.*` kinds did not, which is why a
`loom.kind LIKE 'eta%'` query used to find nothing. These queries filter on
`loom.eta.*` keys instead, which stays valid for old and new rows alike. Bind
both parameters:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery --param_since='2026-09-01 00:00:00' --param_repo='' < eta-queries.sql
```

`loom-daemon/tests/signoz_eta_queries.rs` executes the committed file verbatim
against the same pinned ClickHouse (25.12.5) the telemetry store runs, and runs
the mutation of the committed SQL that breaks each claim below as a
counterfactual rather than describing it. **Read section 0 first**, then these
five, in this order:

- **Absent is never zero.** An `abandoned` outcome (the issue closed as not
  planned) and a refusal carry no `loom.eta.error_sec` key at all. Dropping the
  `mapContains` presence filter does not error — `attributes_number` answers the
  Float64 default — it scores the abandonment as a *flawless* prediction, which
  **lowers** the MAE. Measured: 1048 → 1000 on the test fixture. A promotion
  metric that improves when data goes missing is the worst shape of silent
  defect, which is why section 0 reports `outcomes` and `scored` separately and
  they must reconcile to `abandoned`.
- **`refusals` is a subset of `estimates`**, not a disjoint population — a
  refusal still carries `loom.eta.trigger`. Adding the two columns double-counts.
- **Q2's subtotals need `rolled_up`.** `ROLLUP` blanks an aggregated column to
  the type's default, `''` for these Strings — exactly what
  `attributes_string['loom.eta.heuristic']` answers for a record that lacks the
  key. The query therefore can and does emit **two rows with the identical key
  `('', '', '')`**: the grand total (`rolled_up` = 3) and an unlabelled
  heuristic's own total (`rolled_up` = 2). Without the column they are one
  unreadable pair.
- **Q3: an unvarying feature is not scored as uncorrelated.** `rankCorr`
  average-ranks ties, so a feature that took one value across the window comes
  out at exactly **`rank_corr` = 0.5** — above any genuine correlation weaker
  than that, under this section's own `ORDER BY abs(rank_corr) DESC` — while
  `pearson_corr` reads NaN. `distinct_values` = 1 is the tell. Do not read a
  0.5 without it. The NaN's **spelling** is architecture-dependent (`nan` on
  arm64, `-nan` on amd64 Linux), so never string-match it.
- **Q3 cannot see an estimate older than the window.** Its estimate sub-select
  carries the same `since` bound as the outcome one, so an outcome whose
  estimate predates `since` is scored by Q1/Q2 and contributes no features here.
  Widen `since` past the longest lead time you care about.

`HAVING n >= 20` means a short window makes Q3 return **nothing**, which reads
like "no feature tracks the error" rather than "not enough data" — check section
0's counts before concluding either. `eta_artifacts.rs` additionally fails in
ordinary CI if any attribute drifts from what the ETA mapping emits and the
gateway forwards, or if either disambiguating column is dropped.

Executed against the pinned ClickHouse with a synthetic fixture, and executed
verbatim against the **live trial deployment** (ClickHouse 25.12.5.44,
2026-10-01) where it returned all four sections' documented columns over zero
rows — no ETA record has been ingested there yet. Not yet run over real canary
data.

### Saved views

| Saved view | Procedure |
| --- | --- |
| Loom issue waterfall | Trace Explorer: filter the observed trace ID, open the trace, inspect parent IDs and failed Judge → Doctor → successful Judge attempts (`fixture-queries.sql` 2 and 4) |
| Failed attempts | Trace Explorer: error status on `loom.role_attempt`, group by observed `loom.repo`, `loom.role`, `loom.runtime` and `loom.model`; keep missing labels missing (`fixture-queries.sql` 3 and 8) |
| Phase duration | Trace Explorer: duration of the observed `loom.phase` / `loom.role_attempt` spans, grouped by role/runtime/model, with the same time range as ClickStack (`fixture-queries.sql` 3) |
| Correlated logs | Logs Explorer: exact trace ID and span ID; follow the trace link and inspect related logs from the selected span (`fixture-queries.sql` 6) |
| Host/token gauges | Metrics Explorer: the actual emitted names and units — the shared fixture emits `loom.tokens.usage_fraction` and `loom.tokens.exhausted` only, labelled by `account`. An absent series is not a measured zero: the fixture's `synthetic-unknown` account intentionally has no `usage_fraction` point while `synthetic-zero` has `0.0` (`fixture-queries.sql` 7) |
| Subscription quota utilization | Dashboards → New dashboard `Loom quota` → Time series panel. Metric `loom.tokens.usage_fraction` (5-hour window) and a second query on `loom.tokens.usage_fraction_weekly` (rolling 7-day window, #9005), aggregation **Max**, group by `provider`, `account`; time range 7 days. Providers with no utilization source (Codex, OpenCode/Z.ai, Kimi) have no series at all — a gap, never a `0`. Last week's used fraction per provider and the idle headroom thrown away at each weekly reset need window functions, so they live in SQL only (`quota-utilization.sql` 1–3) |
| GitHub bucket shadow spend | Dashboards → `Loom quota` → Time series panel. Metric `github.ratelimit.used`, aggregation **Max**, group by `account`, `owner`, `resource` (`owner = '-'` is an operator's ambient login; a pre-#10571 daemon's label set can carry two interleaved windows, so the panel is a browsing surface). The per-hour increments, the attributed `loom.forge.calls` and the shadow band need window functions and a join, so they live in SQL only (`github-shadow.sql` 1–5) |
| Ready-queue dwell | Dashboards → New dashboard `Loom queue` → Time series panel. Metric `loom.queue.oldest_wait`, aggregation **Max**, group by `host.id`, `state`; time range 7 days. A second panel on `loom.queue.starved` (Max, group by `host.id`, `state`) is the alert's own signal. Disposition reasons, mean dispatch wait and one issue's admission trail need `JSONExtractString`/span reads and stay SQL-only (`queue-dwell.sql` 1–5) |
| Queue starvation alert | Alerts → Import `alerts/queue-starvation.json`. Fires when `loom.queue.starved` for `state = 'ready'` stays above threshold for the 15-minute eval window on any one host; the alert's own query is `queue-dwell.sql` 1 narrowed to that state, minus its `HAVING starved > 0` so SigNoz can still see the series recover. The embedded query plus the committed threshold are executed against the pinned ClickHouse by `signoz_queue_starvation_alert.rs` (see "Queue dwell and starvation queries" above); no rule evaluator or notification has run ([#9006](https://github.com/rjwalters/loom/issues/9006)) |
| Host disk alerts | Alerts → Import `alerts/host-disk-low.json` (warning) and `alerts/host-disk-critical.json` (critical). Both read `host.health` rows that reach SigNoz through the loom-ui d1-export (`service.name = loom-ui-d1-export`, body `kind = host.health`), per host, over a 10-minute window where every record must breach (each minute bucket takes the `min` of the breach predicate, so one healthy reading clears that minute). Warning: `worktree_root_free_gb` < 30 or < 10% of `worktree_root_total_gb`; critical: < 5 (so 0 included) or < 3%. The percent arm needs `worktree_root_total_gb`; a host that does not report it is judged on the absolute GB arm alone, and a record without `worktree_root_free_gb` is unknown, never 0 GB. Once #10934 rolls out these can key on the direct OTLP `loom.host.worktree_root_free_gb` instead. Executed against the pinned ClickHouse over a replay of free GB falling to 0, and of a host alternating 0 GB and recovered readings within each minute (must not page), by `signoz_ops_alerts.rs`; no rule evaluator or notification has run (#10973) |
| ETA Ready-coverage alert | Alerts → Import `alerts/eta-ready-coverage.json`. Over a 30-minute window, fires when any 10-minute bucket of a host's `ready_wait` `eta.estimate` rows (`loom.eta.stage`) has zero answered and at least half `stale_inputs` refusals. Each ready item produces a record, so rows existing means ready items exist; total silence is the separate no-ETAs-emitted alert (#10898). Because refusals are currently emitted once (item 4 of #10973 will re-emit them), this fires for the window after the refusals begin and then clears. Replay-tested by `signoz_ops_alerts.rs` (#10973) |
| Loom measured usage | Trace Explorer: filter `name = 'loom.runtime.usage'`, group by `loom.model` with **Sum** over the token counters, and separately by `loom.role` / `loom.runtime`. The UI reads these attributes as STRINGS (they are exported as strings, like every span attribute), so a numeric aggregation of them belongs in SQL — and the scope resolution a correct total needs cannot be expressed as an Explorer filter at all. Treat the panel as a browsing surface and the SQL as the figures (`usage-queries.sql` 1 and 2) |
| Usage coverage | Trace Explorer: filter `name = 'loom.role_attempt'` and compare against the usage spans beneath each. An attempt with **no** `loom.runtime.usage` child has usage UNKNOWN; one whose child reports `loom.tokens.total = '0'` is a measured zero. Never impute one from the other — the split is SQL-only (`usage-queries.sql` 3) |
| Unpriced models and rate-card provenance | Trace Explorer: filter `name = 'loom.runtime.usage'` and add `loom.cost.usd_estimate`, `loom.pricing.source`, `loom.pricing.verified_on` as columns. A blank estimate is a model the rate card does not know, never a $0 model: it must be excluded from spend explicitly, and fixing it is a rate-card change, not a query change. Two `verified_on` values in one window mean the fleet rolled a card mid-window (`usage-queries.sql` 4 and 5) |
| Cache composition | Trace Explorer: filter `name = 'loom.runtime.usage'` and add `loom.tokens.input`, `loom.tokens.cache_read`, `loom.tokens.cache_write_5m`, `loom.tokens.cache_write_1h`. The four are DISJOINT — `input` is uncached input, so input-side total is their sum — and the two cache-write horizons stay apart because they are priced differently (`usage-queries.sql` 6) |
| Delivery health | Scrape the neutral gateway's own Prometheus endpoint (`config.yaml` publishes `detailed` telemetry on port 8888) and read `otelcol_exporter_*` series filtered to `exporter="otlp_http/signoz"` — queue size, sent, send-failed and enqueue-failed. These series are **not** exported into SigNoz through the OTLP pipeline, so they are unavailable in the UI and must be captured beside it. Backend readiness is not delivery evidence |
| In-progress sweep | Compare partial child spans before the root completes, then query again after completion; do not infer success from a missing root/end span (`fixture-queries.sql` 5, which lists every trace with children but no `loom.sweep` root) |
| CI duration trend | Dashboards → New dashboard `Loom CI` → Time series panel. Metric `loom.ci.job.duration_ms`, aggregation **P50**, a second query on the same metric with **P95** and a third with **Max**; group by `repo`, `workflow`, `job`; optional filter `repo = '<owner/name>'`; time range 30 days. UI percentiles interpolate within the histogram's 1s…6h bucket bounds; the SQL is exact (`ci-queries.sql` 1) |
| CI regression spotlight | Same dashboard → Table panel. Metric `loom.ci.job.duration_ms`, aggregation **P95**, group by `repo`, `workflow`, `job`, time range = the current window; duplicate the panel with the time shift/compare set to one window earlier and sort by the difference. The ranked delta and the ≥2-runs-in-both-windows rule live in the SQL (`ci-queries.sql` 2) |
| CI outcome mix | Same dashboard → Stacked bar panel. Metric `loom.ci.run.duration_ms`, aggregation **Count** (one data point per run), group by `workflow`, `conclusion`; keep `cancelled` as its own series — never filter it out (`ci-queries.sql` 3) |
| CI top slow jobs | Logs Explorer: filter `body = 'ci.job'`, add columns `loom.repo`, `loom.ci.workflow`, `loom.ci.job`, `loom.ci.duration_ms`, `loom.ci.run_id`, `loom.ci.job_id`; sort by `loom.ci.duration_ms` descending; save as view `CI top slow jobs` (`ci-queries.sql` 4) |
| CI failed run → logs | Logs Explorer: filter `body = 'ci.run' AND loom.ci.conclusion IN ('failure', 'timed_out', 'startup_failure')`, save as `CI failed runs`; take a `loom.ci.run_id`, filter `body = 'ci.job' AND loom.ci.run_id = <id> AND loom.ci.conclusion != 'success'` for its failing jobs, then `loom.ci.job_id = <job id> AND loom.ci.chunk_index EXISTS` sorted by `loom.ci.chunk_index` ascending for the job's log, in order (`ci-queries.sql` 5). **Read the SQL's two absences apart**: `job_id` NULL with an empty `logs_explorer_filter` means the run failed with *no* non-successful job (a `startup_failure`, a cancelled matrix parent, a required check that never produced a job) — there is no log to look for; `job_id` set with `0 of 0` means the job failed and its log never arrived. Before #8528 both read `job_id` 0 / `0 of 0` / `loom.ci.job_id = 0`, the last of which silently matches nothing when pasted into Logs Explorer |
| CI run waterfall | Trace Explorer: filter `name = 'loom.ci.run'`, sort by duration descending, open a run; its `loom.ci.job` children are the waterfall, and the longest child against the root's duration is the run/longest-job comparison (`ci-queries.sql` 6) |
| CI job queue wait | Logs Explorer: filter `body = 'ci.job' AND loom.ci.queued_ms EXISTS`, add columns `loom.repo`, `loom.ci.workflow`, `loom.ci.job`, `loom.ci.queued_ms`; group by `loom.ci.job` with **P50**/**P90**, time range 7 days; **alert when p90 exceeds 60s** — a runner-queue-cap burst shows here long before it shows in the run-level queue segment. A job GitHub reported no `created_at` for contributes no sample, not a zero (`ci-queries.sql` 9) |
| CI shard balance | Logs Explorer: filter `body = 'ci.job' AND loom.ci.shard.kind != 'none'`, add columns `loom.ci.run_id`, `loom.ci.job`, `loom.ci.shard.index`, `loom.ci.shard.total`, `loom.ci.duration_ms`; group by `loom.ci.run_id`, `loom.ci.shard.kind` — the spread between the slowest and fastest leg of one run is the rebalancing signal. The ranked spread across runs lives in the SQL (`ci-queries.sql` 10) |
| CI step waterfall | Trace Explorer: open a run as above and expand a `loom.ci.job` child — its `loom.ci.step` children are the within-job waterfall (compile vs. test). For the cross-run view, filter `name = 'loom.ci.step'`, group by `loom.ci.job`, `loom.ci.step` with **P90**, sorted descending (`ci-queries.sql` 11) |
| CI slowest suites | Trace Explorer: filter `name = 'loom.ci.suite'`, group by `loom.ci.job`, `loom.ci.suite` with **Sum** (and a second query with **P90**), time range 7 days, sorted descending. Add `loom.ci.suite.outcome` / `loom.ci.suite.retried` as filters to separate "slow because it runs twice" from "slow". A suite that did not run in a leg has no span at all, so it never appears here as a fast suite (`ci-queries.sql` 12) |
| CI suite rebalance | Same filter grouped by `loom.ci.run_id`, `loom.ci.shard.index` with **Sum** — one bar per leg of a run, which is the per-leg suite time a `LOOM_CI_SHARD` split should equalize. `argMax(suite, duration)` per leg (the named suite to move) is SQL-only (`ci-queries.sql` 13). Expect the summed suite time to be **less** than the leg's job wall time (checkout and toolchain setup are steps, not suites) and **more** than the wall time of the step that ran them (suites run concurrently); the comparison that matters is between legs of the same run |
| CI critical path | Logs Explorer: filter `body = 'ci.job' AND loom.ci.run_id = <id>`, add columns `loom.ci.job`, `loom.ci.dependency_wait_ms`, `loom.ci.queued_ms`, `loom.ci.duration_ms`, and sort by `loom.ci.duration_ms` descending — the leg with the largest dependency + queue + running sum is the run's critical path, and the three columns say which of the three set it. The `unexplained_s` residual (run wall time minus the run's own queue and that leg's total) requires a run↔job join and is SQL-only (`ci-queries.sql` 14) |
| ETA accuracy | Logs Explorer: filter `loom.kind = 'eta.outcome' AND loom.eta.error_sec EXISTS` (an `eta.*` body is the record's JSON, never the event name, so a `body =` filter matches nothing; a row exported before #10899 has no `loom.kind`, so for an older window drop that term, since `loom.eta.error_sec` is outcome-only anyway), add columns `loom.eta.heuristic`, `loom.eta.revision`, `loom.eta.kind`, `loom.eta.horizon_bucket`, `loom.eta.error_sec`, `loom.eta.covered`; group by `loom.eta.heuristic`, `loom.eta.revision` — **both**, never heuristic alone, or a daemon roll mid-window averages two builds into one number. The `EXISTS` clause is not optional: an abandoned sweep has no error field, and without it the panel scores the abandonment as a perfect prediction. Coverage, bias and the pinball loss that decides a promotion need `avg()`/`median()` over these and stay SQL-only (`eta-queries.sql` 0, Q1, Q2) |
| ETA feature ranking | SQL-only (`eta-queries.sql` Q3): it expands the estimate's explanation body with `JSONExtractKeysAndValuesRaw` and correlates each numeric feature with the error, neither of which is an Explorer operation. Read `distinct_values` beside `rank_corr` — a feature that never varied scores 0.5, not 0 — and `n` beside both: the section's `HAVING n >= 20` makes a short window return nothing at all |
| ETA late surprise, stability, convergence, answer rate | SQL-only (`eta-queries.sql` Q4, Q5, Q6, Q7): Q4 keeps an instant only when every heuristic's late surprise there is decided (an Explorer `avg(loom.eta.above_p90)` would let a heuristic that refuses the hard cases read as better), Q5 needs a window over consecutive emissions, Q6 buckets the actual lead, and Q7 weights each emitted state by how long it stood — counting rows overstates the answer rate because a refusal is never refreshed |
| CI dependency wait | Logs Explorer: filter `body = 'ci.job' AND loom.ci.dependency_wait_ms EXISTS`, add columns `loom.repo`, `loom.ci.workflow`, `loom.ci.job`, `loom.ci.dependency_wait_ms`, `loom.ci.queued_ms`; group by `loom.ci.job` with **P50**/**P90**, time range 7 days. **Alert when a family's p90 dependency wait exceeds its own p90 queue wait**: it is gated by `needs:`, not capacity-starved, and more runners will not move it. An ungated job measures ~0 by construction — treat sub-2s values as job-creation lag, not a serialized edge (`ci-queries.sql` 15) |
| CI slowest tests | Trace Explorer: filter `name = 'loom.ci.test'`, group by `loom.ci.test.binary`, `loom.ci.test` with **Sum** (and a second query with **P90**), time range 7 days, sorted descending. Add `loom.ci.test.outcome` as a filter to separate `flaky` (failed then passed — the #7789 signal) from `fail`/`error` from a clean `pass` (`ci-queries.sql` 16). **Only each leg's slow tail is emitted** — tests at or above the 250 ms floor, capped at `MAX_TEST_SPANS_PER_JOB` (512) per leg — so this panel is a ranking, never a test inventory: a test missing from it is fast or outside the cap, not unrun |
| CI test rebalance | Same filter grouped by `loom.ci.run_id`, `loom.ci.job` with **Sum** — one bar per nextest leg of a run, which is the slow-tail time a `--partition count:k/N` split should equalize. Group by `loom.ci.job` and not by `loom.ci.shard.index` alone: both nextest families shard `1..3`, so index alone merges two unrelated partitions. `argMax(test, duration)` per leg (the named test to move) is SQL-only (`ci-queries.sql` 17). The summed tail time is far **less** than the leg's test-step wall time by design (every sub-floor test is excluded), so compare legs of the same run and the same family, never a leg against its own job span |

`ci-queries.sql` 7) (#9007's per-issue Builder/CI/Judge/merge breakdown) and 8)
(#9337's CI time per trigger reason) have no row above: 7 joins
`loom_analytics.raw_ship_outcome` against `ci.run` records across two logical
sources, which is not a single SigNoz Explorer/dashboard panel the way the
other sections are — it is a `clickhouse-client`-only report, run the same way
as the rollup's other CT queries.

The thirteen CI rows are **recreation steps, not yet observed**: no session has
had an authenticated UI (or API) credential for the trial org since they were
written, so none of the thirteen has been created there yet
([#8946](https://github.com/rjwalters/loom/issues/8946)). Their SQL
counterparts in `ci-queries.sql` are the executed, verified form — see
`evidence.md`'s "CI retro queries, executed live" section — and remain the
acceptance surface until someone with the trial org's login creates the saved
views and records it. The seven #9089 rows are additionally **unexecuted**:
sections 9–15 read attributes and spans that work introduces, so no run
predating it can have produced a row for them.

The two **ETA** rows are in the same category and for the same reason: written,
not created in the trial org, and additionally unexecuted over real data — no
`eta.estimate` or `eta.outcome` record has reached the trial deployment (a
verbatim run of `eta-queries.sql` against it on 2026-10-01 returned every
documented column over zero rows). The verified form is
`loom-daemon/tests/signoz_eta_queries.rs` against the pinned engine.

Save these searches/dashboards through the installed UI and retain sanitized
exports where supported. These precise steps avoid asserting that mutable
organization-specific dashboard IDs are portable. Loom uses operational spans,
not fabricated HTTP requests. Trace Explorer is the acceptance surface; an empty
HTTP service-map/APM page does not establish a missing trace. Record actual
Community-edition limitations and missing usage separately from measured zeros.

### Which SigNoz product views can hold Loom data

Three of SigNoz's product pages behave differently against Loom's spans, and the
reason is Loom's trace *shape*, not the trial's configuration. `evidence.md`'s
"UI view matrix" has the live probes; the short form:

| Page | Holds Loom data? | Why |
| --- | --- | --- |
| Trace Explorer, Logs Explorer, Dashboards | **Yes** — the acceptance surface for every saved view above | They query spans/logs directly, with no semantic-convention requirement |
| Service List / APM overview | **Yes** | The ingester runs every span through `signozspanmetrics/delta` regardless of kind, so RED metrics come from root-span latency/status — not from the OTel HTTP/RPC conventions the page's name suggests |
| Exceptions ("All Errors") | **No, permanently** | It indexes span events named `exception`; Loom emits none, surfacing a failure as span `status=Error` plus a correlated `ERROR` log |
| Service Map | **No, permanently** | A topology edge needs two different `service.name` values, or a CLIENT/SERVER span pair, or a `peer.service`-style peer attribute. Loom's exporter emits one `service.name` (`loom-daemon`) and `SPAN_KIND_INTERNAL` for every span, and the gateway's allowlist forwards no peer key. Configuring a topology connector would not change this |

Do **not** add synthetic HTTP spans, a second `service.name`, or CLIENT-kind
spans to populate the last two: that fabricates a call graph Loom does not have.
The two "No" rows are the documented, expected answer for an operational-span
workload, and `loom-daemon/tests/signoz_topology_shape.rs` fails in ordinary CI
if the shape they depend on changes.

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

**Executed against the pinned engine (2026-09-30), not yet against a live
trial deployment.** `loom-daemon/tests/signoz_cycle_time.rs` (CI, `--ignored`)
runs this view, the shared rollup and all eight CT queries verbatim against
the same pinned ClickHouse `cycle_time_clickhouse.rs` uses for ClickStack, over
a fixture built from the SAME seven `sweep.outcome` envelopes as that
ClickStack proof — hand-translated through the real OTLP mapper's own
`kv_int`/`kv_string` decisions into SigNoz's `attributes_string` /
`attributes_number` split. CT1–CT8's answers match the ClickStack values
exactly; see `evidence.md`'s "Cycle-time analytics executed against the pinned
engine" for the comparison. This is a same-fixture, cross-backend proof against
the pinned engine — it is **not** a live trial observation: no telemetry has
gone through SigNoz's own ingester for this view, which is what #8529's
real-canary comparison still needs.

## Retention and operation

> **Trial-only.** This section governs the isolated trial. The live
> harness-ops store keeps logs 3650 days and traces/metrics 10 years
> ([#10195](https://github.com/rjwalters/loom/issues/10195)); never apply
> `retention.sql` there (#8946 item 2 stays held).

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
TTLs to 7 days for logs/traces and 30 days for metrics, and restores the
30-day *policy* (not the data) on a trial that ran its earlier all-seven-day
version — using the private bundled client:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --multiquery < retention.sql
```

**This deletes data as it runs**, and the deletion is not recoverable:
`MODIFY TTL` materialises on existing parts, so every row already past the new
window is gone when the command returns rather than at some later merge. Read
`retention.sql`'s header before applying it — it carries five behaviours
measured against the pinned engine by
`loom-daemon/tests/signoz_retention_ttl.rs` (#8528), including a units slip
that deletes an entire table while reporting success. **Confirm the command
printed 16 host rows all reading status 0**: one failing statement stops the
client, and because the metric statements are last, a partial run shortens
logs/traces to 7 days while skipping every statement that restores 30 days to
metrics.

Re-run `queries.sql` after every upgrade or retention-setting change. Resource
fingerprint tables retain the upstream **30-minute grace beyond seven days**;
shorter buffer/usage TTLs remain unchanged. Schema migration records, metric
reduction configuration and legacy metadata indexes have no signal TTL. This
is a seven-day log/trace and 30-day metric trial, not a claim that all
metadata is erased at either boundary. Confirm the split from effective DDL,
not from the settings page:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --query "SELECT database, name, extract(create_table_query, 'TTL (.*?)(SETTINGS|\$)') AS ttl, extract(create_table_query, 'ttl_only_drop_parts = \d') AS only_drop_parts FROM system.tables WHERE database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics') AND create_table_query LIKE '%TTL%' AND engine NOT LIKE 'Distributed%' ORDER BY database, name"
```

The TTL column is the **policy**. `ttl_only_drop_parts` is half of the
**outcome**: where it is set, a part holding one over-age row and one in-window
row keeps both, so a row well past seven days stays queryable until an
unrelated merge rewrites that part (measured, with the setting-off and
two-separate-parts controls run alongside it, in
`loom-daemon/tests/signoz_retention_ttl.rs`). The other half is per part, and
needs no per-table time column because ClickHouse stores each part's computed
TTL instants:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --query "SELECT database, table, count() AS parts, sum(rows) AS rows, countIf(delete_ttl_info_max < now()) AS fully_expired_parts, countIf(delete_ttl_info_min < now() AND delete_ttl_info_max >= now()) AS partly_expired_parts, min(delete_ttl_info_min) AS oldest_row_ttl FROM system.parts WHERE active AND database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics') AND delete_ttl_info_min > toDateTime(0) GROUP BY database, table ORDER BY database, table"
```

`fully_expired_parts` above zero means TTL merges are behind (check the merge
failure count in "ClickHouse self-telemetry" below).
`partly_expired_parts` above zero on a `ttl_only_drop_parts = 1` table is the
stuck case: those rows are past the window and will not be deleted on their own.

`signoz_logs.logs_v2` keys its TTL on a per-row `_retention_days` column rather
than a literal interval, so read that column's `default_expression` from
`system.columns` too (a column default applies to rows as they are inserted, so
check the stored values of older rows as well before trusting the split).
TTL deletion uses
background merges and is not an exact deletion deadline or a disk quota.
Accounts, dashboards and settings in PostgreSQL persist independently.
Container stdout/stderr rotate separately at three 10 MiB files per service;
ClickHouse's own system tables and metadata also consume storage.

### ClickHouse self-telemetry

ClickHouse's own `system.*_log` tables, not Loom's signals, dominate this
trial's disk: about 480 MiB against about 2.5 MiB of Loom data on a 2.5-day
soak (`evidence.md`, 2026-09-28). Upstream renders a 1-day TTL for each of
them, which ClickHouse applies only during merges. The casting switches
`system.metric_log` to ClickHouse's `transposed_with_wide_view` schema
because the default 1,552-column table's merge memory grows with input parts
times columns. Under the 2 GiB cap, its TTL-applying merge failed thousands of
times an hour, and parts outlived their TTL. `system.metric_log` stays
queryable as a view with the same columns.

A deployment first started from an older render keeps its wide table. Its
rendered files live in the deployment's own state directory, **not** in this
checkout, so step one is to copy the re-rendered `casting.yaml`,
`casting.yaml.lock` and `pours/` over that directory's copies — a
`--force-recreate` against the stale copies recreates the container with the
*old* config and changes nothing. Keep the originals: that diff is the only
record of what the deployment was running.

The config is then a single-file bind mount, so a plain `up -d` still does not
pick up the re-render: recreate the ClickHouse container (its volume persists).
On start it renames the old table to `system.metric_log_0`, stuck parts
included. That table holds ClickHouse's own diagnostics only, no Loom signal,
so drop it:

```console
diff -r /absolute/deployment/state/pours pours   # expect only the metric_log schema_type line
cp -R casting.yaml casting.yaml.lock pours /absolute/deployment/state/
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml up -d --wait --wait-timeout 1800 --force-recreate --no-deps loom-signoz-telemetrystore-clickhouse-0-0
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --query "DROP TABLE system.metric_log_0"
```

Check for merge failures when you check retention. A non-zero count means some
table has stopped expiring. **Bound the window to the running server** — the
`part_log` rows recording the old failures survive in the volume until their
own 1-day TTL expires them, so an unbounded count keeps reporting a defect that
is already fixed:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml exec -T loom-signoz-telemetrystore-clickhouse-0-0 clickhouse-client --query "SELECT table, countIf(error != 0) failed_merges, countIf(error = 0) ok FROM system.part_log WHERE event_type = 'MergeParts' AND event_time > now() - toIntervalSecond(uptime()) GROUP BY table HAVING failed_merges > 0"
```

Drop the `event_time` predicate only to read the pre-restart history
deliberately; `system.metric_log_0` appearing there is expected for the
seconds between the rename and the `DROP`.

Use the same `--env-file` and `-f` arguments for every Compose command.
`docker compose ... stop` preserves all data.
Restart with the same rendered files and `up -d --wait --wait-timeout 1800`;
verify an old trace, gauge and saved view before accepting restart persistence.
For backup, stop this project and snapshot its four named volumes together:
PostgreSQL data, Keeper coordination, ClickHouse data and histogram user scripts.
Retain the casting, lock and rendered configuration — **and the private
`--env-file`**: the snapshot replays a metastore whose database role and
session-signing secret are already set, so a restore with a fresh secret file
cannot read it. Never stop or remove another deployment's
containers/volumes. Then rehearse the restore as below before relying on the
backup.

## Backup-restore rehearsal

A snapshot you have never restored is a guess. Rehearse it with
`restore-override.yaml`, which layers onto the same rendered compose and turns
it into an isolated `loom-signoz-restore` project. **Do not improvise this with
`docker compose -p` alone**: every volume in the render carries an explicit
top-level `name:`, so a bare project rename produces a second stack that mounts
the *live* volumes read-write. The overlay re-points all four volumes, every
pinned container name, both networks and the published port; it also re-points
the external `loom-observability` network at an egress-less bridge and scales
the ingester to zero, so the rehearsal cannot register the
`signoz-otel-collector` alias a second time and take a share of live OTLP
traffic. `loom-daemon/tests/signoz_restore_contract.rs` re-derives each of
those from the rendered compose, so a later re-render that adds a volume or a
port cannot silently escape the overlay.

Run every command below **from this directory**, like the rest of this README:
`-f` paths are resolved against the working directory, not against the first
compose file. The overlay replaces the published-port list with the Compose
`!override` tag, so Compose **v2.24.4 or newer** is required here (the trial's
baseline of Compose v2 alone is not enough); an older Compose appends instead,
keeps the live `18081` binding and the rehearsal cannot start.

Snapshot (the live project must be stopped for a consistent copy). The helper
that tars each volume is the deployment's **own** pinned PostgreSQL image, so
the procedure introduces no unpinned image and pulls nothing new on the trial
host; it runs as root with `--numeric-owner` so the restored data directories
keep the uids PostgreSQL and ClickHouse expect:

```console
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml stop
TARBALLER=postgres:16@sha256:a3b7f434b2dc57ce85a67e171163eb8ab1a1ebcb39d27484661f26b1dfbe30d6
for v in loom-signoz-metastore-postgres-0-data loom-signoz-telemetrykeeper-0-data \
         loom-signoz-telemetrystore-0-0-data loom-signoz-telemetrystore-user-scripts; do
  docker run --rm --entrypoint sh -v "$v":/src:ro -v /absolute/private/signoz-backup:/bk "$TARBALLER" \
    -c "tar -C /src --numeric-owner -czf /bk/$v.tar.gz ."
done
docker compose --env-file /absolute/private/signoz.env -f pours/deployment/compose.yaml up -d --wait --wait-timeout 1800
```

Restore into the rehearsal project — note the `loom-signoz-restore-` volume
names, and that the source tarball keeps the *live* name so the mapping stays
readable:

```console
docker volume create loom-signoz-restore-telemetrystore-0-0-data
docker run --rm --entrypoint sh -v loom-signoz-restore-telemetrystore-0-0-data:/dst \
  -v /absolute/private/signoz-backup:/bk:ro "$TARBALLER" \
  -c 'cd /dst && tar --numeric-owner -xzf /bk/loom-signoz-telemetrystore-0-0-data.tar.gz'
# ...repeat for the metastore, keeper and user-scripts volumes...

docker compose --env-file /absolute/private/signoz.env \
  -f pours/deployment/compose.yaml -f restore-override.yaml \
  up -d --wait --wait-timeout 1800 loom-signoz-signoz-0
```

Naming only `loom-signoz-signoz-0` starts its dependency chain (metastore,
keeper, ClickHouse, migrator, user-scripts) and leaves the ingester out. Use
the **same** `--env-file` as the backup, for the reason above.

Verify against the live deployment rather than by eye — run the same
`fixture-queries.sql` on both and diff the output:

```console
docker compose --env-file /absolute/private/signoz.env \
  -f pours/deployment/compose.yaml -f restore-override.yaml exec -T \
  loom-signoz-telemetrystore-clickhouse-0-0 \
  clickhouse-client --multiquery --param_run='loom-synthetic-<run-id>' < fixture-queries.sql
```

`exec` addresses a **service**, and the overlay renames containers rather than
services — so this is the same service name the live commands above use, and the
`-f restore-override.yaml` argument is the only thing that decides which of the
two projects it lands in. Never drop it from a rehearsal command: without it the
identical line reads the **live** ClickHouse.

Signal rows, the trace graph and the effective TTL DDL must match exactly.
Expect the raw per-table inventory to differ for `signoz_metrics`: SigNoz's own
self-monitoring metrics keep accruing, so a copy taken later than the baseline
read legitimately holds more of them. Confirm that is what you are seeing by
re-reading the **live** side now — it should have caught up to the restored
count — rather than accepting the delta. Also check the restored app on its own
port (`127.0.0.1:18091`), log in with the same credential, and confirm a
dashboard you created before the snapshot is listed.

Tear down with the project's own arguments, and **dry-run it first** so you can
read the object list before anything is removed:

```console
docker compose --env-file /absolute/private/signoz.env \
  -f pours/deployment/compose.yaml -f restore-override.yaml down --volumes --dry-run
```

Every line must name a `loom-signoz-restore-` object. If any live volume,
container or network appears, stop: the overlay is out of step with the render.
Re-run without `--dry-run` to finish, then `docker volume ls --filter
name=loom-signoz-restore-` to confirm nothing survived — `down --volumes` removes
only volumes the project declares, so a volume you created by hand for a
tarball it turned out not to need is left behind.

**Status of this procedure: the overlay renders and is contract-tested, but the
rehearsal has not been executed against real backup tarballs yet** — see
"Backup-restore rehearsal overlay" in `evidence.md` for exactly what was and was
not verified. [#9279](https://github.com/rjwalters/loom/issues/9279) owns the
live run; until it lands, treat this backup as untested.

## Upgrade, wipe and Cloud

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
