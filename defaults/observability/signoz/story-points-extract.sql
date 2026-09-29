-- SigNoz half of the story-points throughput seam (Issue #9433, epic #9429;
-- pattern: #8665).
--
-- Mirror of `../clickstack/story-points-extract.sql`: it defines exactly ONE
-- object, `loom_analytics.raw_ship_story_points`, with the same column names,
-- order and meanings. Everything downstream -- the durable rollup table, the
-- refresh, and all seven PT queries -- is the backend-neutral pair of files
-- beside it, so both backends answer the canonical question set from one
-- definition. `loom-daemon/tests/story_points_throughput_artifacts.rs` fails
-- in ordinary CI if the two views stop agreeing on that column list.
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery < story-points-extract.sql
--
-- STATUS: contract-checked in CI, NOT yet executed against a live SigNoz
-- deployment -- the ClickStack side carries the live-verified precedent for
-- these conventions (`loom-daemon/tests/cycle_time_clickhouse.rs`). See
-- `../story-points-throughput-questions.md` § "Verification status" before
-- treating a number produced here as comparable evidence under #8529.
--
-- Two deliberate differences from the ClickStack view, both forced by SigNoz's
-- schema rather than chosen (the same two the cycle-time extraction documents):
--
--  1. SigNoz splits attributes by VALUE TYPE across `attributes_string` /
--     `attributes_number`, where the ClickHouse OTel schema keeps one
--     String-valued map. `loom.story_points` is emitted NUMERIC, so which map
--     it lands in is a property of the pinned ingester: the read below tries
--     the number map first and falls back to parsing the string map. That is
--     not defensive clutter: guessing wrong yields NULL points forever,
--     silently, which is indistinguishable from "nobody sized anything".
--  2. Reads go through `distributed_logs_v2`, the query-facing table.
--
-- Everything else is identical, including the absence contract: a field
-- missing from BOTH maps is NULL, never '' and never 0, and `points_present`
-- keys on either map so "the attribute rode the record but did not parse"
-- stays distinguishable from "never sized".

CREATE DATABASE IF NOT EXISTS loom_analytics;

CREATE OR REPLACE VIEW loom_analytics.raw_ship_story_points AS
SELECT
    toDateTime64(fromUnixTimestamp64Nano(toInt64(timestamp)), 3) AS finished_at,
    attributes_string['loom.repo']                               AS repo,
    attributes_string['loom.sweep_id']                           AS sweep_id,
    ifNull(coalesce(
        if(mapContains(attributes_number, 'loom.issue'),
           toUInt32(attributes_number['loom.issue']), NULL),
        toUInt32OrNull(attributes_string['loom.issue'])), 0)     AS issue,
    resources_string['host.id']                                  AS host_id,
    attributes_string['loom.repo.visibility']                    AS repo_visibility,
    attributes_string['loom.result']                             AS result,
    coalesce(
        if(mapContains(attributes_number, 'loom.story_points'),
           toUInt8(attributes_number['loom.story_points']), NULL),
        toUInt8OrNull(attributes_string['loom.story_points']))   AS story_points,
    toUInt8(mapContains(attributes_number, 'loom.story_points')
            OR mapContains(attributes_string, 'loom.story_points'))
                                                                  AS points_present
FROM signoz_logs.distributed_logs_v2
WHERE body = 'sweep.outcome'
  AND mapContains(attributes_string, 'loom.sweep_id');
