-- ClickStack half of the sweep-facts seam (Issues #9446, #9466, #9433;
-- pattern: #8665).
--
-- This file defines exactly ONE object: `loom_analytics.raw_sweep_fact`, the
-- view that turns this backend's raw log rows into the normalized sweep-fact
-- columns the D1 rollup (`../sweep-facts-rollup.sql`) produces from D1's own
-- `records` table. Keeping the per-backend surface to one view is what makes
-- SigNoz/ClickStack/D1 parity a property of the design rather than of three
-- hand-synchronized query sets.
--
--   docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
--     clickhouse-client --multiquery < sweep-facts-extract-clickstack.sql
--
-- Column list and order are load-bearing: they mirror the D1 `sweep_facts`
-- table (minus `schema_version`, which is an envelope field of the D1 store —
-- an OTel log record carries no envelope), and
-- `loom-daemon/tests/sweep_facts_artifacts.rs` fails in ordinary CI if this
-- view and the SigNoz one stop agreeing on them.
--
-- Dialect conventions are the cycle-time extraction's, unchanged: `Body`
-- carries the record kind (the OTLP mapping sets the event name AND the body
-- to the same name, and the ClickHouse OTel schema has no event-name column),
-- `loom.sweep_id` must be present (a row without it cannot be a sweep
-- identity), and every optional field becomes NULL when its key is ABSENT —
-- never '' and never 0 — because a ClickHouse map subscript yields '' for a
-- missing key, which is exactly the absent-vs-zero confusion the telemetry
-- schema forbids. Nested array attributes (`loom.phase_durations`,
-- `loom.rework_events`) arrive as JSON strings in the String-valued attribute
-- map; the rework classification counts are walked here so the fact shape
-- carries the same two counts the D1 rollup computes with `json_each`.
--
-- STATUS: contract-checked in CI; not yet executed live against a
-- sweep-facts window (the cycle-time extraction is the live-verified
-- precedent for these conventions).
--
-- OTLP-attribute coverage (#9586): every `loom.*` key this view reads
-- either survives the gateway's log keep_keys allowlist or is pinned
-- OTLP-absent in `loom-daemon/tests/story_points_gateway_survival.rs` —
-- the daemon exports `loom.attempt_index`, `loom.previous_sweep_id`,
-- `loom.trigger`, `loom.rework_events`, `loom.pr_numbers`,
-- `loom.hw_lines_{added,deleted}`, `loom.hw_files`, `loom.generated_lines`
-- and `loom.test_lines` in the D1/JSONL payload (and, for the three
-- lineage keys, span metadata) but NOT as sweep.outcome log attributes,
-- so those columns are NULL on this backend BY DESIGN until the OTLP
-- mapping exports them. D1 fills them from the payload; admitting the
-- keys in the gateway allowlist would forward nothing.

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_sweep_fact AS
WITH
    JSONExtract(LogAttributes['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations,
    JSONExtract(LogAttributes['loom.rework_events'],
                'Array(Tuple(classification String, kind String, reason String, duration_sec Int64))')
        AS rework_events
SELECT
    toDateTime64(Timestamp, 3)                                          AS emitted_at,
    LogAttributes['loom.repo']                                          AS repo,
    toUInt32OrZero(LogAttributes['loom.issue'])                         AS issue,
    LogAttributes['loom.sweep_id']                                      AS sweep_id,
    ResourceAttributes['host.id']                                       AS host_id,
    LogAttributes['loom.result']                                        AS result,
    if(mapContains(LogAttributes, 'loom.disposition'),
       LogAttributes['loom.disposition'], NULL)                         AS disposition,
    if(mapContains(LogAttributes, 'loom.failure_class'),
       LogAttributes['loom.failure_class'], NULL)                       AS failure_class,
    if(mapContains(LogAttributes, 'loom.tokens_status'),
       LogAttributes['loom.tokens_status'], NULL)                       AS tokens_status,
    if(mapContains(LogAttributes, 'loom.config.arm'),
       LogAttributes['loom.config.arm'], NULL)                          AS config_arm,
    if(mapContains(LogAttributes, 'loom.models_used'),
       LogAttributes['loom.models_used'], NULL)                         AS models_used,
    toInt64OrZero(LogAttributes['loom.total_duration_sec'])             AS total_duration_sec,
    if(mapContains(LogAttributes, 'loom.phase_durations'),
       LogAttributes['loom.phase_durations'], NULL)                     AS phase_durations,
    toInt64OrNull(LogAttributes['loom.tokens_in'])                      AS tokens_in,
    toInt64OrNull(LogAttributes['loom.tokens_out'])                     AS tokens_out,
    if(mapContains(LogAttributes, 'loom.tokens_by_model'),
       LogAttributes['loom.tokens_by_model'], NULL)                     AS tokens_by_model,
    toInt64OrNull(LogAttributes['loom.tokens_unattributed_in'])          AS tokens_unattributed_in,
    toInt64OrNull(LogAttributes['loom.tokens_unattributed_out'])         AS tokens_unattributed_out,
    toInt64OrNull(LogAttributes['loom.lines_added'])                    AS lines_added,
    toInt64OrNull(LogAttributes['loom.lines_deleted'])                  AS lines_deleted,
    toInt64OrNull(LogAttributes['loom.hw_lines_added'])                 AS hw_lines_added,
    toInt64OrNull(LogAttributes['loom.hw_lines_deleted'])               AS hw_lines_deleted,
    toInt64OrNull(LogAttributes['loom.hw_files'])                       AS hw_files,
    toInt64OrNull(LogAttributes['loom.generated_lines'])                AS generated_lines,
    toInt64OrNull(LogAttributes['loom.test_lines'])                     AS test_lines,
    -- The Curator's a-priori size estimate (#9432), the one forecast column
    -- here. NULL when absent — an unsized issue is not an issue sized at zero
    -- — and ORDINAL, never summed as a size (SF8, #9433).
    toInt64OrNull(LogAttributes['loom.story_points'])                   AS story_points,
    toInt64OrNull(LogAttributes['loom.doctor_cycles'])                  AS doctor_cycles,
    if(mapContains(LogAttributes, 'loom.judge_verdicts'),
       LogAttributes['loom.judge_verdicts'], NULL)                      AS judge_verdicts,
    toInt64OrNull(LogAttributes['loom.attempt_index'])                  AS attempt_index,
    if(mapContains(LogAttributes, 'loom.previous_sweep_id'),
       LogAttributes['loom.previous_sweep_id'], NULL)                   AS previous_sweep_id,
    if(mapContains(LogAttributes, 'loom.trigger'),
       LogAttributes['loom.trigger'], NULL)                             AS trigger,
    if(mapContains(LogAttributes, 'loom.rework_events'),
       length(arrayFilter(t -> t.classification = 'substantive', rework_events)),
       NULL)                                                            AS rework_substantive,
    if(mapContains(LogAttributes, 'loom.rework_events'),
       length(arrayFilter(t -> t.classification = 'environmental', rework_events)),
       NULL)                                                            AS rework_environmental,
    toUInt32OrNull(LogAttributes['loom.pr_number'])                     AS pr_number,
    if(mapContains(LogAttributes, 'loom.pr_numbers'),
       LogAttributes['loom.pr_numbers'], NULL)                          AS pr_numbers,
    -- Derived 0/1 (#9454), exactly as the D1 rollup derives it: 1 iff the
    -- emitter's plausibility guard published this record as `suspect`.
    toUInt8(LogAttributes['loom.tokens_status'] = 'suspect')            AS suspect
FROM default.otel_logs
WHERE Body = 'sweep.outcome'
  AND mapContains(LogAttributes, 'loom.sweep_id');
