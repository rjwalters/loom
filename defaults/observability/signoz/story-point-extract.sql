-- SigNoz half of the story-point calibration seam (Issue #9430; pattern:
-- #8665's cycle-time set, whose extract files this mirrors).
--
-- It defines exactly ONE object, `loom_analytics.raw_landing_cost`: one row
-- per terminal ship, with the three cost measures the Fibonacci rubric anchors
-- on and the `clean_landing` verdict that selects the comparable population.
-- `../story-point-queries.sql` (SP1-SP5) reads only this view, and
-- `../clickstack/story-point-extract.sql` defines the same columns from the
-- other backend's log table, so both backends answer the canonical story-point
-- question set from one definition. `loom-daemon/tests/story_point_artifacts.rs`
-- fails in ordinary CI if the two views stop agreeing on that column list, or
-- if this file reads an attribute key the gateway's keep_keys strips — a
-- stripped key yields NULL forever, silently, which is indistinguishable from
-- "the fleet landed nothing".
--
--   docker exec -i harness-ops-signoz-clickhouse clickhouse-client \
--     --multiquery < story-point-extract.sql
--
-- STATUS: the view's SELECT body was executed against the live fleet SigNoz
-- (inlined, without the CREATE, under that host's read-only access) on
-- 2026-09-29 — see `../signoz/evidence.md` "Story-point calibration, executed
-- live". The CREATE itself is the only unexecuted statement.
--
-- Two deliberate differences from the ClickStack view, both forced by SigNoz's
-- schema rather than chosen (the same two the cycle-time extraction documents):
--
--  1. SigNoz splits attributes by VALUE TYPE across `attributes_string` /
--     `attributes_number`, where the ClickHouse OTel schema keeps one
--     String-valued map. Which map an integer attribute lands in is a property
--     of the pinned ingester, so every numeric read below tries the number map
--     first and falls back to parsing the string map.
--  2. Reads go through `distributed_logs_v2`, the query-facing table.
--
-- Two deliberate differences from the sibling extraction views, both forced by
-- this question set's design:
--
--  3. The view carries the FULL terminal population, not just clean landings:
--     SP3 counts the exclusions, so the population they are counted against
--     must live inside the one definition. `clean_landing` is the filter.
--  4. Delivery is at-least-once (a replayed export duplicates rows), and this
--     question set has no rollup of its own yet (#9433/#9446 own durability),
--     so the view itself collapses to one row per (repo, sweep_id) — the
--     identity cycle-time's CT8 reconciles on. A distribution computed over
--     un-deduplicated rows would count a replayed landing twice.
--
-- The absence contract is the schema's own: an optional field missing from
-- BOTH maps is NULL, never '' and never 0.

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_landing_cost AS
WITH
    JSONExtract(attributes_string['loom.phase_durations'],
                'Array(Tuple(phase String, duration_sec Int64))') AS phase_durations,
    length(arrayFilter(t -> t.1 = 'judge', phase_durations))  AS judge_entries_row,
    length(arrayFilter(t -> t.1 = 'doctor', phase_durations)) AS doctor_entries_row,
    -- `doctor_cycles` is the authoritative signal; the phase-array fallback is
    -- the same approximation the cycle-time `ship` view uses when it is absent
    -- (the field exists on records only since 2026-09-18).
    coalesce(
        if(mapContains(attributes_number, 'loom.doctor_cycles'),
           attributes_number['loom.doctor_cycles'] > 0, NULL),
        doctor_entries_row > 0)                               AS doctor_engaged_row,
    -- The clean-landing filter, defined ONCE (Issue #9430):
    --   landed    result = 'success' AND a PR was opened. `loom.disposition`
    --             is stripped by the gateway's keep_keys, so `pr_number`
    --             presence is the landed test here — and it is exactly
    --             equivalent: the telemetry schema's invariant is
    --             disposition = 'landed' <=> pr_number present (#9441).
    --   no repair no Doctor cycle (or, pre-09-18, no doctor phase entry).
    --   one judge exactly one `judge` phase entry — no changes-requested
    --             retry. A record with no phase breakdown cannot be verified
    --             clean (judge_entries reads 0) and is therefore excluded,
    --             counted by SP3, never silently admitted.
    toUInt8(
        attributes_string['loom.result'] = 'success'
        AND (mapContains(attributes_number, 'loom.pr_number')
             OR mapContains(attributes_string, 'loom.pr_number'))
        AND doctor_engaged_row = 0
        AND judge_entries_row = 1)                           AS clean_row
SELECT
    any(toDateTime64(fromUnixTimestamp64Nano(toInt64(timestamp)), 3))
                                                                  AS finished_at,
    attributes_string['loom.repo']                                AS repo,
    attributes_string['loom.sweep_id']                            AS sweep_id,
    any(ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.issue'),
           toUInt32(attributes_number['loom.issue']), NULL),
        toUInt32OrNull(attributes_string['loom.issue'])), 0))      AS issue,
    any(attributes_string['loom.result'])                         AS result,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.pr_number'),
           toUInt32(attributes_number['loom.pr_number']), NULL),
        toUInt32OrNull(attributes_string['loom.pr_number'])))     AS pr_number,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.doctor_cycles'),
           toUInt32(attributes_number['loom.doctor_cycles']), NULL),
        toUInt32OrNull(attributes_string['loom.doctor_cycles']))) AS doctor_cycles,
    any(judge_entries_row)                                        AS judge_entries,
    toUInt8(any(mapContains(attributes_string, 'loom.phase_durations')))
                                                                  AS phase_breakdown_present,
    any(doctor_engaged_row)                                       AS doctor_engaged,
    any(clean_row)                                                AS clean_landing,
    any(if(mapContains(attributes_string, 'loom.tokens_status'),
       attributes_string['loom.tokens_status'], NULL))            AS tokens_status,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.tokens_in'),
           toInt64(attributes_number['loom.tokens_in']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_in'])))      AS tokens_in,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.tokens_out'),
           toInt64(attributes_number['loom.tokens_out']), NULL),
        toInt64OrNull(attributes_string['loom.tokens_out'])))     AS tokens_out,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.lines_added'),
           toInt64(attributes_number['loom.lines_added']), NULL),
        toInt64OrNull(attributes_string['loom.lines_added'])))    AS lines_added,
    any(coalesce(
        if(mapContains(attributes_number, 'loom.lines_deleted'),
           toInt64(attributes_number['loom.lines_deleted']), NULL),
        toInt64OrNull(attributes_string['loom.lines_deleted'])))  AS lines_deleted,
    any(ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.total_duration_sec'),
           toInt64(attributes_number['loom.total_duration_sec']), NULL),
        toInt64OrNull(attributes_string['loom.total_duration_sec'])), 0))
                                                                  AS total_duration_sec,
    any(if(mapContains(attributes_string, 'loom.model'),
       attributes_string['loom.model'], NULL))                    AS model
FROM signoz_logs.distributed_logs_v2
WHERE body = 'sweep.outcome'
  AND mapContains(attributes_string, 'loom.sweep_id')
GROUP BY repo, sweep_id;
