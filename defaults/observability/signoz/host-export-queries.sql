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
-- STATUS: queries 1-3 are executed verbatim against the pinned ClickHouse by
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

-- 4. Per-exporter, per-reason loss in the last 2 h (Issue #11639): which
--    exporter (`https` vs `otlp`) on which host lost records, and why.
--    Query 1 stops at the host-level sum; this reads the `exporters` array in
--    the JSON body. `dropped_by_reason` holds the durable loss-ledger
--    breakdown (`queue_overflow`, `capacity_shrink`, `status_<code>`,
--    `non_json_response`, `response_too_large`, `invalid_response`,
--    `receiver_rejected`, `trace_unmappable`). The map omits zero counts, so
--    no entry for a reason means 0 at that sample. Facts only, read-only:
--    `first_dropped`/`last_dropped` are the cumulative counter at the
--    exporter's first/last in-window sample and `delta` is `last - first`
--    (negative or zero reads as no loss in the window; a ledger reset shows
--    as a negative delta, never a recovery). `reason_delta` is the reason's
--    count at the exporter's last sample carrying it minus its count at the
--    exporter's first in-window sample, where a reason first seen after that
--    sample starts from 0 (absent -> N counts N). Any part of `delta` the
--    reasons do not explain (samples from before #11527 carry no
--    breakdown) is reported as one extra row, reason `unattributed`.
--    Drops before the first in-window sample are not counted (no pre-window
--    baseline, unlike query 1).
--    Also reports `max_queue_depth` and the daemon versions seen, so a loss
--    can be tied to the running build rather than to a merged fix.
--    Durable ledger counts only: session-only admission deferrals are not
--    `host.export` drops and never appear here.
--    STATUS: written against the body shape in
--    telemetry/kinds/host_export.rs; not yet replayed by
--    signoz_host_export_queries.rs, so treat the first live run as its check.
WITH samples AS (
    SELECT if(resources_string['host.name'] != '', resources_string['host.name'], resources_string['host.id']) AS host,
           resources_string['service.version'] AS version,
           timestamp,
           JSONExtractString(e, 'name') AS exporter,
           JSONExtractUInt(e, 'dropped_total') AS dropped_total,
           JSONExtractUInt(e, 'queue_depth') AS queue_depth,
           JSONExtractKeysAndValues(e, 'dropped_by_reason', 'UInt64') AS by_reason
    FROM signoz_logs.distributed_logs_v2
    ARRAY JOIN JSONExtractArrayRaw(body, 'exporters') AS e
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
),
per_reason AS (
    SELECT host, exporter, tupleElement(r, 1) AS reason,
           min(timestamp) AS r_first_ts,
           argMin(tupleElement(r, 2), timestamp) AS first_n,
           argMax(tupleElement(r, 2), timestamp) AS last_n
    FROM samples
    ARRAY JOIN by_reason AS r
    GROUP BY host, exporter, reason
),
per_exporter AS (
    SELECT host, exporter,
           min(timestamp) AS e_first_ts,
           argMin(dropped_total, timestamp) AS first_dropped,
           argMax(dropped_total, timestamp) AS last_dropped,
           max(queue_depth) AS max_queue_depth,
           groupUniqArray(version) AS versions,
           count() AS samples
    FROM samples
    GROUP BY host, exporter
),
-- A reason absent at the exporter's first in-window sample had 0 there.
reason_deltas AS (
    SELECT r.host AS host, r.exporter AS exporter, r.reason AS reason,
           toInt64(r.last_n) - toInt64(if(r.r_first_ts > e.e_first_ts, 0, r.first_n)) AS reason_delta
    FROM per_reason AS r
    INNER JOIN per_exporter AS e ON r.host = e.host AND r.exporter = e.exporter
),
attributed AS (
    SELECT host, exporter, sum(reason_delta) AS attributed_delta
    FROM reason_deltas
    GROUP BY host, exporter
),
reason_rows AS (
    SELECT host, exporter, reason, reason_delta FROM reason_deltas
    UNION ALL
    SELECT e.host AS host, e.exporter AS exporter, 'unattributed' AS reason,
           toInt64(e.last_dropped) - toInt64(e.first_dropped) - toInt64(a.attributed_delta) AS reason_delta
    FROM per_exporter AS e
    LEFT JOIN attributed AS a ON e.host = a.host AND e.exporter = a.exporter
    WHERE reason_delta > 0
)
SELECT e.host AS host,
       e.exporter AS exporter,
       e.versions AS versions,
       e.samples AS samples,
       e.first_dropped AS first_dropped,
       e.last_dropped AS last_dropped,
       toInt64(e.last_dropped) - toInt64(e.first_dropped) AS delta,
       e.max_queue_depth AS max_queue_depth,
       r.reason AS reason,
       r.reason_delta AS reason_delta
FROM per_exporter AS e
LEFT JOIN reason_rows AS r ON e.host = r.host AND e.exporter = r.exporter
WHERE delta > 0
ORDER BY delta DESC, host, exporter, reason_delta DESC;
