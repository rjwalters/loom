-- Hosts losing telemetry, or not reporting their export view (Issue #11124,
-- R2 of #10196), from the `host.export` log the daemon emits once per
-- `host.health` interval. Facts only: the daemon computes no rate.
--
-- `loom.host_export.dropped_total` is cumulative per daemon process, so it
-- restarts at 0 when a daemon restarts. A window's drops are therefore summed
-- as positive steps between consecutive samples; a decrease is a counter reset
-- and contributes the post-reset value, never a negative delta.
--
-- STATUS: not yet executed in CI against the pinned ClickHouse.

-- 1. Hosts with drops in the last 2 h, worst first.
WITH samples AS (
    SELECT resource_attributes_string['host.id'] AS host,
           toUInt64(attributes_number['loom.host_export.dropped_total']) AS dropped,
           timestamp
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
),
steps AS (
    SELECT host,
           dropped,
           lagInFrame(dropped) OVER (PARTITION BY host ORDER BY timestamp
                                     ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS prev,
           row_number() OVER (PARTITION BY host ORDER BY timestamp) AS n
    FROM samples
)
SELECT host,
       sum(multiIf(n = 1, 0, dropped >= prev, dropped - prev, dropped)) AS dropped_in_window
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
    SELECT resource_attributes_string['host.id'] AS host,
           max(timestamp) AS last_seen
    FROM signoz_logs.distributed_logs_v2
    WHERE timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
      AND attributes_string['loom.kind'] != ''
      AND attributes_string['loom.kind'] != 'host.export'
    GROUP BY host
) AS h
LEFT ANTI JOIN (
    SELECT DISTINCT resource_attributes_string['host.id'] AS host
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'host.export'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 2 HOUR)
) AS e ON h.host = e.host
ORDER BY h.host;
