-- Story-points throughput analytics: the canonical question set PT1..PT7
-- (Issue #9433, epic #9429).
--
-- BACKEND-NEUTRAL: these run unchanged on ClickStack's ClickHouse and on
-- SigNoz's ClickHouse, because they read only `loom_analytics.ship_points`,
-- which `story-points-throughput-rollup.sql` creates and each backend's own
-- `story-points-extract.sql` feeds. Run `story-points-throughput-rollup.sql`
-- first.
--
-- Every question, its exact definition, and what this set deliberately cannot
-- answer are in `story-points-throughput-questions.md`. Do not read a number
-- out of here without it -- especially "a failed sweep lands nothing" and
-- "missing points are a data gap, never zero".
--
-- Windows are BOUND, never edited in: rewriting a literal into the SQL is the
-- hand-written-SQL habit this file exists to abolish.
--
--   clickhouse-client --param_since='2026-09-15 00:00:00' \
--                     --param_until='2026-09-22 00:00:00' \
--                     --queries-file story-points-throughput-queries.sql
--
-- All seven answers come back in one pass, in PT order.

-- PT1. How many points land per day? The headline question the epic exists
-- for. Three readings ride one row on purpose (#9433's experiment input):
-- landings (the issue count -- volume alone explained R^2 0.83 of daily
-- delivered size), forecast_points (the raw label sum -- a size-mix-weighted
-- volume index, NOT a unit), and the measured point values (the experiment's
-- bucket ratios, provisional until #9434). An unlabeled success is a data
-- gap, never a zero-point landing; a failed sweep lands nothing and is
-- counted beside the landings, never summed into them.
SELECT
    toStartOfDay(finished_at)                            AS day,
    countIf(result = 'success')                          AS landings,
    countIf(result != 'success')                         AS failed_sweeps,
    sum(points_landed)                                   AS forecast_points,
    round(sum(measured_point_tokens), 1)                 AS measured_points_tokens,
    round(sum(measured_point_lines), 1)                  AS measured_points_lines,
    quantile(0.5)(points_landed)                         AS median_points,
    countIf(result = 'success' AND points_present = 0)   AS unlabeled_landings
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY day
ORDER BY day;

-- PT2. Points landed per ISO-week. `points_per_day` divides by days OBSERVED
-- (days with any terminal sweep in the week), not calendar days, so a partial
-- week at either edge of the window reports its real rate instead of a
-- diluted one.
SELECT
    toStartOfWeek(finished_at, 1)                        AS week,
    countIf(result = 'success')                          AS landings,
    countIf(result != 'success')                         AS failed_sweeps,
    sum(points_landed)                                   AS forecast_points,
    round(sum(measured_point_tokens), 1)                 AS measured_points_tokens,
    round(sum(measured_point_lines), 1)                  AS measured_points_lines,
    round(sum(points_landed)
          / uniqExact(toStartOfDay(finished_at)), 1)     AS points_per_day,
    countIf(result = 'success' AND points_present = 0)   AS unlabeled_landings
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY week
ORDER BY week;

-- PT3. Is throughput trending over months? The question the 7-day raw TTL
-- makes unanswerable and this rollup exists to answer; run it over months.
-- Read beside PT6: a falling forecast_points with a falling labeled share is
-- curation falling behind (the numerator undercounting), not the fleet
-- slowing down.
SELECT
    toStartOfMonth(finished_at)                          AS month,
    countIf(result = 'success')                          AS landings,
    sum(points_landed)                                   AS forecast_points,
    round(sum(measured_point_tokens), 1)                 AS measured_points_tokens,
    round(sum(measured_point_lines), 1)                  AS measured_points_lines,
    round(sum(points_landed)
          / uniqExact(toStartOfDay(finished_at)), 1)     AS points_per_day,
    round(100 * countIf(result = 'success' AND points_present = 1)
               / nullIf(countIf(result = 'success'), 0), 1) AS labeled_pct
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY month
ORDER BY month;

-- PT4. Which repos land the most sized work? `points_per_day` is over the
-- whole bound window (days observed per repo), so a repo that ships in bursts
-- is not inflated by a short window; `median_points` carries the size mix a
-- sum alone hides.
SELECT
    repo                                                 AS repo,
    countIf(result = 'success')                          AS landings,
    countIf(result != 'success')                         AS failed_sweeps,
    sum(points_landed)                                   AS forecast_points,
    round(sum(measured_point_tokens), 1)                 AS measured_points_tokens,
    round(sum(measured_point_lines), 1)                  AS measured_points_lines,
    quantile(0.5)(points_landed)                         AS median_points,
    round(sum(points_landed)
          / uniqExact(toStartOfDay(finished_at)), 1)     AS points_per_day,
    countIf(result = 'success' AND points_present = 0)   AS unlabeled_landings
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY repo
ORDER BY forecast_points DESC, repo;

-- PT5. What is the size mix of the landings? The labels are ordinal, not a
-- unit, so the mix is the signal a single sum hides -- read it beside PT1. A
-- value outside the closed vocabulary appears here as its own class:
-- visible, never folded onto a neighbouring bucket (PT6 counts it too).
SELECT
    story_points                                         AS size_class,
    count()                                              AS landings,
    round(100 * count() / sum(count()) OVER (), 1)       AS pct_of_labeled_landings
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND result = 'success'
  AND story_points IS NOT NULL
GROUP BY size_class
ORDER BY size_class;

-- PT6. Where is the data missing? The CT7 discipline: read this beside every
-- summed answer above. An unlabeled landing is a data-gap count, never a
-- zero-point landing; a failed sweep is excluded from every sum on purpose
-- (it landed nothing) and counted here instead. Telemetry cannot split
-- "never sized" from "defectively sized" -- both omit the attribute -- the
-- daemon's warn log is where that split lives.
SELECT
    count()                                              AS sweeps,
    countIf(result = 'success')                          AS succeeded,
    countIf(result != 'success')                         AS failed_sweeps,
    countIf(result = 'success' AND points_present = 0)   AS landings_without_points,
    countIf(result != 'success' AND points_present = 0)  AS failed_without_points,
    countIf(points_present = 1 AND story_points IS NULL) AS points_key_present_but_unreadable,
    countIf(story_points NOT IN (1, 2, 3, 5, 8, 13))     AS out_of_vocabulary_values
FROM loom_analytics.ship_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime};

-- PT7. Is the rollup faithful to the raw logs? The join is FULL OUTER on
-- purpose (the CT8 discipline): each side has a legitimate reason to hold a
-- ship the other does not, and the two readings are not interchangeable.
--
--   missing_from_rollup   raw has it, the rollup does not -- REAL DRIFT.
--                         Repair by re-running the backfill over the window.
--   rollup_beyond_raw     the rollup has it and raw no longer does -- the
--                         whole point of the rollup, not drift. Outside the
--                         raw TTL this is every ship in the window.
--   mismatched_points /   must ALWAYS be 0, in every window. A non-zero value
--   mismatched_result     means the extraction and the stored row disagree
--                         about the same sweep. Points compare through a -1
--                         sentinel because NULL != NULL in ClickHouse, and
--                         -1 is not a legal label value.
SELECT
    countIf(raw.sweep_id != '')                          AS raw_sweeps,
    countIf(rollup.sweep_id != '')                       AS rolled_up_sweeps,
    countIf(raw.sweep_id != '' AND rollup.sweep_id = '') AS missing_from_rollup,
    countIf(raw.sweep_id = '' AND rollup.sweep_id != '') AS rollup_beyond_raw,
    countIf(raw.sweep_id != '' AND rollup.sweep_id != ''
            AND ifNull(raw.story_points, -1)
                != ifNull(rollup.story_points, -1))      AS mismatched_points,
    countIf(raw.sweep_id != '' AND rollup.sweep_id != ''
            AND raw.result != rollup.result)             AS mismatched_result
FROM (
    SELECT repo, sweep_id, any(story_points) AS story_points, any(result) AS result
    FROM loom_analytics.raw_ship_story_points
    WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
    GROUP BY repo, sweep_id
) AS raw
FULL OUTER JOIN (
    SELECT repo, sweep_id, story_points, result
    FROM loom_analytics.ship_points
    WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
) AS rollup
ON raw.repo = rollup.repo AND raw.sweep_id = rollup.sweep_id;
