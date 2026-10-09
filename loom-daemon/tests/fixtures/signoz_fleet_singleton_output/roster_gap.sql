-- Partial absence of a per-repo output (the PR #11030 Judge's blocking
-- finding), on the same clock as `incident.sql`:
--   silenced_at  1791342000  2026-10-07T03:00:00Z
--   evaluated_at 1791453600  2026-10-08T10:00:00Z
--
-- The expected repos of a `per_repo` row cannot come from the output being
-- watched: a repo that never emitted it, or stopped longer ago than the 72 h
-- window, has no record to be discovered from. The rule reads them from an
-- independent signal instead, the fleet's own per-repo activity in the logs
-- table (`pass.summary`, `role_tick.outcome`, `sweep.started`), each emitted
-- by the host that works the repo, never by the ETA producer.
--
-- One repo per case, while another repo stays healthy, so the fleet-level
-- `eta.fleet_refresh` series has seen > 0 and does not fire by itself:
--
--   org/r01  HEALTHY. eta.fleet_refresh hourly, eta.estimate every 30 min,
--            pass.summary every 5 min, all up to evaluated_at.
--   org/r02  NEVER EMITTED. pass.summary every 5 min, but no eta.fleet_refresh
--            record ever (the producing host does not cover it). No open
--            item either, so its eta.estimate silence is healthy.
--   org/r03  OUTAGE BEYOND THE WINDOW. eta.fleet_refresh hourly until
--            evaluated_at - 100 h, then nothing: 28 h older than the 72 h
--            window. Still worked on: a sweep started 2 h ago, its slug
--            spelled `Org/R03` (the rule compares slugs case-insensitively).
--   org/r04  QUIET BUT COVERED. eta.fleet_refresh hourly; role_tick.outcome
--            hourly; its only item was refused 30 h ago (all-abstaining), so
--            being on the roster must not make its eta.estimate owe.
--
-- Noise: a pass.summary with no `loom.repo` must not add a repo-less series.

INSERT INTO loom_fixture.anchor VALUES (1791342000, 1791453600);

-- pass.summary for org/r01 and org/r02: every 5 min over the last 80 h.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 80 * 3600 + k * 300) * 1000000000,
    '{}',
    map('loom.kind', 'pass.summary',
        'loom.repo', concat('org/r0', toString(n)),
        'loom.pass.mechanism', 'release-stale-blocked'),
    map(), map(),
    map('host.id', concat('host-worker-', toString(n)))
FROM (SELECT number + 1 AS n FROM numbers(2)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(960)) AS slots;

-- role_tick.outcome for org/r04: hourly over the last 80 h.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 80 * 3600 + k * 3600) * 1000000000,
    '{}',
    map('loom.kind', 'role_tick.outcome', 'loom.repo', 'org/r04', 'loom.role', 'judge'),
    map(), map(),
    map('host.id', 'host-worker-4')
FROM (SELECT number AS k FROM numbers(80)) AS slots;

-- Row 1: org/r03's sweep, 2 h ago, mixed-case slug.
-- Row 2: noise, a pass.summary with no loom.repo.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
    (toUInt64(1791453600 - 2 * 3600) * 1000000000, '{}',
     map('loom.kind', 'sweep.started', 'loom.repo', 'Org/R03'),
     map('loom.issue', 303), map(), map('host.id', 'host-worker-3')),
    (toUInt64(1791453600 - 600) * 1000000000, '{}',
     map('loom.kind', 'pass.summary'),
     map(), map(), map('host.id', 'host-worker-1'));

-- eta.fleet_refresh: org/r01 and org/r04 hourly up to evaluated_at
-- (slot k at evaluated_at - 79 h + k * 1 h, k = 0 .. 79).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 79 * 3600 + k * 3600) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fleet_refresh', 'loom.repo', concat('org/r0', toString(n))),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT arrayJoin([1, 4]) AS n) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(80)) AS slots;

-- eta.fleet_refresh: org/r03 hourly from evaluated_at - 130 h to
-- evaluated_at - 100 h, then nothing.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 130 * 3600 + k * 3600) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fleet_refresh', 'loom.repo', 'org/r03'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(31)) AS slots;

-- eta.estimate for org/r01 item 101: every 30 min up to evaluated_at.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 79 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r01', 'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(101)),
    map('loom.eta.primary', true),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(159)) AS slots;

-- org/r04 item 104: estimable until evaluated_at - 40 h, refused once at
-- evaluated_at - 30 h (as org/r30 in `healthy.sql`).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 70 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r04', 'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(104)),
    map('loom.eta.primary', true),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(61)) AS slots;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
    (toUInt64(1791453600 - 30 * 3600) * 1000000000, '{"kind":"land"}',
     map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r04', 'loom.eta.kind', 'land',
         'loom.eta.no_estimate_reason', 'blocked'),
     map('loom.issue', 104), map('loom.eta.primary', true), map('host.id', 'host-authority'));

-- eta.fit and eta.backtest.fold fresh, as in `incident.sql`.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 72 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fit'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 75 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.backtest.fold'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;
