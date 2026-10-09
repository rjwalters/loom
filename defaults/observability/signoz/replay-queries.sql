-- Replay queries (#10196 R3, slice #11125). Contract: ../../docs/telemetry-replay.md.
-- Record schema: ../../docs/telemetry-schema.md (`fleet.state`, `pr.resolved`,
-- `eta.stage_outcome`).
--
-- Question answered: what did the fleet look like at instant `t`, as a daemon
-- running at `t` could have known it?
--
-- Knowable-at: every membership filter below is on `created_at`, SigNoz's own
-- insert clock on signoz_logs.distributed_logs_v2 (see session-output.md,
-- "Querying end-to-end latency"). It is NEVER `timestamp` (event time) and
-- never the producer's `observed_timestamp`: a record delayed in the export
-- queue claims an early producer value. Event time orders and groups rows
-- inside a reconstruction; it does not decide membership. Whether `created_at`
-- is never earlier than `observed_timestamp` is checked by query 0 below.
--
-- Duplicates: delivery is at least once. Deliveries dedupe on
-- `loom.record_id` (`LIMIT 1 BY`, earliest knowable-at kept). Outcome facts
-- (`pr.resolved`, `eta.stage_outcome`) dedupe on the cross-host `loom.fact_id`.
--
-- No row caps: nothing here truncates a result. The only chunking is by bytes
-- for transport, and query 4 detects an anchor whose chunks did not all
-- arrive.
--
-- Written against the documented `fleet-state/v1` schema. The `fleet.state`
-- kind arrives with #10283; adjust the attribute and JSON paths here if its
-- final shape differs. The emitting host is `resources_string['host.name']`
-- (the resource that exported the record); a row's own `host` is the host whose
-- sweep holds the item and is a different fact.
--
-- Parameters (ClickHouse query parameters):
--   t       DateTime64(3)  the replay instant
--   window  UInt32         lookback for the newest anchor, seconds (3900 = 65 min)
--   repo    'owner/name' to scope to one repository, '' for all

-- 0. Verify the knowable-at column on the live store. Second column must be 0.
SELECT count() AS rows,
       countIf(created_at < fromUnixTimestamp64Nano(toInt64(observed_timestamp))) AS earlier_than_observed
FROM signoz_logs.distributed_logs_v2
WHERE timestamp > toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY);

-- 1. Fleet state at t: per emitting host, the newest anchor knowable before t
--    plus every later delta of its chain, merged per (repo, issue), preferring
--    the row that carries a `host`. A host with no anchor inside `window` is
--    absent from the result (its state is UNKNOWN, not empty); see query 3.
WITH recs AS (
    SELECT resources_string['host.name']                                  AS emitter,
           JSONExtractBool(body, 'anchor')                                AS is_anchor,
           parseDateTime64BestEffort(JSONExtractString(body, 'as_of'), 9) AS as_of,
           parseDateTime64BestEffort(JSONExtractString(body, 'anchor_as_of'), 9) AS anchor_as_of,
           JSONExtractArrayRaw(body, 'repos')                             AS repos,
           created_at
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'fleet.state'
      AND created_at < {t:DateTime64(3)}                  -- knowable-at, not event time
      AND created_at >= {t:DateTime64(3)} - INTERVAL {window:UInt32} SECOND
    ORDER BY created_at
    LIMIT 1 BY attributes_string['loom.record_id']
),
newest_anchor AS (
    SELECT emitter, max(as_of) AS anchor_at FROM recs WHERE is_anchor GROUP BY emitter
),
chain AS (
    SELECT r.* FROM recs r
    JOIN newest_anchor a ON r.emitter = a.emitter AND r.anchor_as_of = a.anchor_at
),
repo_entries AS (
    SELECT emitter, as_of,
           JSONExtractString(e, 'repo')            AS repo,
           JSONExtractArrayRaw(e, 'rows')          AS rows,
           JSONExtract(e, 'removed', 'Array(UInt32)') AS removed
    FROM chain ARRAY JOIN repos AS e
),
rows_flat AS (
    SELECT emitter, as_of, repo,
           JSONExtractUInt(row, 'issue')           AS issue,
           JSONExtractString(row, 'stage')         AS stage,
           JSONExtractString(row, 'host')          AS host,
           JSONExtractUInt(row, 'pr')              AS pr,
           parseDateTime64BestEffort(JSONExtractString(row, 'entered_at'), 9) AS entered_at
    FROM repo_entries ARRAY JOIN rows AS row
),
latest_per_host AS (            -- the last upsert of an issue on one emitter's chain
    SELECT emitter, repo, issue,
           argMax(stage, as_of) AS stage, argMax(host, as_of) AS host,
           argMax(pr, as_of) AS pr, argMax(entered_at, as_of) AS entered_at,
           max(as_of) AS last_as_of
    FROM rows_flat GROUP BY emitter, repo, issue
),
live AS (                       -- drop issues a later delta removed
    SELECT l.* FROM latest_per_host l
    LEFT JOIN (SELECT emitter, repo, arrayJoin(removed) AS issue, as_of AS removed_at
               FROM repo_entries) d
      ON l.emitter = d.emitter AND l.repo = d.repo AND l.issue = d.issue
    WHERE d.removed_at IS NULL OR d.removed_at < l.last_as_of
)
SELECT repo, issue,
       argMax(stage, (host != '', last_as_of))       AS stage,    -- a row with a host wins
       argMax(host,  (host != '', last_as_of))       AS host,
       argMax(pr,    (host != '', last_as_of))       AS pr,
       argMax(entered_at, (host != '', last_as_of))  AS entered_at,
       uniqExact(emitter)                            AS reporting_hosts
FROM live
WHERE {repo:String} = '' OR repo = {repo:String}
GROUP BY repo, issue
ORDER BY repo, issue;

-- 2. How far the hosts' views disagree at t: per (repo, issue) seen by more
--    than one emitting host, whether they name different stages, different
--    holders, or different entered-at instants, and the entered-at spread.
--    Run it over query 1's `live` CTE (same WITH prefix).
SELECT repo, issue,
       uniqExact(emitter)                                           AS reporting_hosts,
       uniqExact(stage)                                             AS distinct_stages,
       uniqExactIf(host, host != '')                                AS distinct_holders,
       dateDiff('second', min(entered_at), max(entered_at))         AS entered_at_spread_sec
FROM live
GROUP BY repo, issue
HAVING reporting_hosts > 1
ORDER BY distinct_stages DESC, entered_at_spread_sec DESC;

-- 3. Coverage at t: which hosts a reader may speak for. A host is covered when
--    it has an anchor knowable before t inside `window`; silence from an
--    uncovered host is "unknown", not "nothing happened". `last_anchor_age_sec`
--    shows how stale the newest anchor is; hosts with no anchor are the ones
--    missing from query 1.
SELECT resources_string['host.name'] AS emitter,
       max(created_at)               AS last_knowable,
       maxIf(created_at, JSONExtractBool(body, 'anchor')) AS last_anchor_knowable,
       dateDiff('second', last_anchor_knowable, {t:DateTime64(3)}) AS last_anchor_age_sec,
       last_anchor_knowable >= {t:DateTime64(3)} - INTERVAL {window:UInt32} SECOND AS covered
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] IN ('fleet.state', 'host.health')
  AND created_at < {t:DateTime64(3)}
  AND created_at >= {t:DateTime64(3)} - INTERVAL 1 DAY
GROUP BY emitter
ORDER BY emitter;

-- 4. Volume and anchor completeness (last 7 days; no row cap).
--    (a) bytes and rows per day, rows per anchor.
SELECT toDate(created_at)                              AS day,
       count()                                         AS rows,
       sum(length(body))                               AS body_bytes,
       countIf(JSONExtractBool(body, 'anchor'))        AS anchors,
       round(count() / nullIf(countIf(JSONExtractBool(body, 'anchor')), 0), 2) AS rows_per_anchor
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] = 'fleet.state'
  AND created_at > now64(3) - INTERVAL 7 DAY
GROUP BY day ORDER BY day;

--    (b) anchors whose received chunks differ from `chunk_count` (a chunked
--    anchor with a missing chunk must not be used as a base). Chunk attributes
--    follow the byte-chunking transport; provisional until #10283 settles the
--    names.
SELECT resources_string['host.name']                       AS emitter,
       JSONExtractString(body, 'as_of')                    AS as_of,
       any(attributes_number['loom.fleet.chunk_count'])    AS chunk_count,
       uniqExact(attributes_number['loom.fleet.chunk_index']) AS chunks_received
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] = 'fleet.state'
  AND JSONExtractBool(body, 'anchor')
  AND created_at > now64(3) - INTERVAL 7 DAY
GROUP BY emitter, as_of
HAVING chunk_count > 0 AND chunks_received != chunk_count
ORDER BY as_of DESC;

--    (c) hosts that sent no anchor in the last 2 hours.
SELECT emitter FROM (
    SELECT DISTINCT resources_string['host.name'] AS emitter
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] IN ('fleet.state', 'host.health')
      AND created_at > now64(3) - INTERVAL 7 DAY
) known
WHERE emitter NOT IN (
    SELECT resources_string['host.name']
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'fleet.state'
      AND JSONExtractBool(body, 'anchor')
      AND created_at > now64(3) - INTERVAL 2 HOUR
)
ORDER BY emitter;

-- 5. Outcome facts knowable before t, one row per fact. `LIMIT 1 BY
--    loom.fact_id` keeps the earliest knowable-at across hosts. An external
--    webhook outcome row is primary for the merge/close instant; `pr.resolved`
--    corroborates it, and a missing webhook row is not a missing outcome.
SELECT attributes_string['loom.kind']    AS kind,
       attributes_string['loom.fact_id'] AS fact_id,
       attributes_string['loom.repo']    AS repo,
       fromUnixTimestamp64Nano(toInt64(timestamp)) AS event_time,
       created_at                        AS knowable_at
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] IN ('pr.resolved', 'eta.stage_outcome')
  AND attributes_string['loom.fact_id'] != ''
  AND created_at < {t:DateTime64(3)}
  AND ({repo:String} = '' OR attributes_string['loom.repo'] = {repo:String})
ORDER BY created_at
LIMIT 1 BY attributes_string['loom.fact_id'];
