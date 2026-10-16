-- Hosts losing telemetry, or not reporting their export view (Issue #11124,
-- R2 of #10196), from the `host.export` log the daemon emits once per
-- `host.health` interval. Facts only: the daemon computes no rate.
--
-- `loom.host_export.dropped_total` is a durable per-exporter sum over loss
-- reasons (Issue #11527): the daemon backs it with an on-disk loss ledger
-- (observability-queue[.<name>].loss.json), so it survives daemon restarts
-- instead of resetting to 0. A decrease is therefore no longer an expected
-- counter reset — only a removed or reset ledger file produces one — but the
-- query keeps the reset-defensive read below (a decrease contributes the
-- post-reset value, never a negative delta), which stays correct for either
-- shape.
--
-- STATUS: queries 1-3 and 5 are executed verbatim against the pinned ClickHouse by
-- loom-daemon/tests/signoz_host_export_queries.rs (CI runs it with --ignored).

-- 1. Hosts with drops in the last 2 h, worst first. Each host's latest sample
--    in the 24 h before the window is kept as the baseline, so drops between
--    the window start and the first in-window sample are counted. A host with
--    no earlier sample has an unknown baseline: its first value contributes 0
--    rather than being read as a recent drop.
WITH raw AS (
    SELECT resources_string['host.id'] AS host,
           toUInt64(attributes_number['loom.host_export.dropped_total']) AS dropped,
           timestamp
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 26 HOUR)
),
samples AS (
    SELECT host, dropped, timestamp, 1 AS in_window
    FROM raw
    WHERE timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
    UNION ALL
    SELECT host, argMax(dropped, timestamp) AS last_dropped, max(timestamp) AS last_ts, 0 AS in_window
    FROM raw
    WHERE timestamp < toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
    GROUP BY host
),
steps AS (
    SELECT host,
           dropped,
           in_window,
           lagInFrame(dropped) OVER (PARTITION BY host ORDER BY timestamp
                                     ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS prev,
           row_number() OVER (PARTITION BY host ORDER BY timestamp) AS n
    FROM samples
)
SELECT host,
       sum(multiIf(in_window = 0 OR n = 1, toInt64(0), dropped >= prev, toInt64(dropped) - toInt64(prev), toInt64(dropped))) AS dropped_in_window
FROM steps
GROUP BY host
HAVING dropped_in_window > 0
ORDER BY dropped_in_window DESC;

-- 2. Hosts with no `host.export` in the last 2 h but other Loom logs in that
--    window (`host.health` itself is exported as gauges, so it is not a log
--    to anchor on): the host is alive and not reporting its export view (an older daemon, a
--    stopped OTLP exporter, or a broken pipeline).
SELECT h.host AS host, h.last_seen
FROM (
    SELECT resources_string['host.id'] AS host,
           max(timestamp) AS last_seen
    FROM signoz_logs.distributed_logs_v2
    WHERE timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
      AND attributes_string['loom.kind'] != ''
      AND attributes_string['loom.kind'] != 'host.export'
    GROUP BY host
) AS h
LEFT ANTI JOIN (
    SELECT DISTINCT resources_string['host.id'] AS host
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
) AS e ON h.host = e.host
ORDER BY h.host;

-- 3. Roster hosts that are silent by absence (Issue #11198). Queries 1 and 2
--    anchor on logs a host did send, so a host that sends nothing never
--    appears. Only a declared roster turns absence into a finding. The roster
--    is the fleet-gitops hosts.yml entries with
--    `services.loom-daemon.expected: running`, with their `telemetry:` class,
--    pasted between the markers (the committed rows are placeholders: an
--    unrendered rule fires loudly for them, by design). Each host is judged
--    only on its own `host.export`; no election.
--      always      silent after 900 s (3 missed 5-minute ticks)
--      when_awake  silent after 86400 s (a laptop may sleep)
--    A host that never exported in the 48 h lookback is silent for the whole
--    lookback. `state` splits "exporter stopped" from "host down": other Loom
--    logs inside the host's own threshold mean `exporter_silent`, none mean
--    `host_silent`. The host key is `host.name`, falling back to `host.id`.
WITH roster AS (
    SELECT host, telemetry
    FROM values('host String, telemetry String',
        /* loom:roster:begin */
        ('example-always', 'always'), ('example-when-awake', 'when_awake')
        /* loom:roster:end */
    )
),
seen AS (
    SELECT if(resources_string['host.name'] != '', resources_string['host.name'], resources_string['host.id']) AS host,
           countIf(attributes_string['loom.kind'] = 'host.export') AS exports,
           maxIf(timestamp, attributes_string['loom.kind'] = 'host.export') AS last_export_ns,
           maxIf(timestamp, attributes_string['loom.kind'] NOT IN ('', 'host.export')) AS last_other_ns
    FROM signoz_logs.distributed_logs_v2
    WHERE timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 48 HOUR)
    GROUP BY host
),
judged AS (
    SELECT r.host AS host,
           r.telemetry AS telemetry,
           if(r.telemetry = 'always', 900, 86400) AS threshold_seconds,
           ifNull(s.exports, 0) AS n_exports,
           toInt64(ifNull(s.last_export_ns, 0)) AS le_ns,
           toInt64(ifNull(s.last_other_ns, 0)) AS lo_ns,
           toInt64(toUnixTimestamp64Nano(now64(9))) AS now_ns
    FROM roster AS r
    LEFT JOIN seen AS s ON r.host = s.host
)
SELECT host,
       telemetry,
       if(n_exports = 0, NULL, fromUnixTimestamp64Nano(le_ns)) AS last_export,
       if(n_exports = 0, 172800, intDiv(now_ns - le_ns, 1000000000)) AS silent_seconds,
       if(lo_ns > 0 AND intDiv(now_ns - lo_ns, 1000000000) <= threshold_seconds,
          'exporter_silent', 'host_silent') AS state
FROM judged
WHERE silent_seconds > threshold_seconds
ORDER BY host;

-- 4. Observed per-exporter/per-reason loss lower bounds in the last 2 h.
-- A 24 h pre-window lookback supplies one baseline per host/exporter. Unknown
-- first values contribute zero, never invented recent loss. Decreases contribute
-- the observed post-reset value and flag uncertainty, never erase earlier steps.
-- Identity here is (host, exporter, timestamp, exporter JSON): duplicate delivery
-- collapses; conflicting same-timestamp JSON selects lexical max deterministically
-- and is flagged. Steps touching conflicts contribute zero: no invented loss.
-- A present reason map omits zero keys; a missing legacy map is unknown. Only
-- sample-aligned, nondecreasing reason steps are attributed. A reset uses the
-- current map; missing/inconsistent breakdowns put the step in unattributed.
-- Zero-loss rows with uncertainty use coverage_unknown (not a loss reason).
-- delta/reason_delta are observed lower bounds, not authoritative journal recovery.
-- STATUS: query 4 is replayed with a fixed clock on pinned ClickHouse in CI.
WITH raw AS (
    SELECT if(resources_string['host.name'] != '', resources_string['host.name'], resources_string['host.id']) AS host,
           resources_string['service.version'] AS version, timestamp,
           JSONExtractString(e, 'name') AS exporter, e
    FROM signoz_logs.distributed_logs_v2
    ARRAY JOIN JSONExtractArrayRaw(body, 'exporters') AS e
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 26 HOUR)
      AND timestamp <= toUnixTimestamp64Nano(now64(9))
),
dedup AS (
    SELECT host, exporter, timestamp, max(e) AS sample,
           groupUniqArray(version) AS sample_versions, uniqExact(e) > 1 AS conflicting
    FROM raw GROUP BY host, exporter, timestamp
),
parsed AS (
    SELECT *, JSONExtractUInt(sample, 'dropped_total') AS dropped,
           JSONExtractUInt(sample, 'queue_depth') AS queue_depth,
           JSONHas(sample, 'dropped_by_reason') AS map_known,
           JSONExtractKeysAndValues(sample, 'dropped_by_reason', 'UInt64') AS reasons,
           timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR) AS in_window
    FROM dedup
),
baselines AS (
    SELECT *, row_number() OVER (PARTITION BY host, exporter ORDER BY timestamp DESC) AS baseline_rank
    FROM parsed WHERE NOT in_window
),
samples AS (
    SELECT * FROM parsed WHERE in_window
    UNION ALL
    SELECT * EXCEPT baseline_rank FROM baselines WHERE baseline_rank = 1
),
ordered AS (
    SELECT *, row_number() OVER w AS n,
           lagInFrame(tuple(dropped, map_known, reasons, conflicting)) OVER w AS previous
    FROM samples
    WINDOW w AS (PARTITION BY host, exporter ORDER BY timestamp ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
),
steps AS (
    SELECT *, tupleElement(previous, 1) AS prev_dropped,
           tupleElement(previous, 2) AS prev_map_known,
           tupleElement(previous, 3) AS prev_reasons,
           tupleElement(previous, 4) AS prev_conflicting,
           in_window AND n > 1 AND dropped < prev_dropped AS reset,
           multiIf(NOT in_window OR n = 1 OR conflicting OR prev_conflicting, toUInt64(0), dropped >= prev_dropped,
                   toUInt64(dropped - prev_dropped), dropped) AS increment
    FROM ordered
),
reason_steps AS (
    SELECT *,
           arrayMap(k -> tuple(k,
               if(reset, toInt64(arraySum(arrayMap(r -> if(r.1 = k, r.2, toUInt64(0)), reasons))),
                  toInt64(arraySum(arrayMap(r -> if(r.1 = k, r.2, toUInt64(0)), reasons))) -
                  toInt64(arraySum(arrayMap(r -> if(r.1 = k, r.2, toUInt64(0)), prev_reasons))))),
               arrayDistinct(arrayConcat(arrayMap(r -> r.1, reasons), arrayMap(r -> r.1, prev_reasons)))) AS changes
    FROM steps
),
qualified AS (
    SELECT *, NOT conflicting AND NOT prev_conflicting AND map_known AND (reset OR prev_map_known) AND
           NOT arrayExists(r -> r.2 < 0, changes) AND
           arraySum(arrayMap(r -> r.2, changes)) <= increment AS attribution_known
    FROM reason_steps
),
summary AS (
    SELECT host, exporter, arrayDistinct(arrayFlatten(groupArray(sample_versions))) AS versions,
           count() AS samples, argMin(dropped, timestamp) AS first_dropped,
           argMax(dropped, timestamp) AS last_dropped, sum(increment) AS delta,
           max(queue_depth) AS max_queue_depth, sum(reset) AS reset_count,
           min(n) > 1 AS baseline_known,
           countIf(NOT attribution_known AND n > 1) AS unknown_reason_steps,
           sum(conflicting) AS conflicting_samples
    FROM qualified WHERE in_window GROUP BY host, exporter
),
reason_rows AS (
    SELECT host, exporter, r.1 AS reason, sum(r.2) AS reason_delta
    FROM qualified ARRAY JOIN changes AS r
    WHERE in_window AND n > 1 AND attribution_known AND r.2 > 0
    GROUP BY host, exporter, reason
    UNION ALL
    SELECT host, exporter, 'unattributed' AS reason,
           sum(toInt64(increment - if(attribution_known, toUInt64(arraySum(arrayMap(r -> r.2, changes))), toUInt64(0)))) AS reason_delta
    FROM qualified WHERE in_window GROUP BY host, exporter HAVING reason_delta > 0
    UNION ALL
    SELECT host, exporter, 'coverage_unknown' AS reason, toInt64(0) AS reason_delta
    FROM summary WHERE delta = 0 AND (NOT baseline_known OR reset_count > 0 OR unknown_reason_steps > 0 OR conflicting_samples > 0)
)
SELECT s.host AS host, s.exporter AS exporter, s.versions AS versions,
       s.samples AS samples, s.first_dropped AS first_dropped, s.last_dropped AS last_dropped,
       s.delta AS delta, s.max_queue_depth AS max_queue_depth,
       s.reset_count AS reset_count, s.baseline_known AS baseline_known,
       s.unknown_reason_steps AS unknown_reason_steps, s.conflicting_samples AS conflicting_samples,
       r.reason AS reason, r.reason_delta AS reason_delta
FROM summary AS s INNER JOIN reason_rows AS r ON s.host = r.host AND s.exporter = r.exporter
ORDER BY delta DESC, host, exporter, reason_delta DESC;

-- 5. Per-host last state: running / stopped / silent (Issue #10023). One row
--    per roster host, from the same roster and thresholds as query 3 (paste
--    the same rows between the markers; the committed rows are placeholders).
--    The three lifecycle signals the daemon already emits, no new record kinds:
--      daemon.start  loom.kind = 'daemon.event', loom.topic = 'daemon.start'
--      daemon.exit   loom.kind = 'daemon.exit' (clean, deliberate exit only)
--      host.export   loom.kind = 'host.export' (the heartbeat; one per
--                    host.health interval, carrying queue_depth, dropped_total
--                    and last_flush_ok_at)
--      stopped  the latest daemon.exit is newer than the latest daemon.start
--               (or no start is in the lookback): the daemon said it was going.
--      running  not stopped, and a host.export inside the host's threshold.
--               A start with no later exit and a fresh host.export is running,
--               including a restart after a crash (two starts, no exit between).
--      silent   not stopped and no host.export inside the threshold: crashed,
--               SIGKILLed, power loss, asleep, or export failing. Nothing
--               distinguishes these by absence alone; `cause` does the one
--               split the data allows, as in query 3: other Loom logs inside
--               the threshold = exporter_silent (export failing, host up),
--               none = host_silent (host down or asleep). `cause` is NULL
--               unless state = 'silent'.
--    A daemon that exits cleanly while its host.export is still inside the
--    threshold is `stopped`, not `running`. A host with no signal in the 48 h
--    lookback is `silent` (last_export NULL). The host key is `host.name`,
--    falling back to `host.id`.
WITH roster AS (
    SELECT host, telemetry
    FROM values('host String, telemetry String',
        /* loom:roster:begin */
        ('example-always', 'always'), ('example-when-awake', 'when_awake')
        /* loom:roster:end */
    )
),
seen AS (
    SELECT if(resources_string['host.name'] != '', resources_string['host.name'], resources_string['host.id']) AS host,
           maxIf(timestamp, attributes_string['loom.kind'] = 'host.export') AS last_export_ns,
           maxIf(timestamp, attributes_string['loom.kind'] = 'daemon.event'
                            AND attributes_string['loom.topic'] = 'daemon.start') AS last_start_ns,
           maxIf(timestamp, attributes_string['loom.kind'] = 'daemon.exit') AS last_exit_ns,
           maxIf(timestamp, attributes_string['loom.kind'] NOT IN ('', 'host.export', 'daemon.exit')) AS last_other_ns
    FROM signoz_logs.distributed_logs_v2
    WHERE timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 48 HOUR)
    GROUP BY host
),
judged AS (
    SELECT r.host AS host,
           r.telemetry AS telemetry,
           if(r.telemetry = 'always', 900, 86400) AS threshold_seconds,
           toInt64(ifNull(s.last_export_ns, 0)) AS le_ns,
           toInt64(ifNull(s.last_start_ns, 0)) AS ls_ns,
           toInt64(ifNull(s.last_exit_ns, 0)) AS lx_ns,
           toInt64(ifNull(s.last_other_ns, 0)) AS lo_ns,
           toInt64(toUnixTimestamp64Nano(now64(9))) AS now_ns
    FROM roster AS r
    LEFT JOIN seen AS s ON r.host = s.host
)
SELECT host,
       telemetry,
       multiIf(lx_ns > 0 AND lx_ns > ls_ns, 'stopped',
               le_ns > 0 AND intDiv(now_ns - le_ns, 1000000000) <= threshold_seconds, 'running',
               'silent') AS state,
       if(ls_ns = 0, NULL, fromUnixTimestamp64Nano(ls_ns)) AS last_start,
       if(lx_ns = 0, NULL, fromUnixTimestamp64Nano(lx_ns)) AS last_exit,
       if(le_ns = 0, NULL, fromUnixTimestamp64Nano(le_ns)) AS last_export,
       if(state = 'silent',
          if(lo_ns > 0 AND intDiv(now_ns - lo_ns, 1000000000) <= threshold_seconds,
             'exporter_silent', 'host_silent'),
          NULL) AS cause
FROM judged
ORDER BY host;
