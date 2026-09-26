-- Local-trial retention split (#8826): logs and traces 7 days, metrics 30 days.
-- Run AFTER setting the same split in SigNoz General Settings -> Retention
-- (logs 7 days, traces 7 days, metrics 30 days). The application API owns the
-- active signal tables and standard rollups; it leaves these existing
-- auxiliary/legacy tables at their upstream 15/30-day (or one-month) TTLs, so
-- this file sets them explicitly. Every statement is an idempotent
-- MODIFY TTL, so it is safe to re-run after any upgrade or settings change.
--
-- Metrics (>= 30 days) are the CI retro's trend asset: see "Retention" in
-- ../../docs/ci-observability.md. The metric statements below set exactly 30
-- days — they are no-ops on a fresh render (already upstream 30 days) and
-- RESTORE 30 days on a trial that ran the earlier all-seven-day version of
-- this file, which shortened them.
--
-- Apply only to this pinned isolated trial, then re-run queries.sql and the
-- effective-DDL check in the README's "Retention and operation" section.
-- Preserve schema/configuration tables and the resource-table 30-minute grace.

-- Logs and traces: 7 days.
ALTER TABLE signoz_logs.tag_attributes_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.tag_attributes_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.durationSort ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.signoz_index_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.signoz_spans ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp) + INTERVAL 7 DAY;
ALTER TABLE signoz_traces.top_level_operations ON CLUSTER cluster
    MODIFY TTL time + INTERVAL 7 DAY;

-- Metrics: 30 days.
ALTER TABLE signoz_metrics.metadata ON CLUSTER cluster
    MODIFY TTL toDateTime(last_reported_unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp_ms / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced_1day ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 30 DAY;
