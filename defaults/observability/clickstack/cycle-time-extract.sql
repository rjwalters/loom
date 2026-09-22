-- ClickStack half of the cycle-time analytics seam (Issue #8665).
--
-- This file defines exactly ONE object: `loom_analytics.raw_ship_outcome`, the
-- view that turns this backend's raw log rows into the normalized ship columns
-- `../cycle-time-rollup.sql` ingests. Everything else — the durable table, the
-- refresh, and every CT query — is backend-neutral and lives beside it. Keeping
-- the per-backend surface to one view is what makes SigNoz/ClickStack parity a
-- property of the design rather than of two hand-synchronized query sets.
--
--   docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
--     clickhouse-client --multiquery < cycle-time-extract.sql
--   # then, once, with a window for the backfill:
--   … clickhouse-client --param_since=… --param_until=… \
--       --queries-file ../cycle-time-rollup.sql
--
-- Column list and order are load-bearing: `cycle-time-rollup.sql` inserts them
-- by name, and `loom-daemon/tests/cycle_time_artifacts.rs` fails in ordinary CI
-- if this view and the SigNoz one stop agreeing on them.

CREATE DATABASE IF NOT EXISTS loom_analytics;

-- `Body` carries the record kind: the OTLP mapping sets `LogRecord.event_name`
-- AND the body to the same event name (`observability/otlp/mapping.rs`), and
-- the ClickHouse OTel schema has no event-name column — so the body is the
-- filter that actually exists here. `loom.sweep_id` must be present: a row
-- without it cannot be a ship identity, and silently rolling one up as
-- `sweep_id = ''` would merge unrelated sweeps under the ReplacingMergeTree key.
--
-- Every optional field becomes NULL when its key is ABSENT, never '' or 0 — a
-- ClickHouse map subscript yields '' for a missing key, which is exactly the
-- confusion between "unmeasured" and "measured empty" the schema forbids.
-- Nested attributes (`loom.phase_durations`) arrive as a JSON string in the
-- String-valued attribute map; extracting to a NAMED tuple reads them by key,
-- so a change in the exporter's field order cannot silently transpose them.
CREATE OR REPLACE VIEW loom_analytics.raw_ship_outcome AS
WITH
    JSONExtract(LogAttributes['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations
SELECT
    toDateTime64(Timestamp, 3)                                   AS finished_at,
    LogAttributes['loom.repo']                                   AS repo,
    LogAttributes['loom.sweep_id']                               AS sweep_id,
    toUInt32OrZero(LogAttributes['loom.issue'])                  AS issue,
    ResourceAttributes['host.id']                                AS host_id,
    LogAttributes['loom.repo.visibility']                        AS repo_visibility,
    LogAttributes['loom.result']                                 AS result,
    toInt64OrZero(LogAttributes['loom.total_duration_sec'])      AS total_duration_sec,
    arrayMap(x -> x.1, phase_durations)                          AS phases,
    arrayMap(x -> x.2, phase_durations)                          AS phase_durations_sec,
    toUInt8(mapContains(LogAttributes, 'loom.phase_durations'))  AS phase_durations_present,
    toUInt32OrNull(LogAttributes['loom.pr_number'])              AS pr_number,
    toUInt32OrNull(LogAttributes['loom.doctor_cycles'])          AS doctor_cycles,
    if(mapContains(LogAttributes, 'loom.failure_class'),
       LogAttributes['loom.failure_class'], NULL)                AS failure_class,
    if(mapContains(LogAttributes, 'loom.runtime'),
       LogAttributes['loom.runtime'], NULL)                      AS runtime,
    if(mapContains(LogAttributes, 'loom.provider'),
       LogAttributes['loom.provider'], NULL)                     AS provider,
    if(mapContains(LogAttributes, 'loom.model'),
       LogAttributes['loom.model'], NULL)                        AS model,
    if(mapContains(LogAttributes, 'loom.configured_model'),
       LogAttributes['loom.configured_model'], NULL)             AS configured_model,
    if(mapContains(LogAttributes, 'loom.effort'),
       LogAttributes['loom.effort'], NULL)                       AS effort
FROM default.otel_logs
WHERE Body = 'sweep.outcome'
  AND mapContains(LogAttributes, 'loom.sweep_id');
