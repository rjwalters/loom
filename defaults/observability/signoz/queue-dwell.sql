-- Ready-queue dwell and starvation (Issue #8856). Standing queries over the
-- `loom.queue.*` metric.points names; run with the bundled clickhouse-client in
-- the private ClickHouse container. Metric timestamps are milliseconds.
-- Queries 1-3 take max(), so duplicate time_series_v4 hour-rows are harmless;
-- query 4 sums and must de-duplicate. Resource attributes (host.id) are merged
-- into metric labels by the SigNoz exporter; inspect `time_series_v4.labels` first if a host column is empty.
--
-- Dwell is a lower bound (seeded from the issue's updatedAt, never earlier),
-- so these numbers can under-report a wait, never over-report it.
--
-- Query 5 (at the end of this file) is different in kind from 1-4: it reads
-- `signoz_traces.signoz_index_v3` (per-issue `loom.dispatch.disposition` /
-- `loom.dispatch.admission` SPANS, Issue #9222), not `signoz_metrics` (the
-- low-cardinality `loom.queue.*` gauges below, which never carry a repo or
-- issue number).

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

-- 5. Why hasn't owner/repo#98 started? Latest disposition/admission records
--    first, across every host. `loom.dispatch.disposition` covers every
--    candidate the tick evaluated (including one filtered out before a
--    dispatch() attempt); `loom.dispatch.admission` adds the outcome of an
--    actual attempt when one was made. Reads spans, not metrics — see the
--    header note above.
SELECT timestamp, resources_string['host.id'] AS host,
       name,
       attributes_string['loom.queue.disposition'] AS disposition,
       attributes_string['loom.queue.state']       AS state,
       attributes_string['loom.queue.rank']        AS rank,
       -- Issue #9669: queue position at the sampled tick — candidate_rank is
       -- the plan's pass-2 position when the row has one, else its rank.
       attributes_string['loom.queue.candidate_rank']   AS candidate_rank,
       attributes_string['loom.queue.total_candidates'] AS total_candidates,
       attributes_string['loom.queue.priority_score']   AS priority_score,
       attributes_string['loom.queue.transition']  AS transition,
       attributes_string['loom.queue.park_label']  AS park_label,
       attributes_string['loom.queue.halt_cause']  AS halt_cause,
       attributes_string['loom.dispatch.admission_result'] AS admission_result,
       attributes_string['loom.dispatch.reason']           AS admission_reason
FROM signoz_traces.signoz_index_v3
WHERE name IN ('loom.dispatch.disposition', 'loom.dispatch.admission')
  AND attributes_string['loom.repo']  = 'owner/repo'
  AND attributes_string['loom.issue'] = '98'
  AND timestamp > now() - INTERVAL 24 HOUR
ORDER BY timestamp DESC LIMIT 20;

-- A `workspace_halted` row names its cause on `loom.queue.halt_cause` (#9017
-- token: main_red / gate_pending / token_pool / preflight_advisory / drain /
-- breaker; #9673). The column is empty for a cause-less legacy row — join to
-- the parent `loom.dispatch.tick` span
-- (`attributes_string['loom.dispatch.result'] = 'halted_main_red'`) through
-- `parentSpanID` there.
