# Query API + live tail (Epic #4702, Phase 2 — issues #4726 + #4727)

The read side of the Phase-2 Workers backend: the query API and live event
tail the Phase-3 dashboard UI consumes. Builds on the D1 history store +
`FleetState` Durable Object issue #4725 introduced (`src/index.ts` /
`src/fleetState.ts`) — these issues add read paths only, no new storage.

## Authenticated vs. public: two route surfaces, one redaction policy

Every route below exists **twice** — once under `/api/*` and once under
`/public/*` — returning the same underlying data through two different
policies:

| Prefix | Who reaches it | Visibility-tagged (`private`) data |
|---|---|---|
| `/api/*` | **Authenticated** — the surface an operator's Cloudflare Access policy is expected to gate (see [`cloudflare-access.md`](cloudflare-access.md)'s route map: everything not explicitly Bypassed is Allow-gated) | Full detail, unredacted |
| `/public/*` | **Public** — always reachable, no login | Redacted per record kind (see below); `public`-visibility data is always full detail on either prefix |

**This is a route-based split, not an in-Worker one.** Per the epic's
explicit constraint ("no auth code in the dashboard itself"), the Worker
never parses a JWT or any other credential — `isAuthenticated` in
`src/index.ts` is set purely by which path matched. The `/public` prefix is
the same path `cloudflare-access.md` already reserves as a Bypass
application (§2 of that guide, added ahead of the Phase-3 public page); the
existing `/api/*` prefix is left as the "everything else" Allow-gated
surface that guide already documents. Putting an operator's Access policy in
front of `/api/*` (and leaving `/public/*` bypassed) is what actually makes
the split enforceable end to end — **the Worker's own redaction is a
defense-in-depth control, not a substitute for that edge configuration.**

**Redaction is one policy layer, one enforcement point.**
[`src/redaction.ts`](../src/redaction.ts) wraps every `/api/*` and
`/public/*` handler as a post-processing step over `src/query.ts`'s results
— `query.ts` itself is unmodified and still returns full detail for every
kind (its own "unclassified surface" module doc is unchanged and accurate).
The redaction policy is a **per-kind field allowlist** (not a blocklist): a
private, unauthenticated response for a given `kind` includes only the
fields that module's table explicitly lists as safe (lifecycle/timing/
model/rate fields) — every other field, known or not-yet-invented, is
dropped by default. See that module's doc comment for the full policy,
including the explicit decision on `tokens.snapshot`/`host.health` (host-level
kinds with no `repo` reference — passed through unredacted on both
surfaces).

Implementation: [`src/query.ts`](../src/query.ts) (filter parsing, the D1
query, and the live-tail stream — unclassified), [`src/redaction.ts`](../src/redaction.ts)
(the policy layer) plus the route handlers in [`src/index.ts`](../src/index.ts).
Tests: [`test/query.test.ts`](../test/query.test.ts) (data access),
[`test/redaction.test.ts`](../test/redaction.test.ts) (the adversarial
redaction suite — every record kind × visibility × auth combination).

## `GET /api/fleet-state` / `GET /public/fleet-state`

Current state of every host/sweep known to the `FleetState` Durable Object —
the query-API equivalent of the operator-only `GET /admin/fleet-state` (same
underlying snapshot, no `ADMIN_TOKEN` required). `/public/fleet-state`
redacts each `activeSweeps` entry per the visibility policy above; `/api/fleet-state`
always returns full detail.

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

On `GET /public/fleet-state`, a `visibility: "private"` entry in
`activeSweeps` has `repo`/`issue`/`sweepId` omitted entirely (not
null-valued — `JSON.stringify` drops the key) rather than the shape above;
`phase`/timing/`model`/`effort`/`hostId` survive unchanged. A
`visibility: "public"` entry is identical on both routes.

## `GET /api/history` / `GET /public/history`

Filterable, paginated query over the D1 `records` table — one row per
ingested telemetry record (see `migrations/0001_init.sql`). `/public/history`
applies the same filter/pagination contract below; only the shape of each
*returned record* differs (see "Redaction" below the response shape).

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
  just the columns this backend indexes. **On `/api/history`** this is
  always true, for every record. **On `/public/history`**, this is true only
  for `visibility: "public"` records; a `visibility: "private"` record has
  `repo`/`issue`/`sweepId` nulled at the top level and `record` reduced to a
  per-`kind` field allowlist (see `src/redaction.ts`) — e.g. a private
  `sweep.outcome` keeps `model`/`effort`/`config`/`phase_durations`/
  `total_duration_sec`/`result` but never `repo`/`issue`/`sweep_id`/
  `pr_number`. `tokens.snapshot`/`host.health` records (host-level, no
  `repo` reference) are never redacted on either route.

## `GET /api/events` / `GET /public/events`

Server-Sent Events (`text/event-stream`) live tail of newly-ingested
telemetry — delivers only records ingested **after** the connection opens;
replaying prior history is `GET /api/history`'s job. `/public/events`
applies the same per-`kind` field allowlist `/public/history` uses (above),
per frame, as each record is ingested.

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
(`hostId`) plus the envelope fields, verbatim (`/api/events`) or redacted
per `event.record.visibility` (`/public/events`). The same `sweep.phase`
record above, `visibility: "private"`, arrives on `/public/events` as:

```
data: {"topic":"sweep.phase","event":{"hostId":"host-abc","emittedAt":"2026-07-30T12:03:20Z","schemaVersion":1,"record":{"kind":"sweep.phase","phase":"builder","entered_at":"2026-07-30T12:03:20Z"}}}

```

— `repo`/`visibility`/`issue`/`sweep_id` dropped from `event.record`, every
other field (`hostId`, `topic`/`kind`, timing) unchanged.

A connection also receives:

- A `retry: 3000` directive plus a leading `: connected to loom fleet
  telemetry live tail` comment immediately on connect (mirrors the daemon
  bridge's reconnect-delay convention).
- A `: keepalive` comment roughly every 15s when no new records have arrived,
  so intermediaries never reap an idle connection.

Neither the preamble nor the keepalive comment ever carries record data, so
`/public/events` passes both through unchanged — only `data:` frames are
inspected for redaction.

### Delivery model

Implemented as a short poll loop over D1 (default cadence ~1s) scoped to the
stream's own lifetime, not a Durable-Object-side socket registry — see
`src/query.ts`'s `createLiveTailStream` doc comment for why. The stream
closes when the client disconnects (`Request.signal` aborts) or the consumer
cancels its reader. `/public/events` pipes the same stream through a
`TransformStream` that redacts each `data:` frame in place before it reaches
the client (`src/redaction.ts`'s `redactLiveTailStream`) — the underlying
poll loop and D1 query are identical to `/api/events`.

## Not implemented here (later issues)

- **`model`/`result` server-side filtering on the live tail** — `/api/events`
  and `/public/events` only support `host`/`repo` filters today; a client
  wanting to filter by model/result filters client-side on the streamed
  frames.
- **In-Worker Cloudflare Access JWT verification** — the `/api/*` vs
  `/public/*` split (above) relies entirely on the Cloudflare Access edge
  policy an operator configures per [`cloudflare-access.md`](cloudflare-access.md);
  the Worker itself does not verify a signed Access header. See that guide's
  §5 for the tradeoff and what a future hardening pass would add.
