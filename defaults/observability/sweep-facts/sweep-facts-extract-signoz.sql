-- SigNoz half of the sweep-facts seam (Issues #9446, #9466, #9433; pattern:
-- #8665).
--
-- Mirror of `../sweep-facts-extract-clickstack.sql`: it defines exactly ONE
-- object, `loom_analytics.raw_sweep_fact`, with the same column names, order
-- and meanings. Everything downstream is shared, so both backends answer the
-- canonical sweep-facts question set from one definition.
-- `loom-daemon/tests/sweep_facts_artifacts.rs` fails in ordinary CI if the two
-- views stop agreeing on that column list.
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery < sweep-facts-extract-signoz.sql
--
-- STATUS: contract-checked in CI, NOT yet executed against a live SigNoz
-- deployment — the ClickStack side carries the live-verified precedent for
-- these conventions. See `../sweep-facts-questions.md` § "Verification status"
-- before treating a number produced here as comparable evidence under #8529.
--
-- Two deliberate differences from the ClickStack view, both forced by SigNoz's
-- schema rather than chosen (same two the cycle-time extraction documents):
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
-- field missing from BOTH maps is NULL, never '' and never 0; the rework
-- classification counts, measured seconds and open-event counts (#9507) are
-- walked here (JSONExtract over the array attribute) so the fact shape carries
-- the same rework columns the D1 rollup computes with `json_each`; and `suspect` is the same derived 0/1 (#9454) the D1 rollup
-- derives from `tokens_status`.

CREATE DATABASE IF NOT EXISTS loom_analytics;

-- `duration_sec` is `Nullable(Int64)` on purpose (#9507): a rework event
-- whose clearing forge event was never observed carries NO `duration_sec`, and
-- a plain `Int64` would let JSONExtract default it to 0 — fabricating a
-- measured zero. Nullable keeps it NULL, so `rework_*_open` can count it and
-- `rework_*_sec` sums only measured entries, exactly as the D1 rollup does.
CREATE OR REPLACE VIEW loom_analytics.raw_sweep_fact AS
WITH
    JSONExtract(attributes_string['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations,
    JSONExtract(attributes_string['loom.rework_events'],
                'Array(Tuple(classification String, kind String, reason String, duration_sec Nullable(Int64)))')
        AS rework_events
SELECT
    toDateTime64(fromUnixTimestamp64Nano(toInt64(timestamp)), 3)   AS emitted_at,
    attributes_string['loom.repo']                                 AS repo,
    ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.issue'),
           toUInt32(attributes_number['loom.issue']), NULL),
        toUInt32OrNull(attributes_string['loom.issue'])), 0)       AS issue,
    attributes_string['loom.sweep_id']                             AS sweep_id,
    resources_string['host.id']                                    AS host_id,
    attributes_string['loom.result']                               AS result,
    if(mapContains(attributes_string, 'loom.disposition'),
       attributes_string['loom.disposition'], NULL)                AS disposition,
    if(mapContains(attributes_string, 'loom.failure_class'),
       attributes_string['loom.failure_class'], NULL)              AS failure_class,
    if(mapContains(attributes_string, 'loom.tokens_status'),
       attributes_string['loom.tokens_status'], NULL)              AS tokens_status,
    if(mapContains(attributes_string, 'loom.config.arm'),
       attributes_string['loom.config.arm'], NULL)                 AS config_arm,
    if(mapContains(attributes_string, 'loom.models_used'),
       attributes_string['loom.models_used'], NULL)                AS models_used,
    coalesce(
        if(mapContains(attributes_number, 'loom.total_duration_sec'),
           toInt64(attributes_number['loom.total_duration_sec']), NULL),
        toInt64OrNull(attributes_string['loom.total_duration_sec']))
                                                                   AS total_duration_sec,
    if(mapContains(attributes_string, 'loom.phase_durations'),
       attributes_string['loom.phase_durations'], NULL)            AS phase_durations,
    coalesce(
        if(mapContains(attributes_number, 'loom.tokens_in'),
           toInt64(attributes_number['loom.tokens_in']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_in']))        AS tokens_in,
    coalesce(
        if(mapContains(attributes_number, 'loom.tokens_out'),
           toInt64(attributes_number['loom.tokens_out']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_out']))       AS tokens_out,
    if(mapContains(attributes_string, 'loom.tokens_by_model'),
       attributes_string['loom.tokens_by_model'], NULL)            AS tokens_by_model,
    coalesce(
        if(mapContains(attributes_number, 'loom.tokens_unattributed_in'),
           toInt64(attributes_number['loom.tokens_unattributed_in']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_unattributed_in']))
                                                        AS tokens_unattributed_in,
    coalesce(
        if(mapContains(attributes_number, 'loom.tokens_unattributed_out'),
           toInt64(attributes_number['loom.tokens_unattributed_out']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_unattributed_out']))
                                                        AS tokens_unattributed_out,
    coalesce(
        if(mapContains(attributes_number, 'loom.lines_added'),
           toInt64(attributes_number['loom.lines_added']), NULL),
        toInt64OrNull(attributes_string['loom.lines_added']))      AS lines_added,
    coalesce(
        if(mapContains(attributes_number, 'loom.lines_deleted'),
           toInt64(attributes_number['loom.lines_deleted']), NULL),
        toInt64OrNull(attributes_string['loom.lines_deleted']))    AS lines_deleted,
    coalesce(
        if(mapContains(attributes_number, 'loom.hw_lines_added'),
           toInt64(attributes_number['loom.hw_lines_added']), NULL),
        toInt64OrNull(attributes_string['loom.hw_lines_added']))   AS hw_lines_added,
    coalesce(
        if(mapContains(attributes_number, 'loom.hw_lines_deleted'),
           toInt64(attributes_number['loom.hw_lines_deleted']), NULL),
        toInt64OrNull(attributes_string['loom.hw_lines_deleted'])) AS hw_lines_deleted,
    coalesce(
        if(mapContains(attributes_number, 'loom.hw_files'),
           toInt64(attributes_number['loom.hw_files']), NULL),
        toInt64OrNull(attributes_string['loom.hw_files']))         AS hw_files,
    coalesce(
        if(mapContains(attributes_number, 'loom.generated_lines'),
           toInt64(attributes_number['loom.generated_lines']), NULL),
        toInt64OrNull(attributes_string['loom.generated_lines']))  AS generated_lines,
    coalesce(
        if(mapContains(attributes_number, 'loom.test_lines'),
           toInt64(attributes_number['loom.test_lines']), NULL),
        toInt64OrNull(attributes_string['loom.test_lines']))       AS test_lines,
    -- The Curator's a-priori size estimate (#9432), the one forecast column
    -- here. NULL when absent from BOTH maps — an unsized issue is not an issue
    -- sized at zero — and ORDINAL, never summed as a size (SF8, #9433).
    coalesce(
        if(mapContains(attributes_number, 'loom.story_points'),
           toInt64(attributes_number['loom.story_points']), NULL),
        toInt64OrNull(attributes_string['loom.story_points']))     AS story_points,
    coalesce(
        if(mapContains(attributes_number, 'loom.doctor_cycles'),
           toInt64(attributes_number['loom.doctor_cycles']), NULL),
        toInt64OrNull(attributes_string['loom.doctor_cycles']))    AS doctor_cycles,
    if(mapContains(attributes_string, 'loom.judge_verdicts'),
       attributes_string['loom.judge_verdicts'], NULL)             AS judge_verdicts,
    coalesce(
        if(mapContains(attributes_number, 'loom.attempt_index'),
           toInt64(attributes_number['loom.attempt_index']), NULL),
        toInt64OrNull(attributes_string['loom.attempt_index']))    AS attempt_index,
    if(mapContains(attributes_string, 'loom.previous_sweep_id'),
       attributes_string['loom.previous_sweep_id'], NULL)          AS previous_sweep_id,
    if(mapContains(attributes_string, 'loom.trigger'),
       attributes_string['loom.trigger'], NULL)                    AS trigger,
    if(mapContains(attributes_string, 'loom.rework_events'),
       length(arrayFilter(t -> t.classification = 'substantive', rework_events)),
       NULL)                                                       AS rework_substantive,
    if(mapContains(attributes_string, 'loom.rework_events'),
       length(arrayFilter(t -> t.classification = 'environmental', rework_events)),
       NULL)                                                       AS rework_environmental,
    if(mapContains(attributes_string, 'loom.rework_events'),
       toInt64(arraySum(t -> ifNull(t.duration_sec, 0),
           arrayFilter(t -> t.classification = 'substantive', rework_events))),
       NULL)                                                       AS rework_substantive_sec,
    if(mapContains(attributes_string, 'loom.rework_events'),
       toInt64(arraySum(t -> ifNull(t.duration_sec, 0),
           arrayFilter(t -> t.classification = 'environmental', rework_events))),
       NULL)                                                       AS rework_environmental_sec,
    if(mapContains(attributes_string, 'loom.rework_events'),
       toInt64(length(arrayFilter(
           t -> t.classification = 'substantive' AND isNull(t.duration_sec),
           rework_events))),
       NULL)                                                       AS rework_substantive_open,
    if(mapContains(attributes_string, 'loom.rework_events'),
       toInt64(length(arrayFilter(
           t -> t.classification = 'environmental' AND isNull(t.duration_sec),
           rework_events))),
       NULL)                                                       AS rework_environmental_open,
    coalesce(
        if(mapContains(attributes_number, 'loom.pr_number'),
           toUInt32(attributes_number['loom.pr_number']), NULL),
        toUInt32OrNull(attributes_string['loom.pr_number']))       AS pr_number,
    if(mapContains(attributes_string, 'loom.pr_numbers'),
       attributes_string['loom.pr_numbers'], NULL)                 AS pr_numbers,
    toUInt8(ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.tokens_status'),
           attributes_number['loom.tokens_status'], NULL),
        attributes_string['loom.tokens_status']), '') = 'suspect') AS suspect
FROM signoz_logs.distributed_logs_v2
WHERE body = 'sweep.outcome'
  AND mapContains(attributes_string, 'loom.sweep_id');
