-- Cycle-time analytics: the durable rollup that outlives the raw signal TTL.
-- The questions it answers live in `cycle-time-queries.sql` (CT1..CT8).
--
-- BACKEND-NEUTRAL. Every statement here runs unchanged on ClickStack's
-- ClickHouse and on SigNoz's ClickHouse. The only backend-specific artifact is
-- the `loom_analytics.raw_ship_outcome` view, defined once per backend in
-- `clickstack/cycle-time-extract.sql` / `signoz/cycle-time-extract.sql`; it is
-- the seam, and it exposes exactly the column list the backfill below inserts.
-- Create that view FIRST, then run this file.
--
-- The question definitions, and the reason this rollup exists rather than a
-- raised raw TTL, are in `cycle-time-questions.md`. Read that before reading a
-- number out of any of these tables — in particular "absent vs. zero".
--
--   # once per deployment; the backfill needs a window bound:
--   clickhouse-client --param_since='2026-09-15 00:00:00' \
--                     --param_until='2026-09-22 00:00:00' \
--                     --queries-file cycle-time-rollup.sql
--
-- Delivery is at-least-once and a backfill may be re-run, so `ship_cycle_time`
-- is a ReplacingMergeTree and every read goes through the `ship` view, which
-- applies FINAL. Never read the base table directly.

-- ===========================================================================
-- 1. Durable storage
-- ===========================================================================

CREATE DATABASE IF NOT EXISTS loom_analytics;

-- One row per ship. Deliberately narrow: identity, the terminal result, the
-- execution configuration, and the durations. Anything unmeasured is NULL —
-- never 0, never ''. `phase_durations_present` distinguishes "the outcome
-- carried no phase breakdown" from "the breakdown was empty".
--
-- TTL 400 days: long enough for a year-over-year comparison, bounded so the
-- table cannot grow without limit. It is independent of the raw signal TTL by
-- design; see cycle-time-questions.md "Retention decision".
CREATE TABLE IF NOT EXISTS loom_analytics.ship_cycle_time
(
    finished_at             DateTime64(3),
    repo                    LowCardinality(String),
    sweep_id                String,
    issue                   UInt32,
    host_id                 LowCardinality(String),
    repo_visibility         LowCardinality(String),
    result                  LowCardinality(String),
    total_duration_sec      Int64,
    phases                  Array(LowCardinality(String)),
    phase_durations_sec     Array(Int64),
    phase_durations_present UInt8,
    pr_number               Nullable(UInt32),
    doctor_cycles           Nullable(UInt32),
    failure_class           Nullable(String),
    runtime                 Nullable(String),
    provider                Nullable(String),
    model                   Nullable(String),
    configured_model        Nullable(String),
    effort                  Nullable(String)
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(finished_at)
ORDER BY (repo, sweep_id, finished_at)
TTL toDateTime(finished_at) + INTERVAL 400 DAY;

-- Ships, deduplicated, with the derived per-ship quantities every question
-- needs. `dominant_phase` sums a phase name's occurrences first: a repair loop
-- emits `judge` twice and that is one bottleneck, not two smaller ones.
CREATE OR REPLACE VIEW loom_analytics.ship AS
WITH
    arrayDistinct(phases) AS phase_names,
    arrayMap(p -> arraySum(arrayMap((n, d) -> if(n = p, d, 0), phases, phase_durations_sec)),
             phase_names) AS phase_totals,
    arrayReverseSort(x -> x.2, arrayZip(phase_names, phase_totals)) AS ranked
SELECT
    finished_at,
    repo,
    sweep_id,
    issue,
    host_id,
    repo_visibility,
    result,
    total_duration_sec,
    phases,
    phase_durations_sec,
    phase_durations_present,
    pr_number,
    doctor_cycles,
    failure_class,
    runtime,
    provider,
    model,
    configured_model,
    effort,
    -- NULL, not '' / 0, when this ship carried no phase breakdown at all.
    if(phase_durations_present = 0, NULL, ranked[1].1) AS dominant_phase,
    if(phase_durations_present = 0, NULL, ranked[1].2) AS dominant_phase_sec,
    if(phase_durations_present = 0, NULL,
       round(100 * ranked[1].2 / nullIf(total_duration_sec, 0), 1)) AS dominant_phase_pct,
    -- `doctor_cycles` is the authoritative signal; the phase-array fallback is
    -- the same approximation loom-daemon's own summary uses when it is absent.
    coalesce(doctor_cycles > 0, has(phases, 'doctor')) AS doctor_engaged
FROM loom_analytics.ship_cycle_time FINAL;

-- One row per phase occurrence, for the distribution questions.
CREATE OR REPLACE VIEW loom_analytics.ship_phase AS
SELECT
    finished_at, repo, sweep_id, issue, result, total_duration_sec,
    runtime, provider, model, effort,
    phase,
    phase_sec
FROM loom_analytics.ship
ARRAY JOIN
    phases              AS phase,
    phase_durations_sec AS phase_sec;

-- ===========================================================================
-- 2. Ingest
-- ===========================================================================

-- The one write path. Column list is explicit so a schema change fails loudly
-- here rather than silently shifting values into neighbouring columns.
-- Idempotent: re-running over an already-ingested window collapses on merge
-- (ReplacingMergeTree) and is invisible to every reader (the `ship` view is
-- FINAL), so a backfill can safely overlap a previous one.
--
-- Run for any window still present in the raw table, e.g. after creating the
-- rollup on a deployment that already has a few days of raw signal.
INSERT INTO loom_analytics.ship_cycle_time
    (finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
     total_duration_sec, phases, phase_durations_sec, phase_durations_present,
     pr_number, doctor_cycles, failure_class, runtime, provider, model,
     configured_model, effort)
SELECT
    finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
    total_duration_sec, phases, phase_durations_sec, phase_durations_present,
    pr_number, doctor_cycles, failure_class, runtime, provider, model,
    configured_model, effort
FROM loom_analytics.raw_ship_outcome
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime};

-- Standing incremental ingest. `APPEND` adds each refresh's rows instead of
-- replacing the table, which is what keeps history alive past the raw TTL. The
-- three-hour lookback is deliberately far wider than the refresh interval: a
-- gateway outage replays a backlog late, and re-reading an already-ingested
-- row costs nothing (same idempotence as the backfill above).
CREATE MATERIALIZED VIEW IF NOT EXISTS loom_analytics.ship_cycle_time_refresh
REFRESH EVERY 1 HOUR APPEND TO loom_analytics.ship_cycle_time
    (finished_at DateTime64(3), repo LowCardinality(String), sweep_id String,
     issue UInt32, host_id LowCardinality(String),
     repo_visibility LowCardinality(String), result LowCardinality(String),
     total_duration_sec Int64, phases Array(LowCardinality(String)),
     phase_durations_sec Array(Int64), phase_durations_present UInt8,
     pr_number Nullable(UInt32), doctor_cycles Nullable(UInt32),
     failure_class Nullable(String), runtime Nullable(String),
     provider Nullable(String), model Nullable(String),
     configured_model Nullable(String), effort Nullable(String))
AS SELECT
    finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
    total_duration_sec, phases, phase_durations_sec, phase_durations_present,
    pr_number, doctor_cycles, failure_class, runtime, provider, model,
    configured_model, effort
FROM loom_analytics.raw_ship_outcome
WHERE finished_at >= now() - INTERVAL 3 HOUR;
