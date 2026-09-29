-- ClickStack half of the story-points throughput seam (Issue #9433, epic
-- #9429; pattern: #8665).
--
-- This file defines exactly ONE object: `loom_analytics.raw_ship_story_points`,
-- the view that turns this backend's raw log rows into the normalized per-ship
-- points columns `../story-points-throughput-rollup.sql` ingests. Everything
-- else -- the durable table, the refresh, and every PT query -- is
-- backend-neutral and lives beside it. Keeping the per-backend surface to one
-- view is what makes SigNoz/ClickStack parity a property of the design rather
-- than of two hand-synchronized query sets.
--
--   docker compose --env-file /absolute/private/clickstack.env exec -T clickstack \
--     clickhouse-client --multiquery < story-points-extract.sql
--   # then, once, with a window for the backfill:
--   … clickhouse-client --param_since=… --param_until=… \
--       --queries-file ../story-points-throughput-rollup.sql
--
-- Column list and order are load-bearing: the rollup inserts them by name, and
-- `loom-daemon/tests/story_points_throughput_artifacts.rs` fails in ordinary CI
-- if this view and the SigNoz one stop agreeing on them.
--
-- `loom.story_points` (#9432) is a NUMERIC attribute present only when the
-- issue carried exactly one legal `points:*` label -- an unsized, stacked or
-- out-of-vocabulary label set emits NO attribute at all. Absence is therefore
-- the data-gap signal (`points_present = 0`), never a measured zero, and this
-- view never coerces a missing key to 0: `story_points` is read
-- `toUInt8OrNull`, not `toUInt8OrZero`, for exactly that reason. Dialect
-- conventions otherwise are the cycle-time extraction's, unchanged: `Body`
-- carries the record kind, `loom.sweep_id` must be present (a row without it
-- cannot be a ship identity), and absent keys yield NULL through `mapContains`
-- guards rather than '' map subscripts.

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_ship_story_points AS
SELECT
    toDateTime64(Timestamp, 3)                                AS finished_at,
    LogAttributes['loom.repo']                                AS repo,
    LogAttributes['loom.sweep_id']                            AS sweep_id,
    toUInt32OrZero(LogAttributes['loom.issue'])               AS issue,
    ResourceAttributes['host.id']                             AS host_id,
    LogAttributes['loom.repo.visibility']                     AS repo_visibility,
    LogAttributes['loom.result']                              AS result,
    toUInt8OrNull(LogAttributes['loom.story_points'])         AS story_points,
    toUInt8(mapContains(LogAttributes, 'loom.story_points'))  AS points_present
FROM default.otel_logs
WHERE Body = 'sweep.outcome'
  AND mapContains(LogAttributes, 'loom.sweep_id');
