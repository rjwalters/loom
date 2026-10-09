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
--   t       DateTime64(3)  the replay instant (queries 6-7: the newest sample)
--   window  UInt32         lookback for the base anchor and its chain, seconds
--                          (3900 = 65 min: one anchor interval plus one pass)
--   repo    'owner/name' to scope to one repository, '' for all
--   span    UInt32         queries 1-3 and 6-7: seconds before t to sample back
--                          to. The prefix rebuilds state at every instant
--                          t, t - step, ... down to t - span. Queries 1-3 read
--                          the state at t only; bind 0 for them.
--   step    UInt32         seconds between sample instants (any value >= 1
--                          when span = 0; 300 = one fleet.state pass)
--   threshold UInt32       queries 6-7: seconds a covered host may disagree
--                          with the forge before the check fails (600)

-- 0. Verify the knowable-at column on the live store. Second column must be 0.
SELECT count() AS rows,
       countIf(created_at < fromUnixTimestamp64Nano(toInt64(observed_timestamp))) AS earlier_than_observed
FROM signoz_logs.distributed_logs_v2
WHERE timestamp > toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY);

-- Shared reconstruction prefix for queries 1-3 and 6-7. Query 1 below already
-- starts with it; prepend it unchanged to queries 2, 3, 6 and 7. It rebuilds
-- each emitting host's state at every sample instant `t` (`samples`; with
-- span = 0 that is the one instant t) and decides, per host and instant,
-- whether that state is reconstructable at all (`status.state`). Every CTE
-- after `samples` carries the instant `t` it describes; an instant only ever
-- reads records knowable before itself.
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
WITH samples AS (               -- the replay instants: t, t - step, ... down to t - span
    SELECT arrayJoin(arrayMap(i -> {t:DateTime64(3)} - toIntervalSecond(i * {step:UInt32}),
               range(toUInt64(intDiv({span:UInt32}, greatest({step:UInt32}, 1)) + 1)))) AS t
),
fleet_logs AS (                 -- fleet.state log records (chunks) any instant may read
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
      AND created_at >= {t:DateTime64(3)} - toIntervalSecond({span:UInt32} + {window:UInt32})
),
recs AS (                       -- per instant: delivered records knowable before it, deduped
    SELECT s.t AS t, l.emitter AS emitter, l.record_id AS record_id, l.is_anchor AS is_anchor,
           l.as_of AS as_of, l.anchor_as_of AS anchor_as_of, l.prev_as_of AS prev_as_of,
           l.chunk_index AS chunk_index, l.chunk_count AS chunk_count, l.repos AS repos,
           l.created_at AS created_at
    FROM fleet_logs l CROSS JOIN samples s
    WHERE l.created_at < s.t                              -- knowable-at, not event time
      AND l.created_at >= s.t - toIntervalSecond({window:UInt32})
    ORDER BY created_at
    LIMIT 1 BY t, record_id
),
records AS (                    -- one row per logical record, chunks reassembled
    SELECT t, emitter, as_of,
           any(is_anchor)                                AS is_anchor,
           any(anchor_as_of)                             AS anchor_as_of,
           any(prev_as_of)                               AS prev_as_of,
           uniqExact(chunk_index) = max(chunk_count)     AS chunks_complete
    FROM recs
    GROUP BY t, emitter, as_of
),
heads AS (                      -- the anchor each host's newest chain names
    SELECT t, emitter, max(anchor_as_of) AS base FROM records GROUP BY t, emitter
),
links AS (                      -- the base chain, each record with its predecessor
    SELECT r.t AS t, r.emitter AS emitter, r.as_of AS as_of, r.is_anchor AS is_anchor,
           r.prev_as_of AS prev_as_of, r.chunks_complete AS chunks_complete,
           h.base AS chain_base,
           lagInFrame(r.as_of) OVER (PARTITION BY r.t, r.emitter ORDER BY r.as_of
               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS before
    FROM records r
    JOIN heads h ON r.t = h.t AND r.emitter = h.emitter AND r.anchor_as_of = h.base
),
status AS (                     -- per host and instant: is its state reconstructable?
    SELECT t, emitter, any(chain_base) AS base, max(as_of) AS last_as_of,
           multiIf(countIf(is_anchor AND as_of = chain_base) = 0,       'missing_anchor',
                   countIf(is_anchor AND NOT chunks_complete) > 0, 'incomplete_anchor',
                   countIf(NOT is_anchor AND NOT chunks_complete) > 0, 'incomplete_delta',
                   countIf(NOT is_anchor
                           AND (prev_as_of IS NULL OR prev_as_of != before)) > 0, 'broken_chain',
                   'complete') AS state
    FROM links
    GROUP BY t, emitter
),
repo_entries AS (               -- repo parts of the records on complete chains only
    SELECT t, emitter, as_of,
           JSONExtractString(e, 'repo')               AS repo,
           JSONExtractArrayRaw(e, 'rows')             AS rows,
           JSONExtract(e, 'removed', 'Array(UInt64)') AS removed
    FROM recs ARRAY JOIN repos AS e
    WHERE (t, emitter, anchor_as_of) IN (SELECT t, emitter, base FROM status WHERE state = 'complete')
),
ops AS (                        -- every upsert and removal, one stream
    SELECT t, emitter, as_of, repo,
           JSONExtractUInt(row, 'issue')                              AS issue,
           'upsert'                                                   AS op,
           JSONExtractString(row, 'stage')                            AS stage,
           JSONExtractString(row, 'host')                             AS host,
           JSONExtractUInt(row, 'pr')                                 AS pr,
           parseDateTime64BestEffort(JSONExtractString(row, 'entered_at'), 9) AS entered_at
    FROM repo_entries ARRAY JOIN rows AS row
    UNION ALL
    SELECT t, emitter, as_of, repo, issue, 'remove', '', '', toUInt64(0), toDateTime64(0, 9)
    FROM repo_entries ARRAY JOIN removed AS issue
),
latest_per_host AS (            -- the LAST operation on an issue along one chain
    SELECT t, emitter, repo, issue,
           argMax(op, as_of) AS last_op,
           argMax(stage, as_of) AS stage, argMax(host, as_of) AS host,
           argMax(pr, as_of) AS pr, argMax(entered_at, as_of) AS entered_at,
           max(as_of) AS last_as_of
    FROM ops GROUP BY t, emitter, repo, issue
),
live AS (                       -- a removal after the last upsert deletes the issue
    SELECT t, emitter, repo, issue, stage, host, pr, entered_at, last_as_of,
           (host != '', last_as_of) AS preference      -- a row with a host wins
    FROM latest_per_host
    WHERE last_op = 'upsert'
)
-- <<< replay-prefix

-- 1. Fleet state at t: per complete host, its base anchor plus every delta of
--    the chain, the last operation per issue winning, merged per (repo, issue)
--    and preferring the row that carries a `host`. A host that is not
--    `complete` is absent (its state is UNKNOWN, not empty); see query 3.
--    Only the instant t is read (bind span = 0).
SELECT repo, issue,
       argMax(stage, preference)                     AS stage,
       argMax(host, preference)                      AS host,
       argMax(pr, preference)                        AS pr,
       argMax(entered_at, preference)                AS entered_at,
       uniqExact(emitter)                            AS reporting_hosts
FROM live
WHERE t = {t:DateTime64(3)} AND ({repo:String} = '' OR repo = {repo:String})
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
WHERE t = {t:DateTime64(3)}
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
LEFT JOIN (SELECT * FROM status WHERE t = {t:DateTime64(3)}) s ON k.emitter = s.emitter
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

-- Agreement with the forge (#10196 R7, #11128): queries 6 and 7.
--
-- The comparator is the webhook-derived label state: the forge is the system
-- of record. The loom-ui webhook Worker files one `label.transition` record per
-- `loom:*` label change (and per opened / closed / reopened of an item carrying
-- one) and exports it to SigNoz as a log row with resource
-- `service.name = 'loom-ui-d1-export'` and the D1 record as a flat JSON body.
-- The body keys read here are the whole dependency on that contract (owned by
-- loom-ui, not Loom): `kind`, `repo`, `target` (`issue` / `pr`), `number`,
-- `action` (`labeled`, `unlabeled`, `opened`, `reopened`, `closed`), `label`
-- and `at` (the Worker's receipt time, RFC 3339). The forge state of an item
-- at an instant is built from the rows whose `at` is before it: a label is on
-- when its last `labeled`/`unlabeled` was `labeled`; an item whose last
-- lifecycle row is `closed` is `closed`. An item with no webhook row before
-- the instant has no forge record (`no_forge_record`), never "no labels".
--
-- Stage <-> label (the one written-down mapping; telemetry-replay.md repeats it):
--
--   fleet.state stage            forge stage   labels on the item
--   ready_wait                   ready_wait    issue: loom:issue (not loom:building)
--   sweep.curator, sweep.builder building      issue: loom:building
--   review_wait                  review_wait   PR: loom:review-requested, the only review label
--   doctor                       doctor        PR: loom:changes-requested, the only review label;
--                                              or no review label and loom:treating
--   merge_wait                   merge_wait    PR: loom:pr, the only review label, no hold label
--   merge_hold                   merge_hold    PR: loom:pr, the only review label, and a hold
--                                              label (the registry's merge_hold set:
--                                              loom:operator, loom:operator-decision,
--                                              loom:operator-only)
--
-- A PR stage is compared on the row's `pr`; a row without one is `no_pr`. A
-- stage outside this table is `unmapped`. Neither is a disagreement.
--
-- Each covered host's own rows are compared (nothing is merged or elected).
-- Only a host whose chain is `complete` at an instant has rows there, so an
-- uncovered host never disagrees: query 7 reports it `unknown`. A
-- disagreement's duration is the run of consecutive sample instants at which
-- the same (host, item, host stage, forge stage) disagreed, times `step`; an
-- instant where the host is not covered ends the run (time while a host is not
-- reporting never counts). The check fails on a run longer than `threshold`.
-- The comparison runs one way: it checks every row a covered host reports. A
-- forge item a host does not report is not flagged, because a host sees only
-- its own repos and its own sweeps, and a PR that links no issue is census-only.
--
-- Prepend the replay prefix; query 6 already continues with the agreement
-- block below, and query 7 needs it prepended after the replay prefix.
-- >>> agreement-prefix
, forge_log AS (                -- webhook label.transition rows, at their receipt time
    SELECT lower(JSONExtractString(body, 'repo'))                           AS repo,
           JSONExtractString(body, 'target')                                AS target,
           JSONExtractUInt(body, 'number')                                  AS number,
           JSONExtractString(body, 'action')                                AS action,
           JSONExtractString(body, 'label')                                 AS label,
           parseDateTime64BestEffortOrNull(JSONExtractString(body, 'at'), 3) AS received_at
    FROM signoz_logs.distributed_logs_v2
    WHERE resources_string['service.name'] = 'loom-ui-d1-export'
      AND JSONExtractString(body, 'kind') = 'label.transition'
      AND received_at IS NOT NULL
      AND received_at < {t:DateTime64(3)}
),
forge_labels AS (               -- per instant: each label's last labeled/unlabeled before it
    SELECT s.t AS t, f.repo AS repo, f.target AS target, f.number AS number, f.label AS label,
           argMax(f.action, f.received_at) = 'labeled' AS label_on,
           max(f.received_at)                           AS changed_at
    FROM forge_log f CROSS JOIN samples s
    WHERE f.received_at < s.t AND f.action IN ('labeled', 'unlabeled') AND f.label != ''
    GROUP BY t, repo, target, number, label
),
forge_lifecycle AS (            -- per instant: whether the item's last lifecycle row closed it
    SELECT s.t AS t, f.repo AS repo, f.target AS target, f.number AS number,
           argMax(f.action, f.received_at) = 'closed' AS closed,
           max(f.received_at)                          AS changed_at
    FROM forge_log f CROSS JOIN samples s
    WHERE f.received_at < s.t AND f.action IN ('opened', 'reopened', 'closed')
    GROUP BY t, repo, target, number
),
forge_items AS (                -- per instant and item: the labels on it, and since when
    SELECT t, repo, target, number,
           groupArrayIf(label, label_on) AS labels,
           maxIf(changed_at, label IN ('loom:issue', 'loom:building', 'loom:review-requested',
                                       'loom:changes-requested', 'loom:pr', 'loom:treating',
                                       'loom:operator', 'loom:operator-decision',
                                       'loom:operator-only')) AS labels_changed_at
    FROM forge_labels
    GROUP BY t, repo, target, number
),
forge AS (                      -- per instant and item: the stage the forge names
    SELECT i.t AS t, i.repo AS repo, i.target AS target, i.number AS number,
           has(i.labels, 'loom:review-requested') + has(i.labels, 'loom:changes-requested')
               + has(i.labels, 'loom:pr')                                   AS review_labels,
           multiIf(c.closed, 'closed',
                   i.target = 'issue' AND has(i.labels, 'loom:building'), 'building',
                   i.target = 'issue' AND has(i.labels, 'loom:issue'), 'ready_wait',
                   i.target = 'pr' AND review_labels = 1 AND has(i.labels, 'loom:pr')
                       AND hasAny(i.labels, ['loom:operator', 'loom:operator-decision',
                                             'loom:operator-only']), 'merge_hold',
                   i.target = 'pr' AND review_labels = 1
                       AND has(i.labels, 'loom:review-requested'), 'review_wait',
                   i.target = 'pr' AND review_labels = 1
                       AND has(i.labels, 'loom:changes-requested'), 'doctor',
                   i.target = 'pr' AND review_labels = 1, 'merge_wait',
                   i.target = 'pr' AND review_labels = 0
                       AND has(i.labels, 'loom:treating'), 'doctor',
                   'none')                                                  AS stage,
           greatest(i.labels_changed_at, c.changed_at)                      AS since,
           1                                                                AS found
    FROM forge_items i
    LEFT JOIN forge_lifecycle c
        ON i.t = c.t AND i.repo = c.repo AND i.target = c.target AND i.number = c.number
),
expected AS (                   -- each covered host's own rows, with the forge item they name
    SELECT t, emitter, repo, lower(repo) AS repo_key, issue, pr, stage AS host_stage,
           entered_at,
           multiIf(stage IN ('ready_wait', 'sweep.curator', 'sweep.builder'), 'issue',
                   stage IN ('review_wait', 'doctor', 'merge_wait', 'merge_hold'), 'pr',
                   '')                                                      AS target,
           if(target = 'pr', pr, issue)                                     AS number,
           if(stage IN ('sweep.curator', 'sweep.builder'), 'building', stage) AS want
    FROM live
    WHERE {repo:String} = '' OR repo = {repo:String}
),
compared AS (                   -- per instant, host and row: agree, disagree, or why not compared
    SELECT e.t AS t, e.emitter AS emitter, e.repo AS repo, e.issue AS issue, e.pr AS pr,
           e.host_stage AS host_stage, e.entered_at AS entered_at,
           if(f.found = 1, f.stage, '')                                     AS forge_stage,
           f.since                                                          AS forge_since,
           multiIf(e.target = '', 'unmapped',
                   e.number = 0, 'no_pr',
                   f.found = 0, 'no_forge_record',
                   f.stage = e.want, 'agree',
                   'disagree')                                              AS verdict
    FROM expected e
    LEFT JOIN forge f
        ON e.t = f.t AND e.repo_key = f.repo AND e.target = f.target AND e.number = f.number
),
runs AS (                       -- each disagreement: a run of consecutive covered instants
    SELECT emitter, repo, issue, pr, host_stage, forge_stage,
           count() * {step:UInt32}                AS disagree_sec,
           min(t)                                 AS first_at,
           max(t)                                 AS last_at,
           min(entered_at)                        AS host_entered_at,
           max(forge_since)                       AS forge_since,
           disagree_sec > {threshold:UInt32}      AS over_threshold
    FROM (
        SELECT *,
               intDiv(dateDiff('second', t, {t:DateTime64(3)}), greatest({step:UInt32}, 1))
                 + row_number() OVER (PARTITION BY emitter, repo, issue, pr, host_stage, forge_stage
                                      ORDER BY t) AS island
        FROM compared
        WHERE verdict = 'disagree'
    )
    GROUP BY emitter, repo, issue, pr, host_stage, forge_stage, island
),
hosts AS (                      -- every host seen in the last day before the first instant
    SELECT DISTINCT resources_string['host.name'] AS emitter
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] IN ('fleet.state', 'host.health')
      AND created_at < {t:DateTime64(3)}
      AND created_at >= {t:DateTime64(3)} - toIntervalSecond({span:UInt32}) - INTERVAL 1 DAY
),
host_samples AS (               -- every host at every instant: its chain state, or no_anchor
    SELECT h.emitter AS emitter, s.t AS t,
           if(st.emitter = '', 'no_anchor', st.state) AS state
    FROM hosts h CROSS JOIN samples s
    LEFT JOIN status st ON st.emitter = h.emitter AND st.t = s.t
),
incomplete AS (                 -- records whose received chunks != chunk_count, never used
    SELECT emitter,
           countIf(is_anchor)     AS incomplete_anchors,
           countIf(NOT is_anchor) AS incomplete_deltas
    FROM (
        SELECT emitter, as_of, any(is_anchor) AS is_anchor
        FROM fleet_logs
        GROUP BY emitter, as_of
        HAVING uniqExact(chunk_index) != max(chunk_count)
    )
    GROUP BY emitter
)
-- <<< agreement-prefix

-- 6. Disagreements (`telemetry-replay --check`): one row per run, longest
--    first. A row with `over_threshold` fails the check. Prepend the replay
--    prefix.
SELECT emitter, repo, issue, pr, host_stage, forge_stage, disagree_sec,
       first_at, last_at, host_entered_at, forge_since, over_threshold
FROM runs
ORDER BY over_threshold DESC, disagree_sec DESC, emitter, repo, issue, first_at;

-- 7. Agreement report per host (the daily report: t = now() - 10 min,
--    span = 86400, step = 300, threshold = 600). Every host seen in the day is
--    a row: `coverage` is `covered` (complete at every instant), `partial` or
--    `unknown` (complete at none), with the chain states it was not covered
--    in. `compared` counts host rows x instants compared with the forge;
--    `not_comparable` the `no_pr`, `no_forge_record` and `unmapped` ones.
--    `longest_disagreement_sec` is the host's view lag: its longest run of
--    disagreeing with the forge. `incomplete_anchors` / `incomplete_deltas`
--    count records whose received chunks != chunk_count (not used). Prepend
--    the replay prefix, then the agreement block.
SELECT hs.emitter                                                  AS emitter,
       hs.samples                                                  AS samples,
       hs.covered_samples                                          AS covered_samples,
       multiIf(hs.covered_samples = 0, 'unknown',
               hs.covered_samples < hs.samples, 'partial',
               'covered')                                          AS coverage,
       hs.uncovered_states                                         AS uncovered_states,
       c.agreeing + c.disagreeing                                  AS compared,
       c.agreeing                                                  AS agreeing,
       c.disagreeing                                               AS disagreeing,
       c.not_comparable                                            AS not_comparable,
       r.longest_disagreement_sec                                  AS longest_disagreement_sec,
       r.over_threshold                                            AS disagreements_over_threshold,
       ic.incomplete_anchors                                       AS incomplete_anchors,
       ic.incomplete_deltas                                        AS incomplete_deltas
FROM (
    SELECT emitter, count() AS samples, countIf(state = 'complete') AS covered_samples,
           arraySort(groupUniqArrayIf(state, state != 'complete')) AS uncovered_states
    FROM host_samples GROUP BY emitter
) hs
LEFT JOIN (
    SELECT emitter, countIf(verdict = 'agree') AS agreeing,
           countIf(verdict = 'disagree') AS disagreeing,
           countIf(verdict NOT IN ('agree', 'disagree')) AS not_comparable
    FROM compared GROUP BY emitter
) c ON hs.emitter = c.emitter
LEFT JOIN (
    SELECT emitter, max(disagree_sec) AS longest_disagreement_sec,
           countIf(over_threshold) AS over_threshold
    FROM runs GROUP BY emitter
) r ON hs.emitter = r.emitter
LEFT JOIN incomplete ic ON hs.emitter = ic.emitter
ORDER BY emitter;
