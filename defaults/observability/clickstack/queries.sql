-- Run read-only via HyperDX SQL Explorer or clickhouse-client --multiquery.
-- Change the fixture trace ID to a real Loom canary ID for final acceptance.
SELECT SpanName, TraceId, SpanId, ParentSpanId, Timestamp, Duration,
       StatusCode, SpanAttributes
FROM default.otel_traces
WHERE TraceId = '85270000000000000000000000000001'
ORDER BY Timestamp;

SELECT Timestamp, SeverityText, Body, TraceId, SpanId, LogAttributes
FROM default.otel_logs
WHERE TraceId = '85270000000000000000000000000001'
ORDER BY Timestamp;

SELECT MetricName, MetricUnit, TimeUnix, Value, Attributes
FROM default.otel_metrics_gauge
WHERE ServiceName = 'loom-trial-fixture'
ORDER BY TimeUnix, MetricName;

-- Retention applies to base and derived tables; views themselves have no TTL.
SELECT name, engine, create_table_query
FROM system.tables
WHERE database = 'default' AND name LIKE 'otel_%'
ORDER BY name;

SELECT table, sum(rows) AS rows, sum(bytes_on_disk) AS bytes_on_disk
FROM system.parts
WHERE active AND database = 'default'
GROUP BY table ORDER BY table;

-- Repeated deliveries must be measured separately from unique spans.
SELECT count() AS rows, uniqExact((TraceId, SpanId)) AS unique_spans
FROM default.otel_traces
WHERE ServiceName = 'loom-trial-fixture';
