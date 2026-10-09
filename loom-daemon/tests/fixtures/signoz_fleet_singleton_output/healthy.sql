-- A healthy but quiet fleet (Issue #10916: "A healthy fleet doesn't fire"),
-- on the same clock as `incident.sql`:
--   silenced_at  1791342000  2026-10-07T03:00:00Z  (nothing is silenced here)
--   evaluated_at 1791453600  2026-10-08T10:00:00Z
--
-- Every output the rule watches is within its deadline, and the two shapes
-- that legitimately go quiet are present, so the per-active-repo row has
-- something to get wrong (the #10943 Judge's Required finding 1):
--
--   org/r01 .. org/r28  one estimable item each, refreshed every 30 min.
--   org/r29  IDLE. Item 129 was estimated until 20 h ago and then landed
--            (a `land` eta.outcome 5 min after its last estimate). Item 229
--            was estimated until 3 h ago and its `land` outcome is stamped
--            10 min BEFORE that last estimate: the outcome is stamped at the
--            landing instant, and estimates made in the pass before the
--            daemon learned of the landing trail it (`eta/tracker.rs`
--            `resolve`). Nothing is open, so nothing is owed.
--   org/r30  ALL-ABSTAINING. Item 130 was estimable until 40 h ago, then
--            refused (`loom.eta.no_estimate_reason`) once at 30 h ago. A
--            refusal is emitted once and never refreshed (`eta/emit.rs`), so
--            silence after it is healthy.
--
-- `eta.fleet_refresh` covers all 30 repos hourly (PerRepo: idle or not);
-- `eta.fit` and `eta.backtest.fold` are as in `incident.sql`.

INSERT INTO loom_fixture.anchor VALUES (1791342000, 1791453600);

-- eta.estimate for org/r01 .. org/r28: every 30 min up to evaluated_at.
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
    map('host.id', 'host-authority')
FROM (SELECT number + 1 AS n FROM numbers(28)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(223)) AS slots;

-- org/r29 item 129: every 30 min until evaluated_at - 20 h (slot 182), then
-- its land outcome 5 min later.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r29', 'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(129)),
    map('loom.eta.primary', true),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(183)) AS slots;

-- org/r29 item 229: every 30 min until evaluated_at - 3 h (slot 216).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r29', 'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(229)),
    map('loom.eta.primary', true),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(217)) AS slots;

-- Row 1: item 129 lands 5 min after its last estimate (evaluated_at - 20 h).
-- Row 2: item 229 landed 10 min BEFORE its last estimate (evaluated_at - 3 h).
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
    (toUInt64(1791453600 - 20 * 3600 + 300) * 1000000000, '{}',
     map('loom.kind', 'eta.outcome', 'loom.repo', 'org/r29', 'loom.eta.kind', 'land',
         'loom.eta.outcome', 'landed'),
     map('loom.issue', 129), map(), map('host.id', 'host-authority')),
    (toUInt64(1791453600 - 3 * 3600 - 600) * 1000000000, '{}',
     map('loom.kind', 'eta.outcome', 'loom.repo', 'org/r29', 'loom.eta.kind', 'land',
         'loom.eta.outcome', 'landed'),
     map('loom.issue', 229), map(), map('host.id', 'host-authority'));

-- org/r30 item 130: estimable every 30 min until evaluated_at - 40 h (slot
-- 142), then one refusal at evaluated_at - 30 h.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 1800) * 1000000000,
    '{"kind":"land"}',
    map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r30', 'loom.eta.kind', 'land'),
    map('loom.issue', toFloat64(130)),
    map('loom.eta.primary', true),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(143)) AS slots;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
VALUES
    (toUInt64(1791453600 - 30 * 3600) * 1000000000, '{"kind":"land"}',
     map('loom.kind', 'eta.estimate', 'loom.repo', 'org/r30', 'loom.eta.kind', 'land',
         'loom.eta.no_estimate_reason', 'blocked'),
     map('loom.issue', 130), map('loom.eta.primary', true), map('host.id', 'host-authority'));

-- eta.fleet_refresh: all 30 repos hourly up to evaluated_at.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 80 * 3600 + k * 3600) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fleet_refresh',
        'loom.repo', concat('org/r', leftPad(toString(n), 2, '0'))),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number + 1 AS n FROM numbers(30)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(112)) AS slots;

-- eta.fit: daily at 03:00, the last one 7 h old.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 72 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.fit'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;

-- eta.backtest.fold: stamped at each day's 00:00 cutoff, the last one 10 h old.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791342000 - 75 * 3600 + k * 86400) * 1000000000,
    '{}',
    map('loom.kind', 'eta.backtest.fold'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number AS k FROM numbers(5)) AS days;

-- Fleet activity (the rule's independent roster, see `roster_gap.sql`): all 30
-- repos leave a `pass.summary` every 5 min. Every one of them has a fresh
-- `eta.fleet_refresh`, so the roster must add no firing series, and it must
-- not make the idle org/r29 or the all-abstaining org/r30 owe estimates.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, body, attributes_string, attributes_number, attributes_bool, resources_string)
SELECT
    toUInt64(1791453600 - 72 * 3600 + k * 300) * 1000000000,
    '{}',
    map('loom.kind', 'pass.summary',
        'loom.repo', concat('org/r', leftPad(toString(n), 2, '0')),
        'loom.pass.mechanism', 'release-stale-blocked'),
    map(), map(),
    map('host.id', 'host-authority')
FROM (SELECT number + 1 AS n FROM numbers(30)) AS repos
CROSS JOIN (SELECT number AS k FROM numbers(864)) AS slots;
