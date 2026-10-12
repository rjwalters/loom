-- Fixture for signoz_host_export_queries.rs (#11198). Timestamps are relative
-- to now64(9) because host-export-queries.sql reads the clock verbatim.
-- Columns: host, loom.kind, minutes ago, loom.host_export.dropped_total.
-- host.name and host.id are both set to the host.

CREATE DATABASE IF NOT EXISTS signoz_logs;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    observed_timestamp UInt64,
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

-- Fixed-clock query 4 regression. Duplicate delivery and conflicting same-time
-- records are deliberate; the captain sequence models the reported 2026-10-12 reset.
INSERT INTO signoz_logs.distributed_logs_v2
(timestamp, body, attributes_string, resources_string)
SELECT toUnixTimestamp64Nano(parseDateTime64BestEffort(concat('2026-10-12 ', t), 9, 'UTC')),
       b, map('loom.kind','host.export'),
       map('host.name', h, 'host.id', h, 'service.version','0.19.1127')
FROM values('h String, t String, b String',
('captain', '15:00:00', '{"exporters":[{"name":"https","dropped_total":10000,"queue_depth":0}]}'),
('captain', '16:11:03', '{"exporters":[{"name":"https","dropped_total":11241,"queue_depth":0}]}'),
('captain', '16:14:27', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('captain', '16:49:27', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('captain', '16:54:27', '{"exporters":[{"name":"https","dropped_total":851,"queue_depth":2,"dropped_by_reason":{"queue_overflow":851}}]}'),
('captain', '16:54:27', '{"exporters":[{"name":"https","dropped_total":851,"queue_depth":2,"dropped_by_reason":{"queue_overflow":851}}]}'),
('captain', '16:59:27', '{"exporters":[{"name":"https","dropped_total":851,"queue_depth":0,"dropped_by_reason":{"queue_overflow":851}}]}'),
('captain', '17:04:27', '{"exporters":[{"name":"https","dropped_total":1584,"queue_depth":0,"dropped_by_reason":{"queue_overflow":1584}}]}'),
('captain', '15:00:00', '{"exporters":[{"name":"otlp","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('captain', '17:04:27', '{"exporters":[{"name":"otlp","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('flat', '15:00:00', '{"exporters":[{"name":"https","dropped_total":7,"queue_depth":0,"dropped_by_reason":{"queue_overflow":7}}]}'),
('flat', '17:00:00', '{"exporters":[{"name":"https","dropped_total":7,"queue_depth":0,"dropped_by_reason":{"queue_overflow":7}}]}'),
('unknown', '15:10:00', '{"exporters":[{"name":"https","dropped_total":100,"queue_depth":0,"dropped_by_reason":{"queue_overflow":100}}]}'),
('unknown', '17:00:00', '{"exporters":[{"name":"https","dropped_total":102,"queue_depth":0,"dropped_by_reason":{"queue_overflow":102}}]}'),
('unknown-flat', '16:00:00', '{"exporters":[{"name":"https","dropped_total":99,"queue_depth":0,"dropped_by_reason":{"queue_overflow":99}}]}'),
('reason-reset', '15:00:00', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('reason-reset', '15:10:00', '{"exporters":[{"name":"https","dropped_total":2,"queue_depth":0,"dropped_by_reason":{"a":2}}]}'),
('reason-reset', '16:00:00', '{"exporters":[{"name":"https","dropped_total":3,"queue_depth":0,"dropped_by_reason":{"a":2,"b":1}}]}'),
('reason-reset', '16:10:00', '{"exporters":[{"name":"https","dropped_total":1,"queue_depth":0,"dropped_by_reason":{"b":1}}]}'),
('reason-reset', '16:20:00', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('reason-reset', '17:00:00', '{"exporters":[{"name":"https","dropped_total":2,"queue_depth":0,"dropped_by_reason":{"a":2}}]}'),
('legacy', '15:00:00', '{"exporters":[{"name":"https","dropped_total":10,"queue_depth":0}]}'),
('legacy', '16:00:00', '{"exporters":[{"name":"https","dropped_total":12,"queue_depth":0,"dropped_by_reason":{"a":12}}]}'),
('legacy', '17:00:00', '{"exporters":[{"name":"https","dropped_total":13,"queue_depth":0,"dropped_by_reason":{"a":13}}]}'),
('inconsistent', '15:00:00', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('inconsistent', '17:00:00', '{"exporters":[{"name":"https","dropped_total":2,"queue_depth":0,"dropped_by_reason":{"a":4}}]}'),
('conflict', '15:00:00', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('conflict', '17:00:00', '{"exporters":[{"name":"https","dropped_total":2,"queue_depth":0,"dropped_by_reason":{"a":2}}]}'),
('conflict', '17:00:00', '{"exporters":[{"name":"https","dropped_total":3,"queue_depth":0,"dropped_by_reason":{"a":3}}]}'),
('outside', '15:09:59', '{"exporters":[{"name":"https","dropped_total":0,"queue_depth":0,"dropped_by_reason":{}}]}'),
('outside', '17:10:01', '{"exporters":[{"name":"https","dropped_total":5,"queue_depth":0,"dropped_by_reason":{"a":5}}]}'));
