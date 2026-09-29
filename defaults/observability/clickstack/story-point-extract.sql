-- ClickStack half of the story-point calibration seam (Issue #9430; pattern:
-- #8665's cycle-time set, whose extract files this mirrors).
--
-- This file defines exactly ONE object: `loom_analytics.raw_landing_cost`, the
-- view that turns this backend's raw log rows into the normalized cost columns
-- `../story-point-queries.sql` answers SP1-SP5 from, with the `clean_landing`
-- verdict defined once inside it. Everything about the filter, the column
-- order and the absence contract is documented in the SigNoz twin
-- (`../signoz/story-point-extract.sql`) and in `../story-point-questions.md`;
-- `loom-daemon/tests/story_point_artifacts.rs` fails in ordinary CI if the two
-- views stop agreeing on the column list.
--
--   docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
--     clickhouse-client --multiquery < story-point-extract.sql
--
-- STATUS: not executed against a live ClickStack deployment. The ClickStack
-- half carries the live-verified precedent for these conventions
-- (`cycle_time_clickhouse.rs`), and the SigNoz twin's SELECT body ran against
-- the live fleet SigNoz on 2026-09-29 — see `../signoz/evidence.md`.
--
-- `Body` carries the record kind (the OTLP mapping sets the event name AND the
-- body), and `loom.sweep_id` must be present: a row without it cannot be a
-- ship identity. `loom.phase_durations` arrives as a JSON string in the
-- String-valued attribute map; extracting to a NAMED tuple reads entries by
-- key, so the exporter's field order cannot silently transpose them. Every
-- optional field becomes NULL when its key is ABSENT, never '' or 0. The view
-- carries the full terminal population and collapses at-least-once
-- redeliveries to one row per (repo, sweep_id), for the reasons the SigNoz
-- twin documents.

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_landing_cost AS
WITH
    JSONExtract(LogAttributes['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations,
    length(arrayFilter(t -> t.1 = 'judge', phase_durations))  AS judge_entries_row,
    length(arrayFilter(t -> t.1 = 'doctor', phase_durations)) AS doctor_entries_row,
    coalesce(
        if(mapContains(LogAttributes, 'loom.doctor_cycles'),
           toInt64OrZero(LogAttributes['loom.doctor_cycles']) > 0, NULL),
        doctor_entries_row > 0)                               AS doctor_engaged_row,
    -- The clean-landing filter (Issue #9430): landed (success + a PR —
    -- pr_number presence is exactly disposition = 'landed', #9441), no repair,
    -- exactly one judge entry. Defined once; SP3 counts what it excludes.
    toUInt8(
        LogAttributes['loom.result'] = 'success'
        AND isNotNull(toUInt32OrNull(LogAttributes['loom.pr_number']))
        AND doctor_engaged_row = 0
        AND judge_entries_row = 1)                           AS clean_row
SELECT
    any(toDateTime64(Timestamp, 3))                               AS finished_at,
    LogAttributes['loom.repo']                                    AS repo,
    LogAttributes['loom.sweep_id']                                AS sweep_id,
    any(toUInt32OrZero(LogAttributes['loom.issue']))              AS issue,
    any(LogAttributes['loom.result'])                             AS result,
    any(toUInt32OrNull(LogAttributes['loom.pr_number']))          AS pr_number,
    any(toUInt32OrNull(LogAttributes['loom.doctor_cycles']))      AS doctor_cycles,
    any(judge_entries_row)                                        AS judge_entries,
    toUInt8(any(mapContains(LogAttributes, 'loom.phase_durations')))
                                                                  AS phase_breakdown_present,
    any(doctor_engaged_row)                                       AS doctor_engaged,
    any(clean_row)                                                AS clean_landing,
    any(if(mapContains(LogAttributes, 'loom.tokens_status'),
       LogAttributes['loom.tokens_status'], NULL))                AS tokens_status,
    any(toInt64OrNull(LogAttributes['loom.tokens_in']))           AS tokens_in,
    any(toInt64OrNull(LogAttributes['loom.tokens_out']))          AS tokens_out,
    any(toInt64OrNull(LogAttributes['loom.lines_added']))         AS lines_added,
    any(toInt64OrNull(LogAttributes['loom.lines_deleted']))       AS lines_deleted,
    any(toInt64OrZero(LogAttributes['loom.total_duration_sec']))  AS total_duration_sec,
    any(if(mapContains(LogAttributes, 'loom.model'),
       LogAttributes['loom.model'], NULL))                        AS model
FROM default.otel_logs
WHERE Body = 'sweep.outcome'
  AND mapContains(LogAttributes, 'loom.sweep_id')
GROUP BY repo, sweep_id;
