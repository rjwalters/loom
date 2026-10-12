-- Hosts losing telemetry, or not reporting their export view (Issue #11124,
-- R2 of #10196), from the `host.export` log the daemon emits once per
-- `host.health` interval. Facts only: the daemon computes no rate.
--
-- `loom.host_export.dropped_total` is cumulative per daemon process, so it
-- restarts at 0 when a daemon restarts. A window's drops are therefore summed
-- as positive steps between consecutive samples; a decrease is a counter reset
-- and contributes the post-reset value, never a negative delta.
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
