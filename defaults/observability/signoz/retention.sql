-- Optional local-trial parity after setting seven-day retention through SigNoz.
-- The application API leaves these existing auxiliary/legacy TTLs at 15/30 days.
-- Apply only to this pinned isolated trial, then re-run queries.sql.
-- Preserve schema/configuration tables and the resource-table 30-minute grace.
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
ALTER TABLE signoz_metrics.metadata ON CLUSTER cluster
    MODIFY TTL toDateTime(last_reported_unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v2 ON CLUSTER cluster
    MODIFY TTL toDateTime(timestamp_ms / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_last_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_30m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_5m ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.samples_v4_reduced_sum_60s ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
ALTER TABLE signoz_metrics.time_series_v4_reduced_1day ON CLUSTER cluster
    MODIFY TTL toDateTime(unix_milli / 1000) + INTERVAL 7 DAY;
