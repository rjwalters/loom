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
| `/api/*` | **Authenticated** — the surface an operator's Cloudflare Access policy is expected to gate (see [`cloudflare-access.md`](cloudflare-access.md) §3e: `/api/*` gets its own Allow application) | Full detail, unredacted |
| `/public/*` | **Public** — always reachable, no login | Redacted per record kind (see below); `public`-visibility data is always full detail on either prefix |

**For these two prefixes this is a route-based split, not an in-Worker
one.** The Worker does not parse a JWT or any other credential on `/api/*`
or `/public/*` — `isAuthenticated` in `src/index.ts` is set purely by which
path matched. Putting an operator's Access policy in front of `/api/*` (and
leaving `/public/*` ungated) is what actually makes the split enforceable
end to end — **the Worker's own redaction is a defense-in-depth control, not
a substitute for that edge configuration.** In particular, since issue #4795
narrowed the old hostname-wide Access application down to `/login`, `/api/*`
needs its **own** Access application or it is matched by none at all: see
[`cloudflare-access.md`](cloudflare-access.md) §3e, and the pitfalls table
row "The `/api/*` query API is reachable without logging in".

**The dashboard root `/` is the one exception** (issue #4795): it verifies
the visitor's Access JWT in-Worker ([`src/accessAuth.ts`](../src/accessAuth.ts))
and picks the redacted or unredacted variant of the same page accordingly,
so a single URL can serve both audiences without dead-ending an anonymous
visitor at an SSO wall. That check is fail-closed by construction — a
missing/malformed/expired/wrong-`aud` token, or an unreachable JWKS
endpoint, all render the public variant. It does **not** change anything
about the `/api/*` vs. `/public/*` contract described here; the page's own
live feed simply points at whichever prefix matches the variant it rendered.

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
including the explicit decisions on the two host-level kinds (no `repo`
reference on either): `host.health` passes through unredacted on both
surfaces, while `tokens.snapshot`'s per-account rows are replaced by a
non-identifying aggregate for public viewers.

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
redacts each `activeSweeps` entry per the visibility policy above and empties
`activeCompute` entirely; `/api/fleet-state` always returns full detail.

**Response** (`200`, camelCase — mirrors the Durable Object's own JSON, see
`src/fleetState.ts`'s `FleetSnapshot`):

```json
{
  "hosts": {
    "host-abc": {
      "health": {
        "record": { "kind": "host.health", "...": "..." },
        "updatedAt": "2026-07-30T12:00:00Z",
        "freshness": { "status": "live", "ageSeconds": 42 }
      },
      "tokens": {
        "record": { "kind": "tokens.snapshot", "...": "..." },
        "updatedAt": "2026-07-30T12:00:00Z",
        "freshness": { "status": "live", "ageSeconds": 42 }
      }
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
      "runtime": "claude",
      "updatedAt": "2026-07-30T12:03:20Z"
    }
  ]
}
```

`runtime` is the adapter the sweep was dispatched on (`claude`, `codex`, …),
copied from `sweep.started` or late `sweep.identity` enrichment; absent when the emitting daemon did not name one.
`provider` names the resolved launch provider separately from the runtime adapter
(e.g. `zai-coding-plan` versus `opencode`); `model` is the resolved launch model.
These describe the sweep launch, not all child roles. Missing values remain
unknown. Identity enrichment preserves start/phase/freshness and applies only to
an existing same-host active entry, never recreating a completed sweep. Deploy
this Worker/UI before schema-4 identity producers; older daemons remain readable.

**`freshness`** (issue #4957): derived from `updatedAt` alone (never the
daemon-supplied `captured_at`, which a clock-skewed host could spoof) —
`status` is one of `"live"` (reported within the last ~15 minutes, roughly
2x the daemon's `host.health`/`tokens.snapshot` sampling cadence),
`"stale"` (up to ~4 hours), or `"offline"` (beyond that — the daemon has
very likely stopped, the host is asleep, or it lost its tailnet).
`ageSeconds` is seconds since `updatedAt`. An entry older than 7 days is
pruned from the Durable Object entirely on the next snapshot build rather
than ever appearing here — see `src/fleetState.ts`'s `classifyFreshness`/
`PRUNE_AFTER_MS`. Both the SSR `/` fallback page (`src/publicPage.ts`) and
any consumer of this route should treat a `stale`/`offline` sample's
numbers as historical, never as current.

A completed sweep is not present in `activeSweeps` (removed on
`sweep.completed` — see `src/fleetState.ts`'s module doc); its full record
lives in D1 and is queryable via `GET /api/history`.

**`activeCompute`** (issue #8305) is the same idea for `ephemeral_compute`:
one entry per currently-running cloud compute job, created by the launch
record and deleted by the completion record, so presence *is* the definition
of "running". Each entry carries `hostId`, `jobId`, and whatever the launch
record described (`instanceId`/`region`/`instanceType`/`spot`/`ami`/
`startedAt`), plus `updatedAt` (the backend's own ingest clock) and a derived
`leaked` boolean:

```json
"activeCompute": [
  {
    "hostId": "2am-elastic",
    "jobId": "job-abc123",
    "instanceId": "i-0123456789abcdef0",
    "region": "us-east-1",
    "instanceType": "c7i.4xlarge",
    "spot": true,
    "startedAt": "2026-09-19T12:00:00Z",
    "updatedAt": "2026-09-19T12:00:00Z",
    "leaked": false
  }
]
```

`leaked` is `true` once an entry has gone 24 hours with no completion record
— the instance is very likely still running and billing with nothing watching
it. It is computed from `updatedAt`, never the emitter-supplied `startedAt`,
so a skewed emitter clock cannot fake liveness. Cost and wall clock are
deliberately absent: both exist only on the completion record, which removes
the entry — a finished job's cost is read from `GET /api/spend`.

On `GET /public/fleet-state` this is **always `[]`**, not a reduced entry: no
`ephemeral_compute` field survives the allowlist, so even the count of
running instances is withheld (it is itself infrastructure-spend detail about
a private compute fleet).

**Consumer**: the Phase-3 dashboard UI ([`../web/`](../web/)) reads this route
and only this route. Its client (`web/src/api.ts`) plus the narrowing layer
(`web/src/parse.ts`) are a worked example of the tolerances this contract
requires — every `host.health` measurement and most `activeSweeps` fields are
optional, and an absent measurement must be rendered as unknown rather than
zero.

On `GET /public/fleet-state`, a `visibility: "private"` entry in
`activeSweeps` has `repo`/`issue`/`sweepId` omitted entirely (not
null-valued — `JSON.stringify` drops the key) rather than the shape above;
`phase`/timing/`model`/`effort`/`runtime`/`hostId` survive unchanged. A
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
| `kind` | string | `records.kind` (exact match; open vocabulary — e.g. `sweep.completed`, `host.health` — see `.loom/docs/telemetry-schema.md`) |
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
  `pr_number` — nor (Issue #5357) its `tokens_in`/`tokens_out`/`lines_added`/
  `lines_deleted` work-output fields, held back for the same "workload
  detail about a private repo" reason as `pr_number`. `host.health` records
  (host-level, no `repo` reference) are never redacted on either route.

  `tokens.snapshot` is the one kind whose *shape* differs by route. `/api/*`
  returns the per-account rows as ingested (`accounts[]`, each with
  `account`/`provider`/`rank`/`usage_fraction`/`limit_window_reset_at`/
  `exhausted`). `/public/*` drops `accounts` entirely and returns a derived
  aggregate in its place:

  ```json
  {
    "kind": "tokens.snapshot",
    "captured_at": "2026-07-30T12:00:00Z",
    "account_count": 13,
    "exhausted_count": 5,
    "mean_usage_fraction": 0.3246,
    "max_usage_fraction": 0.91,
    "next_limit_window_reset_at": "2026-07-30T18:00:00Z",
    "providers": [
      {
        "provider": "claude",
        "account_count": 10,
        "exhausted_count": 5,
        "max_usage_fraction": 0.91,
        "next_limit_window_reset_at": "2026-07-30T18:00:00Z"
      },
      {
        "provider": "codex",
        "account_count": 3,
        "exhausted_count": 0,
        "max_usage_fraction": null,
        "next_limit_window_reset_at": null
      }
    ]
  }
  ```

  `providers` is the same summary sliced per provider pool (`claude`, `codex`,
  …), in first-seen order — a provider name is the daemon's own
  `AccountProvider` vocabulary, not an account identifier. A row with no
  `provider` (a daemon that predates per-provider pools) counts as `claude`.

  How loaded the pool is, and when capacity returns, without naming an
  account or exposing any single account's burn. The two usage figures are
  `null` — never `0` — when no account reported a `usage_fraction`.
  `next_limit_window_reset_at` is the **earliest** per-account
  `limit_window_reset_at` in the pool, and is likewise `null` — never a
  fabricated instant — when no account reported one. Each account's
  `limit_window_reset_at` is the reset of whichever window is gating *that*
  account (7d once exhausted, 5h otherwise; the daemon resolves it before
  export), so the aggregate reads as "the first moment any account frees up".

## `GET /api/spend` / `GET /public/spend`

Aggregate `ephemeral_compute` spend over a time window, bucketed by UTC day
(issue #8306, Phase 3 of #8257). Unlike `/api/history` this returns a
*summary*, not rows: the underlying `SUM` runs in D1 rather than shipping
every completion record to the browser to add up.

### Query parameters (all optional)

| Param | Meaning |
|---|---|
| `since` | Inclusive lower bound on `emitted_at` (RFC 3339). |
| `until` | Exclusive upper bound on `emitted_at` (RFC 3339). |
| `host` | Only the named emitting host (for an elastic fleet, the synthetic ingest identity — see [`../../defaults/docs/observability.md`](../../defaults/docs/observability.md) §5d). |

An invalid `since`/`until` is a `400` with `{ "error": ... }`, exactly as on
`/api/history`.

### Response (`200`, authenticated)

```json
{
  "since": "2026-09-12T00:00:00Z",
  "until": "2026-09-19T00:00:00Z",
  "totalCostUsd": 137.5,
  "jobCount": 9,
  "totalWallClockSec": 41400,
  "peakDailyCostUsd": 104.25,
  "days": [{ "day": "2026-09-18", "costUsd": 104.25, "jobCount": 6 }]
}
```

Three contracts worth knowing:

- **Which rows count.** Exactly those whose payload carries a *numeric*
  `estimated_cost_usd`. A job emits two records — a launch record (no cost
  yet) and a completion record — so this predicate is also what makes each
  job count once. Jobs still *running* are not here at all; they live in the
  Durable Object and surface on `GET /api/fleet-state`'s `activeCompute`.
- **Unknown is not zero.** `totalWallClockSec` is `null` — never `0` — when
  no job in the window reported one, and `peakDailyCostUsd` is `null` when
  the window had no spend at all. A window with genuinely no completed jobs
  returns `totalCostUsd: 0` with `days: []`, which is a real answer, not an
  error.
- **`peakDailyCostUsd` exists because a window total cannot answer "did any
  one day breach the standing daily ceiling".** An average hides exactly the
  day that did.

### Response (`200`, public)

```json
{ "since": "2026-09-12T00:00:00Z", "until": null, "withheld": true }
```

**No field of `ephemeral_compute` survives redaction** (see
[`src/redaction.ts`](../src/redaction.ts)'s allowlist entry), so there is no
reduced variant to serve — and a zeroed summary would be a *lie* rather than
a redaction, indistinguishable from a real idle window. The echoed
`since`/`until` are the requester's own parameters coming back. The public
handler does not run the D1 aggregation at all, so the unredacted numbers
never exist on that code path.

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
- **In-Worker Cloudflare Access JWT verification _on these query routes_** —
  the `/api/*` vs `/public/*` split (above) still relies entirely on the
  Cloudflare Access edge policy an operator configures per
  [`cloudflare-access.md`](cloudflare-access.md); neither prefix's handler
  verifies a credential itself. (The dashboard root `/` **does**, as of issue
  #4795 — see that guide's §5 and [`src/accessAuth.ts`](../src/accessAuth.ts)
  — but extending the same check to `/api/*` as defense in depth is
  deliberately *not* done here: `/api/*` is also reached non-interactively
  with an Access **service token**, whose JWT carries the `/api/*`
  application's own `aud`, so a single pinned-`aud` check would reject
  exactly the callers it must not break. Doing it properly means accepting
  the `Cf-Access-Jwt-Assertion` header with a per-application `aud`
  allowlist — a separate change.)
