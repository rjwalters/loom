-- Story-points throughput analytics: the durable rollup that outlives the raw
-- signal TTL (Issue #9433, epic #9429; pattern: #8665). The questions it
-- answers live in `story-points-throughput-queries.sql` (PT1..PT7).
--
-- BACKEND-NEUTRAL. Every statement here runs unchanged on ClickStack's
-- ClickHouse and on SigNoz's ClickHouse. The only backend-specific artifact is
-- the `loom_analytics.raw_ship_story_points` view, defined once per backend in
-- `clickstack/story-points-extract.sql` / `signoz/story-points-extract.sql`;
-- it is the seam, and it exposes exactly the column list the backfill below
-- inserts. Create that view FIRST, then run this file.
--
-- The question definitions -- in particular "a failed sweep lands nothing" and
-- "missing points are a data gap, never zero" -- and the retention decision
-- this table inherits are in `story-points-throughput-questions.md`. Read that
-- before reading a number out of any of these tables.
--
--   # once per deployment; the backfill needs a window bound:
--   clickhouse-client --param_since='2026-09-15 00:00:00' \
--                     --param_until='2026-09-22 00:00:00' \
--                     --queries-file story-points-throughput-rollup.sql
--
-- Delivery is at-least-once and a backfill may be re-run, so
-- `ship_story_points` is a ReplacingMergeTree and every read goes through the
-- `ship_points` view, which applies FINAL. Never read the base table directly.

-- ===========================================================================
-- 1. Durable storage
-- ===========================================================================

CREATE DATABASE IF NOT EXISTS loom_analytics;

-- One row per terminal sweep, carrying that sweep's story-point estimate.
-- Deliberately narrow: identity, the terminal result, and the estimate. Failed
-- sweeps are stored on purpose -- the questions count them beside the landings
-- (the ship-vs-fail separation), and only the dedup view decides that a failed
-- sweep's estimate is not landed work. `story_points` is NULL when the record
-- carried no `loom.story_points` attribute (unsized, stacked or
-- out-of-vocabulary label set); `points_present` keeps "the key rode the
-- record but did not parse" visible as its own defect, distinct from "never
-- sized". Neither is ever a measured zero.
--
-- TTL 400 days, the same figure and for the same reason as the cycle-time
-- rollup (`cycle-time-rollup.sql`): long enough for a year-over-year
-- comparison, bounded so the table cannot grow without limit, independent of
-- the raw signal TTL by design.
CREATE TABLE IF NOT EXISTS loom_analytics.ship_story_points
(
    finished_at     DateTime64(3),
    repo            LowCardinality(String),
    sweep_id        String,
    issue           UInt32,
    host_id         LowCardinality(String),
    repo_visibility LowCardinality(String),
    result          LowCardinality(String),
    story_points    Nullable(UInt8),
    points_present  UInt8
)
ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMM(finished_at)
ORDER BY (repo, sweep_id, finished_at)
TTL toDateTime(finished_at) + INTERVAL 400 DAY;

-- Ships, deduplicated, with the derived per-ship quantities every question
-- needs.
--
-- `points_landed` is the ONE column the questions sum: NULL unless the sweep
-- both succeeded and carried an estimate, so a failed sweep -- which lands
-- nothing -- can never contribute to a landed-points total, and an unlabeled
-- success can never contribute a zero-point landing. Sums over anything else
-- are a defect, and PT7/PT6 exist to make that loud.
--
-- `measured_point_*` map the estimate onto the story-points experiment's
-- measured bucket ratios (#9433's experiment input: raw Fibonacci labels are
-- ordinal, not a unit, so a size-weighted sum must not sum them raw). They are
-- PROVISIONAL and a deliberate mirror of `sweep-facts/landed-size.sql`'s
-- `measured_points` (#9466) -- change both tables together until #9434's
-- calibration collapses them into one point value per class. A label outside
-- the vocabulary has no measured point: NULL, never an extrapolation.
CREATE OR REPLACE VIEW loom_analytics.ship_points AS
SELECT
    finished_at,
    repo,
    sweep_id,
    issue,
    host_id,
    repo_visibility,
    result,
    story_points,
    points_present,
    if(result = 'success', story_points, NULL) AS points_landed,
    if(result = 'success',
       multiIf(story_points = 1,  1.0,
               story_points = 2,  1.3,
               story_points = 3,  2.2,
               story_points = 5,  3.4,
               story_points = 8,  5.1,
               story_points = 13, 8.2,
               NULL), NULL)                    AS measured_point_tokens,
    if(result = 'success',
       multiIf(story_points = 1,  1.0,
               story_points = 2,  8.5,
               story_points = 3,  21.0,
               story_points = 5,  47.0,
               story_points = 8,  82.0,
               story_points = 13, 197.0,
               NULL), NULL)                    AS measured_point_lines
FROM loom_analytics.ship_story_points FINAL;

-- ===========================================================================
-- 2. Ingest
-- ===========================================================================

-- The one write path. Column list is explicit so a schema change fails loudly
-- here rather than silently shifting values into neighbouring columns.
-- Idempotent: re-running over an already-ingested window collapses on merge
-- (ReplacingMergeTree keyed on repo, sweep_id, finished_at) and is invisible
-- to every reader (the `ship_points` view applies FINAL), so a backfill can
-- safely overlap a previous one and a replayed delivery costs nothing.
INSERT INTO loom_analytics.ship_story_points
    (finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
     story_points, points_present)
SELECT
    finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
    story_points, points_present
FROM loom_analytics.raw_ship_story_points
WHERE finished_at >= {since:DateTime} AND finished_at < {until:DateTime};

-- Standing incremental ingest. `APPEND` adds each refresh's rows instead of
-- replacing the table, which is what keeps history alive past the raw TTL. The
-- three-hour lookback is deliberately far wider than the refresh interval: a
-- gateway outage replays a backlog late, and re-reading an already-ingested
-- row costs nothing (same idempotence as the backfill above).
CREATE MATERIALIZED VIEW IF NOT EXISTS loom_analytics.ship_story_points_refresh
REFRESH EVERY 1 HOUR APPEND TO loom_analytics.ship_story_points
    (finished_at DateTime64(3), repo LowCardinality(String), sweep_id String,
     issue UInt32, host_id LowCardinality(String),
     repo_visibility LowCardinality(String), result LowCardinality(String),
     story_points Nullable(UInt8), points_present UInt8)
AS SELECT
    finished_at, repo, sweep_id, issue, host_id, repo_visibility, result,
    story_points, points_present
FROM loom_analytics.raw_ship_story_points
WHERE finished_at >= now() - INTERVAL 3 HOUR;
