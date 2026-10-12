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

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, observed_timestamp, body, attributes_string, attributes_number,
     attributes_bool, resources_string)
SELECT toUnixTimestamp64Nano(now64(9) - toIntervalSecond(mins * 60)), 0, '',
       map('loom.kind', kind),
       if(kind = 'host.export', map('loom.host_export.dropped_total', dropped), map()),
       map(),
       map('host.name', h, 'host.id', h)
FROM values('h String, kind String, mins Int32, dropped Float64',
    -- roster cases (queries 3 and the alert)
    ('a-ok', 'host.export', 1, 0), ('a-ok', 'host.export', 6, 0), ('a-ok', 'host.export', 11, 0),
    ('a-ok', 'sweep.event', 1, 0),
    ('a-silent', 'host.export', 20, 0), ('a-silent', 'host.export', 25, 0),
    ('a-silent', 'sweep.event', 1, 0), ('a-silent', 'sweep.event', 10, 0), ('a-silent', 'sweep.event', 20, 0),
    ('a-down', 'host.export', 20, 0), ('a-down', 'host.export', 25, 0),
    ('a-down', 'sweep.event', 20, 0), ('a-down', 'sweep.event', 25, 0),
    ('a-edge', 'host.export', 14, 0), ('a-edge', 'sweep.event', 1, 0),
    ('w-asleep', 'host.export', 180, 0), ('w-asleep', 'sweep.event', 180, 0),
    ('w-gone', 'host.export', 1500, 0), ('w-gone', 'sweep.event', 1, 0), ('w-gone', 'sweep.event', 100, 0),
    ('w-never', 'sweep.event', 1, 0), ('w-never', 'sweep.event', 60, 0),
    ('off-roster', 'sweep.event', 1, 0),
    -- drop-counter cases (query 1); none emit other logs, so none reach query 2
    ('q1-inc', 'host.export', 90, 5), ('q1-inc', 'host.export', 30, 8),
    ('q1-flat', 'host.export', 90, 5), ('q1-flat', 'host.export', 30, 5),
    ('q1-reset', 'host.export', 90, 10), ('q1-reset', 'host.export', 60, 14), ('q1-reset', 'host.export', 30, 2),
    ('q1-nobase', 'host.export', 60, 7), ('q1-nobase', 'host.export', 30, 9),
    ('q1-base', 'host.export', 180, 4), ('q1-base', 'host.export', 30, 6)
);
