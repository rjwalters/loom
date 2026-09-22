-- SigNoz-side queries for the SHARED telemetry fixture manifest produced by
-- `loom-daemon telemetry-fixture` (see ../../docs/telemetry-fixtures.md).
--
-- `queries.sql` is the separate, live-verified ad-hoc three-signal check from the
-- original deployment proof; it stays as-is. This file is the reproducible
-- artifact for the shared manifest that the cross-backend comparison (#8529)
-- consumes, so its vocabulary is pinned to what Loom actually exports:
-- `loom-daemon/tests/signoz_trial_artifacts.rs` fails if any span name, metric
-- name, attribute key or resource key below drifts from the generated manifest
-- or from the gateway's `keep_keys` allowlist.
--
-- STATUS: not executed against a live backend by the change that added it — this
-- sweep host has no Docker access and the trial deployment is stopped. Column
-- names follow the pinned SigNoz v0.142.1 schema; run query 0 first and reconcile
-- before trusting any later result. Record observations in `evidence.md`.
--
-- Run every statement in one pass, binding the run under test once:
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery --param_run='loom-synthetic-<run-id>' \
--     < fixture-queries.sql
--
-- `<run-id>` is the `--run-id` passed to `telemetry-fixture`; the generator sets
-- `host.id` / `service.instance.id` to `loom-synthetic-<run-id>` precisely so one
-- trial can be isolated without a time-window guess. Delivery is at least once:
-- always compare distinct identities, never bare row counts.

-- 0. Confirm the attribute/resource container columns actually exist with these
--    names before running anything below. A pinned-schema mismatch must be
--    reconciled here, not worked around inside an analysis query.
SELECT database, table, name, type
FROM system.columns
WHERE database IN ('signoz_traces', 'signoz_logs', 'signoz_metrics')
  AND table IN ('signoz_index_v3', 'logs_v2', 'samples_v4', 'time_series_v4')
  AND (name LIKE '%attributes%' OR name LIKE '%resources%' OR name = 'labels')
ORDER BY database, table, name;

-- 1. Totals. The version-1 manifest expects 37 distinct spans, 14 correlated
--    logs and 3 metric data points. `rows` above `unique_spans` is duplicate
--    delivery, which is permitted; `unique_spans` below the manifest count is
--    loss and is not.
SELECT count() AS rows,
       uniqExact(trace_id, span_id) AS unique_spans,
       uniqExact(trace_id) AS traces
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String};

-- 2. Full graph for the run. Verify parentage by ID only: the fixture puts issue
--    18 in two repositories on overlapping timestamps precisely so an issue
--    number can never be used to reconstruct a trace.
SELECT trace_id, span_id, parent_span_id, name,
       attributes_string['loom.repo'] AS repo,
       attributes_string['loom.phase'] AS phase,
       attributes_string['loom.attempt'] AS attempt,
       attributes_string['loom.result'] AS result,
       status_code_string, timestamp, duration_nano
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
ORDER BY trace_id, timestamp, span_id;

-- 3. Failure and duration by repo/role/runtime/model over role attempts. A
--    ClickHouse Map subscript yields '' for an absent key, so read this beside
--    query 8: an empty cell here is MISSING, never a measured empty value.
SELECT attributes_string['loom.repo'] AS repo,
       attributes_string['loom.role'] AS role,
       attributes_string['loom.runtime'] AS runtime,
       attributes_string['loom.model'] AS model,
       count() AS attempts,
       countIf(status_code_string = 'Error') AS errors,
       round(quantile(0.95)(duration_nano) / 1e9, 3) AS p95_seconds,
       round(max(duration_nano) / 1e9, 3) AS max_seconds
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
  AND name = 'loom.role_attempt'
GROUP BY repo, role, runtime, model
ORDER BY errors DESC, repo, role, runtime, model;

-- 4. Repair cycle. The repair scenario must show a rejected Judge attempt 1, a
--    Doctor, then an approved Judge attempt 2 with a DIFFERENT span ID. A retry
--    that reuses a span ID would collapse the two verdicts into one row.
SELECT attributes_string['loom.phase'] AS phase,
       attributes_string['loom.attempt'] AS attempt,
       attributes_string['loom.result'] AS result,
       span_id, parent_span_id, status_code_string, timestamp
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
  AND name = 'loom.role_attempt'
  AND attributes_string['loom.sweep_id'] LIKE '%-repair'
ORDER BY timestamp, span_id;

-- 5. Incomplete versus successful. Any trace with children but no exported
--    `loom.sweep` root is IN PROGRESS OR CRASHED, never a success — the
--    crash_incomplete scenario must appear here and nothing else may. Re-run
--    this against a long-running real sweep before its root ends, then again
--    afterwards, to establish partial-trace behaviour rather than inferring it.
SELECT trace_id,
       count() AS spans,
       countIf(name = 'loom.sweep') AS root_spans,
       anyIf(attributes_string['loom.result'], name = 'loom.sweep') AS root_result
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
GROUP BY trace_id
HAVING root_spans = 0
ORDER BY trace_id;

-- 6. Log correlation. An empty `span_name` marks a log whose span did not
--    arrive; that is an uncorrelated log, not a missing log.
SELECT l.timestamp, l.trace_id, l.span_id, l.severity_text,
       s.name AS span_name,
       s.attributes_string['loom.phase'] AS span_phase
FROM signoz_logs.logs_v2 AS l
LEFT JOIN signoz_traces.signoz_index_v3 AS s
  ON l.trace_id = s.trace_id AND l.span_id = s.span_id
WHERE l.resources_string['host.id'] = {run:String}
ORDER BY l.timestamp, l.span_id;

SELECT count() AS rows,
       uniqExact(trace_id, span_id, timestamp) AS unique_logs
FROM signoz_logs.logs_v2
WHERE resources_string['host.id'] = {run:String};

-- 7. Token gauges: missing usage must stay distinguishable from measured zero.
--    `synthetic-zero` has `loom.tokens.usage_fraction` = 0.0; `synthetic-unknown`
--    must have NO usage_fraction data point at all. An absent row for the unknown
--    account is the expected observation, not a failed query. Metric labels are
--    filtered on the fixture's own account names because the metric pipeline's
--    datapoint allowlist keeps `account` but the presence of `host.id` among
--    metric labels is schema-dependent — inspect it first:
SELECT DISTINCT metric_name, labels
FROM signoz_metrics.time_series_v4
WHERE metric_name LIKE 'loom.%'
ORDER BY metric_name, labels;

SELECT s.metric_name,
       JSONExtractString(t.labels, 'account') AS account,
       s.unix_milli, s.value
FROM signoz_metrics.samples_v4 AS s
INNER JOIN signoz_metrics.time_series_v4 AS t USING (fingerprint)
WHERE s.metric_name IN ('loom.tokens.usage_fraction', 'loom.tokens.exhausted')
  AND JSONExtractString(t.labels, 'account') IN ('synthetic-zero', 'synthetic-unknown')
ORDER BY s.metric_name, account, s.unix_milli;

-- 8. Absence is reported as absence. The preflight_rejection scenario carries no
--    runtime/model launch metadata, so `has_runtime`/`has_model` must be 0 for
--    its spans and 1 where a launch was observed. `mapContains` is the only way
--    to tell an absent key from a stored empty string.
SELECT attributes_string['loom.phase'] AS phase,
       name,
       mapContains(attributes_string, 'loom.runtime') AS has_runtime,
       mapContains(attributes_string, 'loom.model') AS has_model,
       mapContains(attributes_string, 'loom.sweep_id') AS has_sweep_id,
       count() AS spans
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
GROUP BY phase, name, has_runtime, has_model, has_sweep_id
ORDER BY name, phase;

-- 9. Privacy assertion. The fixture deliberately ships the harmless string
--    LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529 under a prohibited
--    `prompt.content` attribute, which the gateway's allowlist must drop.
--    EVERY row of this query must report 0. A non-zero count means the gateway
--    forwarded a prohibited attribute and the run is void as privacy evidence.
SELECT 'traces' AS signal, count() AS sentinel_rows
FROM signoz_traces.signoz_index_v3
WHERE resources_string['host.id'] = {run:String}
  AND (mapContains(attributes_string, 'prompt.content')
       OR position(name, 'LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529') > 0
       OR arrayExists(v -> position(v, 'LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529') > 0,
                      mapValues(attributes_string)))
UNION ALL
SELECT 'logs', count()
FROM signoz_logs.logs_v2
WHERE resources_string['host.id'] = {run:String}
  AND (mapContains(attributes_string, 'prompt.content')
       OR position(body, 'LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529') > 0
       OR arrayExists(v -> position(v, 'LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529') > 0,
                      mapValues(attributes_string)))
UNION ALL
SELECT 'metrics', count()
FROM signoz_metrics.time_series_v4
WHERE position(labels, 'LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529') > 0;

-- 10. Footprint for this run only, for the shared comparison. Active parts cover
--     signal storage; they exclude PostgreSQL, system tables, images and total
--     volume usage, so this is not a capacity or cost benchmark.
SELECT database, table, sum(rows) AS rows, sum(bytes_on_disk) AS active_bytes
FROM system.parts
WHERE active AND database IN ('signoz_logs', 'signoz_traces', 'signoz_metrics')
GROUP BY database, table
ORDER BY database, table;
