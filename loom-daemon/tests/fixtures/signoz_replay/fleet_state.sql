-- Synthetic `signoz_logs.distributed_logs_v2` rows for the live proof of
-- `replay-queries.sql` queries 1-3 (#11125). Executed by
-- `loom-daemon/tests/signoz_replay_queries.rs` with t = 2026-10-04 13:00:00
-- and window = 3900 s (so the window opens at 11:55:00).
--
-- Bodies follow `fleet-state/v1` from #10283 (`telemetry/kinds/fleet_state.rs`):
-- `anchor`, `as_of`, `anchor_as_of`, `prev_as_of` (deltas only) and `repos`
-- with `rows` / `removed`. Chunk attributes `loom.fleet.chunk_index` /
-- `loom.fleet.chunk_count` are the provisional names the SQL documents. Every
-- record is knowable 10 s after its `as_of` unless noted. One emitting host per
-- scenario, so each host's `status.state` is asserted in isolation:
--
--   h-readd       upsert 1,3 @12:00 -> remove 1,3 @12:05 -> re-add 1,3 @12:10
--                 -> remove 1 @12:15 (redelivered under a second record id).
--                 Expect 1 GONE (the any-earlier-removal predicate kept it),
--                 3 live as sweep_doctor. Also holds issue 60 WITH a host.
--   h-rmtwice     upsert 2,98 @12:00 (anchor delivered twice, same record id)
--                 -> remove 2 @12:05 -> remove 2 again + 98 to sweep_judge
--                 @12:10. Expect 2 gone, 98 live as sweep_judge.
--   h-gap         anchor 7 @12:00; delta @12:05 removing 7 LOST; delta @12:10
--                 (prev_as_of 12:05) upserts 8. Expect broken_chain: neither
--                 7 nor 8, not covered.
--   h-chunk       anchor @12:00 in 2 chunks, only chunk 0 (issue 20) arrived.
--                 Expect incomplete_anchor.
--   h-chunked-ok  anchor @12:00 in 2 chunks, both arrived (30 + 60 without a
--                 host; 31), delta @12:05 upserts 32. Expect complete; 30, 31,
--                 32 live, and 60 merged with h-readd's row (host wins).
--   h-dchunk      anchor 70 @12:00; delta @12:05 removing 70 in 2 chunks, only
--                 chunk 0 arrived. Expect incomplete_delta.
--   h-lostanchor  complete anchor 40 @11:57; delta @12:58 names anchor 12:57,
--                 which never arrived. Expect missing_anchor: the older anchor
--                 is not used past the newer one.
--   h-late        anchor 50 @12:00; delta @12:55 removing 50 is knowable only
--                 at 13:00:05 (after t). Expect complete, 50 still live.
--   h-silent      host.health only. Expect no_anchor.

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
    -- h-readd
    ('h-readd', 'ra-a', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":1,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"},{"issue":3,"stage":"sweep_judge","entered_at":"2026-10-04T11:00:00Z"},{"issue":60,"stage":"review_wait","entered_at":"2026-10-04T11:00:00Z","pr":600,"host":"h-readd","slot":"regular"}]}]}'),
    ('h-readd', 'ra-d1', 'fleet.state', '2026-10-04 12:05:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:05:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","removed":[1,3]}]}'),
    ('h-readd', 'ra-d2', 'fleet.state', '2026-10-04 12:10:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:10:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:05:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":1,"stage":"sweep_doctor","entered_at":"2026-10-04T12:10:00Z"},{"issue":3,"stage":"sweep_doctor","entered_at":"2026-10-04T12:10:00Z"}]}]}'),
    ('h-readd', 'ra-d3', 'fleet.state', '2026-10-04 12:15:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:15:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:10:00Z","repos":[{"repo":"rjwalters/loom","removed":[1]}]}'),
    ('h-readd', 'ra-d3-retry', 'fleet.state', '2026-10-04 12:16:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:15:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:10:00Z","repos":[{"repo":"rjwalters/loom","removed":[1]}]}'),
    -- h-rmtwice
    ('h-rmtwice', 'rt-a', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":2,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"},{"issue":98,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-rmtwice', 'rt-a', 'fleet.state', '2026-10-04 12:00:40', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":2,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"},{"issue":98,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-rmtwice', 'rt-d1', 'fleet.state', '2026-10-04 12:05:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:05:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","removed":[2]}]}'),
    ('h-rmtwice', 'rt-d2', 'fleet.state', '2026-10-04 12:10:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:10:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:05:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":98,"stage":"sweep_judge","entered_at":"2026-10-04T12:10:00Z"}],"removed":[2]}]}'),
    -- h-gap (the 12:05 delta removing 7 is lost)
    ('h-gap', 'gp-a', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":7,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-gap', 'gp-c', 'fleet.state', '2026-10-04 12:10:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:10:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:05:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":8,"stage":"sweep_builder","entered_at":"2026-10-04T12:10:00Z"}]}]}'),
    -- h-chunk (chunk 1 of the anchor never arrived)
    ('h-chunk', 'ck-a0', 'fleet.state', '2026-10-04 12:00:10',
     map('loom.fleet.chunk_index', 0, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":20,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    -- h-chunked-ok
    ('h-chunked-ok', 'co-a0', 'fleet.state', '2026-10-04 12:00:10',
     map('loom.fleet.chunk_index', 0, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":30,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"},{"issue":60,"stage":"merge_wait","entered_at":"2026-10-04T11:30:00Z","pr":600}]}]}'),
    ('h-chunked-ok', 'co-a1', 'fleet.state', '2026-10-04 12:00:12',
     map('loom.fleet.chunk_index', 1, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":31,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-chunked-ok', 'co-d1', 'fleet.state', '2026-10-04 12:05:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:05:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":32,"stage":"sweep_builder","entered_at":"2026-10-04T12:05:00Z"}]}]}'),
    -- h-dchunk (chunk 1 of the delta never arrived)
    ('h-dchunk', 'dc-a', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":70,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-dchunk', 'dc-d0', 'fleet.state', '2026-10-04 12:05:10',
     map('loom.fleet.chunk_index', 0, 'loom.fleet.chunk_count', 2),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:05:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","removed":[70]}]}'),
    -- h-lostanchor (the 12:57 anchor never arrived)
    ('h-lostanchor', 'la-a1', 'fleet.state', '2026-10-04 11:57:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T11:57:00Z","anchor":true,"anchor_as_of":"2026-10-04T11:57:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":40,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-lostanchor', 'la-d', 'fleet.state', '2026-10-04 12:58:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:58:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:57:00Z","prev_as_of":"2026-10-04T12:57:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":41,"stage":"sweep_builder","entered_at":"2026-10-04T12:58:00Z"}]}]}'),
    -- h-late (the removal is not knowable before t)
    ('h-late', 'lt-a', 'fleet.state', '2026-10-04 12:00:10', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:00:00Z","anchor":true,"anchor_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","rows":[{"issue":50,"stage":"sweep_builder","entered_at":"2026-10-04T11:00:00Z"}]}]}'),
    ('h-late', 'lt-d', 'fleet.state', '2026-10-04 13:00:05', map(),
     '{"schema":"fleet-state/v1","as_of":"2026-10-04T12:55:00Z","anchor":false,"anchor_as_of":"2026-10-04T12:00:00Z","prev_as_of":"2026-10-04T12:00:00Z","repos":[{"repo":"rjwalters/loom","removed":[50]}]}'),
    -- h-silent
    ('h-silent', 'sl-h', 'host.health', '2026-10-04 12:30:00', map(), '{}')
);
