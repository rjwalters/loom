-- Fixture for query 5 of host-export-queries.sql (#10023): per-host
-- running / stopped / silent. Timestamps are relative to now64(9).
-- Columns: host, loom.kind, loom.topic, minutes ago. host.name = host.id = host.
-- daemon.start is a `daemon.event` log whose loom.topic is `daemon.start`;
-- daemon.exit and host.export are their own loom.kind.

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
       map('loom.kind', kind, 'loom.topic', topic),
       map(), map(),
       map('host.name', h, 'host.id', h)
FROM values('h String, kind String, topic String, mins Int32',
    -- running: started, exporting
    ('s-run', 'daemon.event', 'daemon.start', 120), ('s-run', 'host.export', '', 1), ('s-run', 'host.export', '', 6),
    -- stopped: clean exit newer than start; a host.export inside the threshold must not make it running
    ('s-stop', 'daemon.event', 'daemon.start', 60), ('s-stop', 'daemon.exit', '', 30), ('s-stop', 'host.export', '', 31),
    -- silent, exporter_silent: other Loom logs are fresh, host.export is not
    ('s-expfail', 'daemon.event', 'daemon.start', 120), ('s-expfail', 'host.export', '', 30), ('s-expfail', 'sweep.event', '', 1),
    -- running: restart after a crash (two starts, no exit between), exporting again
    ('s-restart', 'daemon.event', 'daemon.start', 300), ('s-restart', 'daemon.event', 'daemon.start', 200),
    ('s-restart', 'host.export', '', 1),
    -- silent, host_silent: crashed (two starts, no exit), nothing recent
    ('s-crash', 'daemon.event', 'daemon.start', 300), ('s-crash', 'daemon.event', 'daemon.start', 200),
    ('s-crash', 'host.export', '', 100),
    -- running: stopped earlier, started again since
    ('s-back', 'daemon.exit', '', 100), ('s-back', 'daemon.event', 'daemon.start', 50), ('s-back', 'host.export', '', 1),
    -- running: a when_awake laptop that last exported 16 h ago (threshold 24 h)
    ('s-laptop', 'daemon.event', 'daemon.start', 1000), ('s-laptop', 'host.export', '', 960),
    -- off roster, must not be reported
    ('s-off', 'host.export', '', 1)
);
