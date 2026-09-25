-- Ready-queue dwell and starvation (Issue #8856). Standing queries over the
-- `loom.queue.*` metric.points names; run with the bundled clickhouse-client in
-- the private ClickHouse container. Metric timestamps are milliseconds.
-- Queries 1-3 take max(), so duplicate time_series_v4 hour-rows are harmless;
-- query 4 sums and must de-duplicate. Resource attributes (host.id) are merged
-- into metric labels by the SigNoz exporter; inspect `time_series_v4.labels` first if a host column is empty.
--
-- Dwell is a lower bound (seeded from the issue's updatedAt, never earlier),
-- so these numbers can under-report a wait, never over-report it.

-- 1. Starved issues per host and state, last 24 h (the alert's signal).
SELECT JSONExtractString(t.labels, 'host.id') AS host,
       JSONExtractString(t.labels, 'state') AS state,
       toStartOfInterval(toDateTime(intDiv(s.unix_milli, 1000)), INTERVAL 5 MINUTE) AS bucket,
       max(s.value) AS starved
FROM signoz_metrics.samples_v4 AS s
INNER JOIN signoz_metrics.time_series_v4 AS t USING (fingerprint)
WHERE s.metric_name = 'loom.queue.starved'
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 24 HOUR) * 1000
GROUP BY host, state, bucket
HAVING starved > 0
ORDER BY bucket, host, state;

-- 2. Why the starved issues are held (queue disposition), last 24 h.
SELECT JSONExtractString(t.labels, 'reason') AS reason,
       max(s.value) AS peak_starved
FROM signoz_metrics.samples_v4 AS s
INNER JOIN signoz_metrics.time_series_v4 AS t USING (fingerprint)
WHERE s.metric_name = 'loom.queue.starved.by_reason'
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 24 HOUR) * 1000
GROUP BY reason
ORDER BY peak_starved DESC;

-- 3. Oldest waiting issue per host and state, hourly peak, last 7 days.
SELECT JSONExtractString(t.labels, 'host.id') AS host,
       JSONExtractString(t.labels, 'state') AS state,
       toStartOfHour(toDateTime(intDiv(s.unix_milli, 1000))) AS hour,
       round(max(s.value) / 3600, 2) AS oldest_wait_hours
FROM signoz_metrics.samples_v4 AS s
INNER JOIN signoz_metrics.time_series_v4 AS t USING (fingerprint)
WHERE s.metric_name = 'loom.queue.oldest_wait'
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 7 DAY) * 1000
GROUP BY host, state, hour
ORDER BY hour, host, state;

-- 4. Mean dispatch wait (queue turnaround) per host per day, last 30 days:
--    summed delta seconds divided by summed samples.
SELECT JSONExtractString(t.labels, 'host.id') AS host,
       toDate(toDateTime(intDiv(s.unix_milli, 1000))) AS day,
       sumIf(s.value, s.metric_name = 'loom.queue.dispatch_wait') AS wait_secs,
       sumIf(s.value, s.metric_name = 'loom.queue.dispatch_wait.samples') AS dispatches,
       round(wait_secs / nullIf(dispatches, 0) / 60, 1) AS mean_wait_minutes
FROM signoz_metrics.samples_v4 AS s
-- time_series_v4 holds one row per series per hour, so join a de-duplicated
-- fingerprint set; a plain join would count each sample once per hour-row.
INNER JOIN (SELECT fingerprint, any(labels) AS labels
            FROM signoz_metrics.time_series_v4
            WHERE metric_name IN ('loom.queue.dispatch_wait', 'loom.queue.dispatch_wait.samples')
            GROUP BY fingerprint) AS t USING (fingerprint)
WHERE s.metric_name IN ('loom.queue.dispatch_wait', 'loom.queue.dispatch_wait.samples')
  AND s.unix_milli >= toUnixTimestamp(now() - INTERVAL 30 DAY) * 1000
GROUP BY host, day
ORDER BY day, host;
