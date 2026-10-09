-- Synthetic `signoz_logs.distributed_logs_v2` rows for the live proof of
-- `replay-queries.sql` queries 6-7 (agreement with the forge, #11128).
-- Executed by `loom-daemon/tests/signoz_replay_queries.rs` and
-- `loom-daemon/tests/telemetry_replay_fixture_store.rs` with
-- t = 2026-10-04 13:00:00, span = 3600, step = 300 (13 instants, 12:00 to
-- 13:00) and window = 3900.
--
-- `fleet.state` bodies use the wire stage names (`telemetry/kinds/fleet_state.rs`).
-- Every host sends an anchor at 11:00 and at 12:00 (so every instant has a
-- base); every record is knowable 10 s after its `as_of`. Webhook rows are
-- the loom-ui export's shape (resource `service.name = loom-ui-d1-export`,
-- the D1 `label.transition` record as the body) and are all inserted at
-- 13:30, after t: the forge state is read at the receipt time `at`, not at
-- the export's insert time.
--
--   h-agree   covered. 1 ready_wait (loom:issue), 2 review_wait PR 200
--             (loom:review-requested), 9 merge_hold PR 900 (loom:pr +
--             loom:operator), 10 sweep.builder (loom:building, after
--             loom:issue was removed). 7 review_wait with no PR (no_pr) and
--             8 ready_wait with no webhook row (no_forge_record) are not
--             compared. Agrees at every instant.
--   h-lag     covered. 3 ready_wait; the forge moves it to loom:building at
--             12:28, the host's delta follows at 12:31. Disagrees at 12:30
--             only: 300 s, under the 600 s threshold.
--   h-stuck   covered, repo rjwalters/other. 5 review_wait PR 500; the forge
--             moves PR 500 to loom:pr at 12:19 and the host never follows.
--             Disagrees 12:20-13:00: 9 instants, 2700 s, over the threshold.
--   h-gap     the same stale row as h-stuck (rjwalters/loom 5, PR 500, same
--             forge move), but every chain is broken (a lost delta after each
--             anchor). Never covered: unknown, never a disagreement.
--   h-chunk   both anchors miss chunk 1 of 2. Holds 6 ready_wait, which the
--             forge closed at 10:30: the rows are never used. Two incomplete
--             anchors, unknown.
--   h-silent  host.health only. unknown (no_anchor).

CREATE DATABASE IF NOT EXISTS signoz_logs;

CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    observed_timestamp UInt64,
    created_at         DateTime64(9),
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, observed_timestamp, created_at, body,
     attributes_string, attributes_number, attributes_bool, resources_string)
SELECT 0, 0, toDateTime64(c, 9), b,
       map('loom.kind', k, 'loom.record_id', id), m, map(), map('host.name', h)
FROM values('h String, id String, k String, c String, m Map(String, Float64), b String',
    -- h-agree
    ('h-agree', 'ag-a1', 'fleet.state', '2026-10-04 11:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":1,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1},{"issue":2,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":200},{"issue":7,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z"},{"issue":8,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":2},{"issue":9,"stage":"merge_hold","entered_at":"2026-10-04T10:01:00Z","pr":900},{"issue":10,"stage":"sweep.builder","entered_at":"2026-10-04T10:00:00Z","host":"h-agree","slot":"regular"}]}]}'),
    ('h-agree', 'ag-a2', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":1,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1},{"issue":2,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":200},{"issue":7,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z"},{"issue":8,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":2},{"issue":9,"stage":"merge_hold","entered_at":"2026-10-04T10:01:00Z","pr":900},{"issue":10,"stage":"sweep.builder","entered_at":"2026-10-04T10:00:00Z","host":"h-agree","slot":"regular"}]}]}'),
    -- h-lag
    ('h-lag', 'lg-a1', 'fleet.state', '2026-10-04 11:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":3,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1}]}]}'),
    ('h-lag', 'lg-a2', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":3,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1}]}]}'),
    ('h-lag', 'lg-d1', 'fleet.state', '2026-10-04 12:31:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:31:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":3,"stage":"sweep.builder","entered_at":"2026-10-04T12:29:00Z","host":"h-lag","slot":"regular"}]}]}'),
    -- h-stuck
    ('h-stuck', 'st-a1', 'fleet.state', '2026-10-04 11:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:00:00Z","repos":[{"repo":"rjwalters/other","rows":[{"issue":5,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":500}]}]}'),
    ('h-stuck', 'st-a2', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/other","rows":[{"issue":5,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":500}]}]}'),
    -- h-gap (the 11:05 and 11:59 deltas are lost)
    ('h-gap', 'gp-a1', 'fleet.state', '2026-10-04 11:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":5,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":500}]}]}'),
    ('h-gap', 'gp-d1', 'fleet.state', '2026-10-04 11:10:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:10:00Z","anchor":false,"anchor_as_of":"2026-10-04T11:00:00Z","prev_as_of":"2026-10-04T11:05:00Z","repos":[{"repo":"rjwalters/loom","rows":[]}]}'),
    ('h-gap', 'gp-a2', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":5,"stage":"review_wait","entered_at":"2026-10-04T10:00:00Z","pr":500}]}]}'),
    ('h-gap', 'gp-d2', 'fleet.state', '2026-10-04 12:01:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:01:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T11:59:00Z","repos":[{"repo":"rjwalters/loom","rows":[]}]}'),
    -- h-chunk (chunk 1 of each anchor never arrived)
    ('h-chunk', 'ck-a1', 'fleet.state', '2026-10-04 11:00:10',
     map('loom.fleet.chunk_index', 0, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":6,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1}]}]}'),
    ('h-chunk', 'ck-a2', 'fleet.state', '2026-10-04 12:00:10',
     map('loom.fleet.chunk_index', 0, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":6,"stage":"ready_wait","entered_at":"2026-10-04T10:00:00Z","rank":1}]}]}'),
    -- h-silent
    ('h-silent', 'sl-h', 'host.health', '2026-10-04 12:30:00', map(), '{}')
);

-- The webhook export: label.transition records as loom-ui exports them.
INSERT INTO signoz_logs.distributed_logs_v2
    (timestamp, observed_timestamp, created_at, body,
     attributes_string, attributes_number, attributes_bool, resources_string)
SELECT 0, 0, toDateTime64('2026-10-04 13:30:00', 9),
       concat('{"kind":"label.transition","repo":"', repo, '","target":"', target,
              '","number":', toString(number), ',"action":"', action, '"',
              if(label = '', '', concat(',"label":"', label, '"')),
              ',"at":"', at, '","labels_after":[]}'),
       map('loom.export.id', concat('records:', toString(rowNumberInAllBlocks()))), map(), map(),
       map('service.name', 'loom-ui-d1-export')
FROM values('repo String, target String, number UInt32, action String, label String, at String',
    -- h-agree's items
    ('rjwalters/loom', 'issue', 1,   'labeled',   'loom:issue',             '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'pr',    200, 'opened',    '',                       '2026-10-04T09:59:00.000Z'),
    ('rjwalters/loom', 'pr',    200, 'labeled',   'loom:review-requested',  '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'pr',    900, 'labeled',   'loom:pr',                '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'pr',    900, 'labeled',   'loom:operator',          '2026-10-04T10:01:00.000Z'),
    ('rjwalters/loom', 'issue', 10,  'labeled',   'loom:issue',             '2026-10-04T09:00:00.000Z'),
    ('rjwalters/loom', 'issue', 10,  'unlabeled', 'loom:issue',             '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'issue', 10,  'labeled',   'loom:building',          '2026-10-04T10:00:00.000Z'),
    -- h-lag's item: the forge moves first (12:28), the host follows at 12:31
    ('rjwalters/loom', 'issue', 3,   'labeled',   'loom:issue',             '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'issue', 3,   'unlabeled', 'loom:issue',             '2026-10-04T12:28:00.000Z'),
    ('rjwalters/loom', 'issue', 3,   'labeled',   'loom:building',          '2026-10-04T12:28:00.000Z'),
    -- PR 500, approved at 12:19, in both repos (h-stuck covers other, h-gap loom)
    ('rjwalters/other', 'pr',   500, 'labeled',   'loom:review-requested',  '2026-10-04T10:00:00.000Z'),
    ('rjwalters/other', 'pr',   500, 'unlabeled', 'loom:review-requested',  '2026-10-04T12:19:00.000Z'),
    ('rjwalters/other', 'pr',   500, 'labeled',   'loom:pr',                '2026-10-04T12:19:00.000Z'),
    ('rjwalters/loom', 'pr',    500, 'labeled',   'loom:review-requested',  '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'pr',    500, 'unlabeled', 'loom:review-requested',  '2026-10-04T12:19:00.000Z'),
    ('rjwalters/loom', 'pr',    500, 'labeled',   'loom:pr',                '2026-10-04T12:19:00.000Z'),
    -- h-chunk's item, closed long before
    ('rjwalters/loom', 'issue', 6,   'labeled',   'loom:issue',             '2026-10-04T10:00:00.000Z'),
    ('rjwalters/loom', 'issue', 6,   'closed',    '',                       '2026-10-04T10:30:00.000Z')
);
