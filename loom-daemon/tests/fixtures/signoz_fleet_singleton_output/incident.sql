-- The 2026-10-07/08 incident, replayed (Issue #10916): the ETA authority moved
-- to a host covering 2 of ~30 repos with no OTLP exporter, every host stayed
-- "up", and 28 repos received no `eta.estimate` for about 31 h.
--
-- Clock (seconds since the epoch):
--   silenced_at  1791342000  2026-10-07T03:00:00Z  the authority moves
--   evaluated_at 1791453600  2026-10-08T10:00:00Z  silenced_at + 31 h
--
-- Until `silenced_at` the fleet is healthy: 30 repos `org/r01` .. `org/r30`,
-- one open estimable item each (`loom.issue` = 100 + n), refreshed every
-- 30 min (the registry's `eta.estimate` cadence) from 80 h before the
-- outage, and one `eta.fleet_refresh` record per repo every hour. From
-- `silenced_at` on only `org/r01` and `org/r02` keep both outputs flowing.
--
-- `eta.fit` (daily at 03:00) and `eta.backtest.fold` (stamped at each day's
-- 00:00 cutoff) keep running throughout, so the outage is exactly the two
-- per-repo rows: the test can name every series that must fire.

INSERT INTO loom_fixture.anchor VALUES (1791342000, 1791453600);

-- eta.estimate: every 30 min, slot k at silenced_at - 80 h + k * 30 min.
-- Repos 3..30 stop at slot 160 (= silenced_at); repos 1..2 run to slot 222
-- (= evaluated_at).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate',
        'loom.repo', concat('org/r', leftPad(toString(n), 2, '0')),
        'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(100 + n)),
    map('loom.eta.primary', true),
    map('host.id', if(n <= 2, 'host-new-authority', 'host-old-authority'))
FROM (SELECT number + 1 AS n FROM numbers(30)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(223)) AS slots
WHERE k <= 160 OR n <= 2;

-- eta.fleet_refresh: hourly, slot k at silenced_at - 80 h + k * 1 h. Same cut.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 3600) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fleet_refresh',
        'loom.repo', concat('org/r', leftPad(toString(n), 2, '0'))),
    map(),
    map(),
    map('host.id', if(n <= 2, 'host-new-authority', 'host-old-authority'))
FROM (SELECT number + 1 AS n FROM numbers(30)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(112)) AS slots
WHERE k <= 80 OR n <= 2;

-- eta.fit: daily at 03:00, the last one at 2026-10-08T03:00 (7 h old).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 72 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fit'),
    map(), map(),
    map('host.id', 'host-new-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;

-- eta.backtest.fold: stamped at each day's 00:00 cutoff, the last one at
-- 2026-10-08T00:00 (10 h old).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 75 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.backtest.fold'),
    map(), map(),
    map('host.id', 'host-new-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;

-- Row 1, noise: a kind no registry row names. It must never produce a series.
-- Row 2: org/r03's item STARTED 10 min before the outage, a `start` outcome
-- while the issue stays open. Only a `land` outcome closes an item, so org/r03
-- must still fire.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
    (toUInt64(1791453600 - 60) * 1000000000, 'sweep.outcome',
     map('loom.kind', 'sweep.outcome', 'loom.repo', 'org/r05'), map(), map(),
     map('host.id', 'host-old-authority')),
    (toUInt64(1791342000 - 600) * 1000000000, '{}',
     map('loom.kind', 'eta.outcome', 'loom.repo', 'org/r03', 'loom.eta.kind', 'start',
         'loom.eta.outcome', 'started'),
     map('loom.issue', 103), map(), map('host.id', 'host-old-authority'));
