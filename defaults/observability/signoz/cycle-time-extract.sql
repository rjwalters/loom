-- SigNoz half of the cycle-time analytics seam (Issue #8665).
--
-- Mirror of `../clickstack/cycle-time-extract.sql`: it defines exactly ONE
-- object, `loom_analytics.raw_ship_outcome`, with the same column names, order
-- and meanings. Everything downstream — the durable rollup table, the refresh,
-- and all eight CT queries — is the backend-neutral pair of files beside it, so
-- both backends answer the canonical question set from one definition.
-- `loom-daemon/tests/cycle_time_artifacts.rs` fails in ordinary CI if the two
-- views stop agreeing on that column list.
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery < cycle-time-extract.sql
--
-- STATUS: contract-checked in CI, NOT yet executed against a live SigNoz
-- deployment — the ClickStack side is the live-verified one
-- (`loom-daemon/tests/cycle_time_clickhouse.rs`). See
-- `../cycle-time-questions.md` § "Verification status" before treating a number
-- produced here as comparable evidence under #8529.
--
-- Two deliberate differences from the ClickStack view, both forced by SigNoz's
-- schema rather than chosen:
--
--  1. SigNoz splits attributes by VALUE TYPE across `attributes_string` /
--     `attributes_number`, where the ClickHouse OTel schema keeps one
--     String-valued map. Which map an integer attribute lands in is a property
--     of the pinned ingester, so every numeric read below tries the number map
--     first and falls back to parsing the string map. That is not defensive
--     clutter: guessing wrong yields zero rows, silently, forever.
--  2. Reads go through `distributed_logs_v2`, the query-facing table.
--
-- Everything else is identical, including the absence contract: an optional
-- field missing from BOTH maps is NULL, never '' and never 0.

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_ship_outcome AS
WITH
    JSONExtract(attributes_string['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations
SELECT
    toDateTime64(fromUnixTimestamp64Nano(toInt64(timestamp)), 3)  AS finished_at,
    attributes_string['loom.repo']                                AS repo,
    attributes_string['loom.sweep_id']                            AS sweep_id,
    ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.issue'),
           toUInt32(attributes_number['loom.issue']), NULL),
        toUInt32OrNull(attributes_string['loom.issue'])), 0)      AS issue,
    resources_string['host.id']                                   AS host_id,
    attributes_string['loom.repo.visibility']                     AS repo_visibility,
    attributes_string['loom.result']                              AS result,
    ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.total_duration_sec'),
           toInt64(attributes_number['loom.total_duration_sec']), NULL),
        toInt64OrNull(attributes_string['loom.total_duration_sec'])), 0)
                                                                  AS total_duration_sec,
    arrayMap(x -> x.1, phase_durations)                           AS phases,
    arrayMap(x -> x.2, phase_durations)                           AS phase_durations_sec,
    toUInt8(mapContains(attributes_string, 'loom.phase_durations'))
                                                                  AS phase_durations_present,
    coalesce(
        if(mapContains(attributes_number, 'loom.pr_number'),
           toUInt32(attributes_number['loom.pr_number']), NULL),
        toUInt32OrNull(attributes_string['loom.pr_number']))      AS pr_number,
    coalesce(
        if(mapContains(attributes_number, 'loom.doctor_cycles'),
           toUInt32(attributes_number['loom.doctor_cycles']), NULL),
        toUInt32OrNull(attributes_string['loom.doctor_cycles']))  AS doctor_cycles,
    if(mapContains(attributes_string, 'loom.failure_class'),
       attributes_string['loom.failure_class'], NULL)             AS failure_class,
    if(mapContains(attributes_string, 'loom.runtime'),
       attributes_string['loom.runtime'], NULL)                   AS runtime,
    if(mapContains(attributes_string, 'loom.provider'),
       attributes_string['loom.provider'], NULL)                  AS provider,
    if(mapContains(attributes_string, 'loom.model'),
       attributes_string['loom.model'], NULL)                     AS model,
    if(mapContains(attributes_string, 'loom.configured_model'),
       attributes_string['loom.configured_model'], NULL)          AS configured_model,
    if(mapContains(attributes_string, 'loom.effort'),
       attributes_string['loom.effort'], NULL)                    AS effort
FROM signoz_logs.distributed_logs_v2
WHERE body = 'sweep.outcome'
  AND mapContains(attributes_string, 'loom.sweep_id');
