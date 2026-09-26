-- Cycle-time analytics: the canonical question set CT1..CT8.
--
-- BACKEND-NEUTRAL: these run unchanged on ClickStack's ClickHouse and on
-- SigNoz's ClickHouse, because they read only `loom_analytics.*`, which
-- `cycle-time-rollup.sql` creates and each backend's own
-- `cycle-time-extract.sql` feeds. Run `cycle-time-rollup.sql` first.
--
-- Every question, its exact definition, and what this set deliberately cannot
-- answer are in `cycle-time-questions.md`. Do not read a number out of here
-- without it -- especially "absent vs. zero".
--
-- Windows are BOUND, never edited in: rewriting a literal into the SQL is the
-- hand-written-SQL habit this file exists to abolish.
--
--   clickhouse-client --param_since='2026-09-15 00:00:00' \
--                     --param_until='2026-09-22 00:00:00' --param_top_n=10 \
--                     --queries-file cycle-time-queries.sql
--
-- All eight answers come back in one pass, in CT order.

-- CT1. Top N slowest ships in the window, and the phase that dominated each.
-- This is the headline question: "top 10 slowest ships last week, dominated by
-- which phase". An empty `dominant_phase` is a ship with no phase breakdown,
-- not a ship with no slow phase — read CT7 beside this.
SELECT
    repo,
    issue,
    pr_number,
    sweep_id,
    finished_at,
    result,
    total_duration_sec,
    dominant_phase,
    dominant_phase_sec,
    dominant_phase_pct,
    runtime,
    model
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
  AND result = 'success'
ORDER BY total_duration_sec DESC, repo, sweep_id
LIMIT {top_n:UInt32};

-- CT2. Where the time actually goes: per-phase duration distribution and the
-- share of measured cycle time each phase accounts for.
SELECT
    phase,
    count() AS occurrences,
    uniqExact(repo, sweep_id) AS ships,
    round(quantile(0.5)(phase_sec)) AS p50_sec,
    round(quantile(0.9)(phase_sec)) AS p90_sec,
    round(quantile(0.95)(phase_sec)) AS p95_sec,
    max(phase_sec) AS max_sec,
    sum(phase_sec) AS total_sec,
    round(100 * sum(phase_sec) / sum(sum(phase_sec)) OVER (), 1) AS pct_of_measured
FROM loom_analytics.ship_phase
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY phase
ORDER BY total_sec DESC;

-- CT3. Which repos ship slowly.
SELECT
    repo,
    count() AS ships,
    countIf(result = 'success') AS succeeded,
    round(100 * countIf(result = 'success') / count(), 1) AS success_pct,
    round(quantile(0.5)(total_duration_sec)) AS p50_sec,
    round(quantile(0.9)(total_duration_sec)) AS p90_sec,
    round(quantile(0.95)(total_duration_sec)) AS p95_sec,
    max(total_duration_sec) AS max_sec
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY repo
ORDER BY p90_sec DESC, repo;

-- CT4. Which execution configuration is the bottleneck. A NULL cell is an
-- unreported setting; it is never collapsed into a "default" bucket.
SELECT
    runtime,
    provider,
    model,
    effort,
    count() AS ships,
    countIf(result != 'success') AS not_succeeded,
    round(quantile(0.5)(total_duration_sec)) AS p50_sec,
    round(quantile(0.9)(total_duration_sec)) AS p90_sec,
    round(avg(dominant_phase_sec)) AS avg_dominant_phase_sec,
    topK(1)(dominant_phase) AS most_common_dominant_phase
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY runtime, provider, model, effort
ORDER BY p90_sec DESC;

-- CT5. What repair costs: ships that engaged Doctor against those that did not.
SELECT
    doctor_engaged,
    count() AS ships,
    round(100 * count() / sum(count()) OVER (), 1) AS pct_of_ships,
    round(quantile(0.5)(total_duration_sec)) AS p50_sec,
    round(quantile(0.9)(total_duration_sec)) AS p90_sec,
    round(avg(arraySum(arrayMap((n, d) -> if(n = 'judge', d, 0), phases, phase_durations_sec)))) AS avg_judge_sec,
    round(avg(arraySum(arrayMap((n, d) -> if(n = 'doctor', d, 0), phases, phase_durations_sec)))) AS avg_doctor_sec
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY doctor_engaged
ORDER BY doctor_engaged;

-- CT6. Is the fleet getting slower? The question the 7-day raw TTL makes
-- unanswerable and this rollup exists to answer; run it over months.
SELECT
    toStartOfWeek(finished_at, 1) AS week,
    count() AS ships,
    countIf(result = 'success') AS succeeded,
    round(quantile(0.5)(total_duration_sec)) AS p50_sec,
    round(quantile(0.9)(total_duration_sec)) AS p90_sec,
    topK(1)(dominant_phase) AS most_common_dominant_phase
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
GROUP BY week
ORDER BY week;

-- CT7. Where the data is missing. Read beside every grouped answer above: a
-- blank cell there is one of these, never a measured zero.
SELECT
    count() AS ships,
    countIf(phase_durations_present = 0) AS without_phase_breakdown,
    countIf(runtime IS NULL) AS without_runtime,
    countIf(provider IS NULL) AS without_provider,
    countIf(model IS NULL) AS without_model,
    countIf(effort IS NULL) AS without_effort,
    countIf(pr_number IS NULL) AS without_pr_number,
    countIf(doctor_cycles IS NULL) AS without_doctor_cycles,
    countIf(total_duration_sec <= 0) AS without_positive_total
FROM loom_analytics.ship
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime};

-- CT8. Is the rollup faithful to the raw logs? The join is FULL OUTER on
-- purpose: each side has a legitimate reason to hold a ship the other does not,
-- and the two readings are not interchangeable.
--
--   missing_from_rollup   raw has it, the rollup does not — REAL DRIFT. Repair
--                         by re-running the backfill over the window.
--   rollup_beyond_raw     the rollup has it and raw no longer does — the whole
--                         point of the rollup, not drift. Outside the raw TTL
--                         this is every ship in the window.
--   mismatched_totals     must ALWAYS be 0, in every window. A non-zero value
--                         means the extraction and the stored row disagree
--                         about the same sweep.
SELECT
    countIf(raw.sweep_id != '') AS raw_ships,
    countIf(rollup.sweep_id != '') AS rolled_up_ships,
    countIf(raw.sweep_id != '' AND rollup.sweep_id = '') AS missing_from_rollup,
    countIf(raw.sweep_id = '' AND rollup.sweep_id != '') AS rollup_beyond_raw,
    countIf(raw.sweep_id != '' AND rollup.sweep_id != ''
            AND raw.total_duration_sec != rollup.total_duration_sec) AS mismatched_totals
FROM (
    SELECT repo, sweep_id, any(total_duration_sec) AS total_duration_sec
    FROM loom_analytics.raw_ship_outcome
    WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
    GROUP BY repo, sweep_id
) AS raw
FULL OUTER JOIN (
    SELECT repo, sweep_id, total_duration_sec
    FROM loom_analytics.ship
    WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime}
) AS rollup
ON raw.repo = rollup.repo AND raw.sweep_id = rollup.sweep_id;
