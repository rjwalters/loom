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
-- for transport. A record whose chunks did not all arrive, or a chain with a
-- lost delta, makes that host's state unknown at t (the replay prefix gates
-- queries 1-3 on it); query 4 (b) lists such records over the last 7 days.
--
-- Written against the documented `fleet-state/v1` schema. The `fleet.state`
-- kind arrives with #10283; adjust the attribute and JSON paths here if its
-- final shape differs. The emitting host is `resources_string['host.name']`
-- (the resource that exported the record); a row's own `host` is the host whose
-- sweep holds the item and is a different fact.
--
-- Parameters (ClickHouse query parameters):
--   t       DateTime64(3)  the replay instant
--   window  UInt32         lookback for the base anchor and its chain, seconds
--                          (3900 = 65 min: one anchor interval plus one pass)
--   repo    'owner/name' to scope to one repository, '' for all

-- 0. Verify the knowable-at column on the live store. Second column must be 0.
SELECT count() AS rows,
       countIf(created_at < fromUnixTimestamp64Nano(toInt64(observed_timestamp))) AS earlier_than_observed
FROM signoz_logs.distributed_logs_v2
WHERE timestamp > toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY);

-- Shared reconstruction prefix for queries 1-3. Query 1 below already starts
-- with it; prepend it unchanged to queries 2 and 3. It rebuilds each emitting
-- host's state at t and decides, per host, whether that state is
-- reconstructable at all (`status.state`):
--
--   complete           the base anchor and every delta on its chain arrived
--   missing_anchor     a delta names an anchor that is not knowable before t
--   incomplete_anchor  the base anchor is missing a byte chunk
--   incomplete_delta   a delta on the chain is missing a byte chunk
--   broken_chain       a delta's `prev_as_of` is not the record before it: a
--                      delta between them was lost
--
-- Only `complete` hosts contribute rows. Any other state is UNKNOWN, never
-- "empty" and never "the last state we could build": a lost delta may have
-- removed an issue, and a partial anchor is a partial base. The base is the
-- anchor the host's newest knowable chain names (max `anchor_as_of`), so an
-- older complete anchor is never used past a newer one. A lost delta at the
-- very end of a chain (nothing after it yet) cannot be detected from the chain;
-- query 3's `last_as_of` shows how fresh each host's chain is.
--
-- Chunk fields: a record split by bytes for transport is several log records
-- sharing `(emitter, as_of)`, each with the header fields in its body and
-- `loom.fleet.chunk_index` / `loom.fleet.chunk_count` attributes. A record
-- without them is one chunk. The attribute names are provisional until #10283
-- settles the chunking transport.
-- >>> replay-prefix
WITH recs AS (                  -- delivered log records (chunks), deduped
    SELECT resources_string['host.name']                                  AS emitter,
           attributes_string['loom.record_id']                            AS record_id,
           JSONExtractBool(body, 'anchor')                                AS is_anchor,
           parseDateTime64BestEffort(JSONExtractString(body, 'as_of'), 9) AS as_of,
           parseDateTime64BestEffort(JSONExtractString(body, 'anchor_as_of'), 9) AS anchor_as_of,
           parseDateTime64BestEffortOrNull(JSONExtractString(body, 'prev_as_of'), 9) AS prev_as_of,
           toUInt32(attributes_number['loom.fleet.chunk_index'])          AS chunk_index,
           greatest(toUInt32(attributes_number['loom.fleet.chunk_count']), 1) AS chunk_count,
           JSONExtractArrayRaw(body, 'repos')                             AS repos,
           created_at
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'fleet.state'
      AND created_at < {t:DateTime64(3)}                  -- knowable-at, not event time
      AND created_at >= {t:DateTime64(3)} - INTERVAL {window:UInt32} SECOND
    ORDER BY created_at
    LIMIT 1 BY record_id
),
records AS (                    -- one row per logical record, chunks reassembled
    SELECT emitter, as_of,
           any(is_anchor)                                AS is_anchor,
           any(anchor_as_of)                             AS anchor_as_of,
           any(prev_as_of)                               AS prev_as_of,
           uniqExact(chunk_index) = max(chunk_count)     AS chunks_complete
    FROM recs
    GROUP BY emitter, as_of
),
heads AS (                      -- the anchor each host's newest chain names
    SELECT emitter, max(anchor_as_of) AS base FROM records GROUP BY emitter
),
links AS (                      -- the base chain, each record with its predecessor
    SELECT r.emitter AS emitter, r.as_of AS as_of, r.is_anchor AS is_anchor,
           r.prev_as_of AS prev_as_of, r.chunks_complete AS chunks_complete,
           h.base AS chain_base,
           lagInFrame(r.as_of) OVER (PARTITION BY r.emitter ORDER BY r.as_of
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS before
    FROM records r
    JOIN heads h ON r.emitter = h.emitter AND r.anchor_as_of = h.base
),
status AS (                     -- per host: is its state at t reconstructable?
    SELECT emitter, any(chain_base) AS base, max(as_of) AS last_as_of,
           multiIf(countIf(is_anchor AND as_of = chain_base) = 0,       'missing_anchor',
                   countIf(is_anchor AND NOT chunks_complete) > 0, 'incomplete_anchor',
                   countIf(NOT is_anchor AND NOT chunks_complete) > 0, 'incomplete_delta',
                   countIf(NOT is_anchor
                           AND (prev_as_of IS NULL OR prev_as_of != before)) > 0, 'broken_chain',
                   'complete') AS state
    FROM links
    GROUP BY emitter
),
repo_entries AS (               -- repo parts of the records on complete chains only
    SELECT emitter, as_of,
           JSONExtractString(e, 'repo')               AS repo,
           JSONExtractArrayRaw(e, 'rows')             AS rows,
           JSONExtract(e, 'removed', 'Array(UInt64)') AS removed
    FROM recs ARRAY JOIN repos AS e
    WHERE (emitter, anchor_as_of) IN (SELECT emitter, base FROM status WHERE state = 'complete')
),
ops AS (                        -- every upsert and removal, one stream
    SELECT emitter, as_of, repo,
           JSONExtractUInt(row, 'issue')                              AS issue,
           'upsert'                                                   AS op,
           JSONExtractString(row, 'stage')                            AS stage,
           JSONExtractString(row, 'host')                             AS host,
           JSONExtractUInt(row, 'pr')                                 AS pr,
           parseDateTime64BestEffort(JSONExtractString(row, 'entered_at'), 9) AS entered_at
    FROM repo_entries ARRAY JOIN rows AS row
    UNION ALL
    SELECT emitter, as_of, repo, issue, 'remove', '', '', toUInt64(0), toDateTime64(0, 9)
    FROM repo_entries ARRAY JOIN removed AS issue
),
latest_per_host AS (            -- the LAST operation on an issue along one chain
    SELECT emitter, repo, issue,
           argMax(op, as_of) AS last_op,
           argMax(stage, as_of) AS stage, argMax(host, as_of) AS host,
           argMax(pr, as_of) AS pr, argMax(entered_at, as_of) AS entered_at,
           max(as_of) AS last_as_of
    FROM ops GROUP BY emitter, repo, issue
),
live AS (                       -- a removal after the last upsert deletes the issue
    SELECT emitter, repo, issue, stage, host, pr, entered_at, last_as_of,
           (host != '', last_as_of) AS preference      -- a row with a host wins
    FROM latest_per_host
    WHERE last_op = 'upsert'
)
-- <<< replay-prefix

-- 1. Fleet state at t: per complete host, its base anchor plus every delta of
--    the chain, the last operation per issue winning, merged per (repo, issue)
--    and preferring the row that carries a `host`. A host that is not
--    `complete` is absent (its state is UNKNOWN, not empty); see query 3.
SELECT repo, issue,
       argMax(stage, preference)                     AS stage,
       argMax(host, preference)                      AS host,
       argMax(pr, preference)                        AS pr,
       argMax(entered_at, preference)                AS entered_at,
       uniqExact(emitter)                            AS reporting_hosts
FROM live
WHERE {repo:String} = '' OR repo = {repo:String}
GROUP BY repo, issue
ORDER BY repo, issue;

-- 2. How far the hosts' views disagree at t: per (repo, issue) seen by more
--    than one complete host, whether they name different stages, different
--    holders, or different entered-at instants, and the entered-at spread.
--    Prepend the replay prefix.
SELECT repo, issue,
       uniqExact(emitter)                                           AS reporting_hosts,
       uniqExact(stage)                                             AS distinct_stages,
       uniqExactIf(host, host != '')                                AS distinct_holders,
       dateDiff('second', min(entered_at), max(entered_at))         AS entered_at_spread_sec
FROM live
GROUP BY repo, issue
HAVING reporting_hosts > 1
ORDER BY distinct_stages DESC, entered_at_spread_sec DESC;

-- 3. Coverage at t: which hosts a reader may speak for. Every host seen in the
--    last day (`fleet.state` or `host.health`) gets a row; it is `covered`
--    only when its state is `complete`. Silence from an uncovered host is
--    "unknown", not "nothing happened". `no_anchor` means no `fleet.state`
--    record inside `window`. `anchor_age_sec` and `last_as_of` show how stale
--    the base anchor and the chain's newest record are. Prepend the replay
--    prefix.
SELECT k.emitter                                             AS emitter,
       if(s.emitter = '', 'no_anchor', s.state)              AS state,
       s.emitter != '' AND s.state = 'complete'              AS covered,
       if(s.emitter = '', NULL, s.base)                      AS anchor_as_of,
       if(s.emitter = '', NULL, s.last_as_of)                AS last_as_of,
       if(s.emitter = '', NULL, dateDiff('second', s.base, {t:DateTime64(3)})) AS anchor_age_sec
FROM (
    SELECT DISTINCT resources_string['host.name'] AS emitter
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] IN ('fleet.state', 'host.health')
      AND created_at < {t:DateTime64(3)}
      AND created_at >= {t:DateTime64(3)} - INTERVAL 1 DAY
) k
LEFT JOIN status s ON k.emitter = s.emitter
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

--    (b) records (anchors and deltas) whose received chunks differ from
--    `chunk_count`. Such a record is never used as a base or applied as a
--    delta: the replay prefix marks its host `incomplete_anchor` or
--    `incomplete_delta`. Chunk attributes are provisional until #10283 settles
--    the names.
SELECT resources_string['host.name']                       AS emitter,
       JSONExtractString(body, 'as_of')                    AS as_of,
       any(JSONExtractBool(body, 'anchor'))                AS anchor,
       max(attributes_number['loom.fleet.chunk_count'])    AS chunk_count,
       uniqExact(attributes_number['loom.fleet.chunk_index']) AS chunks_received
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] = 'fleet.state'
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
--    A record with no forge instant carries no fact id and is not returned
--    here; see telemetry-replay.md "No forge instant, no fact id" (#11126).
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
