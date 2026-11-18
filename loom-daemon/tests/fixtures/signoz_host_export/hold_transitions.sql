-- Fixture for the telemetry-hold-transitions replay in
-- signoz_host_export_queries.rs (#14094). Timestamps are literal unix
-- nanoseconds because the CLI inlines the window bounds; the replay builds
-- its statement around the exact [1000000000, 5000000000) window below.
-- Same DDL as fixture.sql: `timestamp` is UInt64 ns.
-- Columns: timestamp ns, loom.repo, loom.kind, loom.hold.reason_code, body.

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
SELECT ts, 0, body,
       map('loom.kind', kind, 'loom.repo', repo, 'loom.hold.reason_code', code),
       map(), map(), map()
FROM values('ts UInt64, repo String, kind String, code String, body String',
    -- in the window, matching repo and reason_code (kept)
    (2000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', '{"pr":42}'),
    -- right reason, wrong repo: dropped by the bound repo param
    (2500000000, 'o/two', 'hold.transition', 'merge-risk-self-release', '{"pr":7}'),
    -- right repo, wrong reason: dropped by the bound reason_code param
    (3000000000, 'o/one', 'hold.transition', 'merge-risk-pass', '{"pr":8}'),
    -- wrong kind: dropped by the loom.kind filter
    (3500000000, 'o/one', 'sweep.event', 'merge-risk-self-release', '{"pr":9}'),
    -- window edges: since is inclusive, until exclusive
    (1000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', '{"pr":10}'),
    (5000000000, 'o/one', 'hold.transition', 'merge-risk-self-release', '{"pr":11}')
);
