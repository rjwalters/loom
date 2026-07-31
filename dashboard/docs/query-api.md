# Query API + live tail (Epic #4702, Phase 2 — issue #4726)

The read side of the Phase-2 Workers backend: the query API and live event
tail the Phase-3 dashboard UI consumes. Builds on the D1 history store +
`FleetState` Durable Object issue #4725 introduced (`src/index.ts` /
`src/fleetState.ts`) — this issue adds read paths only, no new storage.

**Unclassified surface.** Every route below returns full record detail
regardless of the stored `visibility` tag, and requires no authentication.
Visibility-based redaction (a public view vs. an authenticated view with full
detail) is issue #4727's job, as a wrapper in front of these routes — not
implemented here. Do not point an untrusted public client at these routes
directly until #4727 lands.

Implementation: [`src/query.ts`](../src/query.ts) (filter parsing, the D1
query, and the live-tail stream) plus the route handlers in
[`src/index.ts`](../src/index.ts). Tests:
[`test/query.test.ts`](../test/query.test.ts).

## `GET /api/fleet-state`

Current state of every host/sweep known to the `FleetState` Durable Object —
the unclassified equivalent of the operator-only `GET /admin/fleet-state`
(same underlying snapshot, no `ADMIN_TOKEN` required).

**Response** (`200`, camelCase — mirrors the Durable Object's own JSON, see
`src/fleetState.ts`'s `FleetSnapshot`):

```json
{
  "hosts": {
    "host-abc": {
      "health": { "record": { "kind": "host.health", "...": "..." }, "updatedAt": "2026-07-30T12:00:00Z" },
      "tokens": { "record": { "kind": "tokens.snapshot", "...": "..." }, "updatedAt": "2026-07-30T12:00:00Z" }
    }
  },
  "activeSweeps": [
    {
      "hostId": "host-abc",
      "sweepId": "sweep-issue-4703-0",
      "repo": "rjwalters/loom",
      "visibility": "public",
      "issue": 4703,
      "phase": "builder",
      "startedAt": "2026-07-30T12:00:00Z",
      "enteredPhaseAt": "2026-07-30T12:03:20Z",
      "model": "opus",
      "effort": "high",
      "updatedAt": "2026-07-30T12:03:20Z"
    }
  ]
}
```

A completed sweep is not present in `activeSweeps` (removed on
`sweep.completed` — see `src/fleetState.ts`'s module doc); its full record
lives in D1 and is queryable via `GET /api/history`.

## `GET /api/history`

Filterable, paginated query over the D1 `records` table — one row per
ingested telemetry record (see `migrations/0001_init.sql`).

### Query parameters (all optional)

| Param | Type | Matches |
|---|---|---|
| `host` | string | `records.host_id` (exact match) |
| `repo` | string | `records.repo` (exact match) |
| `model` | string | `record.model`, extracted from the JSON payload (present on `sweep.started`/`sweep.outcome`) |
| `result` | string | `record.result`, extracted from the JSON payload (present on `sweep.completed`/`sweep.outcome`; one of `success`/`failure`/`cancelled`/`blocked`) |
| `since` | RFC 3339 datetime | `emitted_at >= since` (inclusive) |
| `until` | RFC 3339 datetime | `emitted_at < until` (exclusive) |
| `limit` | positive integer | Page size. Default `50`, capped at `500`. |
| `cursor` | positive integer | Keyset pagination cursor — pass the previous page's `nextCursor`. |

An invalid `since`/`until` (unparseable datetime), `limit` (non-positive or
non-integer), or `cursor` (non-positive or non-integer) returns `400` with a
`{"error": "..."}` body naming the first invalid param.

### Response (`200`)

```json
{
  "records": [
    {
      "id": 42,
      "schemaVersion": 1,
      "emittedAt": "2026-07-30T12:00:00Z",
      "hostId": "host-abc",
      "kind": "sweep.outcome",
      "repo": "rjwalters/loom",
      "visibility": "public",
      "issue": 4703,
      "sweepId": "sweep-issue-4703-0",
      "ingestedAt": "2026-07-30T12:00:01Z",
      "record": { "kind": "sweep.outcome", "model": "opus", "result": "success", "...": "..." }
    }
  ],
  "nextCursor": 41
}
```

- **Ordering**: always newest-first, by `id` descending.
- **Pagination**: `nextCursor` is the `id` of the last record on this page,
  or `null` when this page reached the end of the matching result set. Pass
  it back as `?cursor=` to fetch the next page. This is keyset pagination
  (`WHERE id < cursor`) — O(1) per page, and stable under concurrent inserts
  (a new row never shifts an already-issued cursor's meaning), unlike
  `OFFSET`-based paging.
- `record` is the full, verbatim JSON payload that was ingested (the same
  object the wire envelope's `record` field carried) — so any field the
  schema doc documents (`.loom/docs/telemetry-schema.md`) is available, not
  just the columns this backend indexes.

## `GET /api/events`

Server-Sent Events (`text/event-stream`) live tail of newly-ingested
telemetry — delivers only records ingested **after** the connection opens;
replaying prior history is `GET /api/history`'s job.

### Query parameters (optional)

| Param | Matches |
|---|---|
| `host` | Only stream records from this `host_id`. |
| `repo` | Only stream records for this `repo`. |

### Frame shape

Every event arrives as a default (`message`-typed, no `event:` field) SSE
frame:

```
data: {"topic":"sweep.phase","event":{"hostId":"host-abc","emittedAt":"2026-07-30T12:03:20Z","schemaVersion":1,"record":{"kind":"sweep.phase","repo":"rjwalters/loom","visibility":"public","issue":4703,"sweep_id":"sweep-issue-4703-0","phase":"builder","entered_at":"2026-07-30T12:03:20Z"}}}

```

This deliberately mirrors the shape `loom-daemon`'s own frozen `sweep.*` SSE
bridge emits (`loom-daemon/src/serve.rs`'s `sse_frame` —
`data: {"topic": ..., "event": {...}}\n\n`, no `event:` field since the topic
is parameterized by issue number and a browser cannot `addEventListener` for
a dynamic topic): `topic` is the record's own `kind` — already exactly
`sweep.started`/`sweep.phase`/`sweep.completed` for the three that overlap
the frozen per-issue taxonomy — and `event` carries the multi-host extension
(`hostId`) plus the envelope fields, verbatim.

A connection also receives:

- A `retry: 3000` directive plus a leading `: connected to loom fleet
  telemetry live tail` comment immediately on connect (mirrors the daemon
  bridge's reconnect-delay convention).
- A `: keepalive` comment roughly every 15s when no new records have arrived,
  so intermediaries never reap an idle connection.

### Delivery model

Implemented as a short poll loop over D1 (default cadence ~1s) scoped to the
stream's own lifetime, not a Durable-Object-side socket registry — see
`src/query.ts`'s `createLiveTailStream` doc comment for why. The stream
closes when the client disconnects (`Request.signal` aborts) or the consumer
cancels its reader.

## Not implemented here (later issues)

- **Visibility-based redaction** (#4727) — this API always returns full
  detail; #4727 wraps it with a public view that redacts/aggregates private
  records instead of omitting the field entirely.
- **`model`/`result` server-side filtering on the live tail** — `/api/events`
  only supports `host`/`repo` filters today; a client wanting to filter by
  model/result filters client-side on the streamed frames.
- **Cloudflare Access / any authentication** on the `/api/*` routes — see the
  "Unclassified surface" note above.
