-- Fixture for query 5 of host-export-queries.sql (#10023). Timestamps are
-- relative to now64(9) because the query reads the clock verbatim.
-- Columns: host, loom.kind, loom.topic ('' when none), minutes ago.
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
       if(topic = '', map('loom.kind', kind), map('loom.kind', kind, 'loom.topic', topic)),
       map(), map(),
       map('host.name', h, 'host.id', h)
FROM values('h String, kind String, topic String, mins Int32',
    -- running: started, exporting inside 900 s
    ('l-running', 'daemon.event', 'daemon.start', 120),
    ('l-running', 'host.export', '', 1), ('l-running', 'host.export', '', 6),
    -- stopped: clean exit after the start, the last export before the exit
    ('l-stopped', 'daemon.event', 'daemon.start', 120),
    ('l-stopped', 'host.export', '', 60), ('l-stopped', 'daemon.exit', '', 50),
    -- silent: no exit, exports stopped 40 min ago (crashed, killed or asleep)
    ('l-crashed', 'daemon.event', 'daemon.start', 120),
    ('l-crashed', 'host.export', '', 40), ('l-crashed', 'host.export', '', 45),
    -- restart after a crash: two starts with no exit between, exporting now
    ('l-restart-running', 'daemon.event', 'daemon.start', 300),
    ('l-restart-running', 'daemon.event', 'daemon.start', 100),
    ('l-restart-running', 'host.export', '', 1),
    -- restart after a crash that is silent again
    ('l-restart-silent', 'daemon.event', 'daemon.start', 300),
    ('l-restart-silent', 'daemon.event', 'daemon.start', 100),
    ('l-restart-silent', 'host.export', '', 50),
    -- exit, then a later start and fresh export: running
    ('l-exit-restart', 'daemon.event', 'daemon.start', 300),
    ('l-exit-restart', 'daemon.exit', '', 200),
    ('l-exit-restart', 'daemon.event', 'daemon.start', 100),
    ('l-exit-restart', 'host.export', '', 1),
    -- exporting with no lifecycle record in the lookback: running
    ('l-nostart', 'host.export', '', 2),
    -- a laptop asleep 3 h is inside its 86400 s threshold: running
    ('w-asleep', 'daemon.event', 'daemon.start', 600),
    ('w-asleep', 'host.export', '', 180),
    -- when_awake past its threshold: silent
    ('w-gone', 'daemon.event', 'daemon.start', 2000),
    ('w-gone', 'host.export', '', 1500),
    -- a daemon.event that is not a start must not count as one
    ('l-othertopic', 'daemon.event', 'daemon.drain.started', 1),
    ('l-othertopic', 'host.export', '', 40));
    -- l-never has no rows at all: silent
