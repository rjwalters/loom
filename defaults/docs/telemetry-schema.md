# Fleet Telemetry Schema (wire format)

> Epic #4702, Phase 1 — the versioned telemetry record schema the fleet
> observability pipeline is built on. Defined in Rust in
> `loom-daemon/src/telemetry/` (`mod.rs` — record kinds + envelope;
> `visibility.rs` — the repo-visibility derivation helper). This document is the
> **format-independent reference** so the Phase-2 Workers/TypeScript backend can
> parse the wire format without the Rust types.

This is a **schema + serialization** deliverable only — the daemon does not yet
emit, persist, or export these records here. Sibling Phase-1 issues consume these
types: #4704 persists them to a local journal, #4705 pushes them to a backend.

## Envelope

Every record is transmitted inside a versioned envelope:

```json
{
  "schema_version": 1,
  "emitted_at": "2026-07-30T12:00:00Z",
  "host_id": "fleet-host-abc",
  "record": {
    "kind": "sweep.outcome",
    "...": "record fields, flattened alongside `kind`"
  }
}
```

| Field            | Type              | Notes |
|------------------|-------------------|-------|
| `schema_version` | integer (`u32`)   | Current value: **1** (`CURRENT_SCHEMA_VERSION`). |
| `emitted_at`     | RFC 3339 datetime | When the daemon produced the envelope. |
| `host_id`        | string            | Stable identifier for the emitting host. Opaque to the schema. |
| `record`         | object            | The record payload, internally tagged on `kind` (see below). |

### `schema_version` semantics

`schema_version` is a **plain integer**, not a semver string, deliberately: a
backend ingesting a mixed-version fleet (some hosts on an older daemon mid
rolling-upgrade) gates on a numeric compare, with no semver parsing. It is bumped
only on a **breaking** wire change to the record shapes below. A backend should:

- **accept** records at any `schema_version` it recognizes;
- treat an **unknown (higher)** `schema_version` as forward-compatible where it
  can (unknown fields are additive) or route it to a dead-letter path otherwise;
- **never** silently coerce a missing `schema_version` to `0` — a record with no
  `schema_version` is malformed.

## `RepoVisibility` contract — private by default

Every record that references a repository carries a `visibility` tag, either
`"public"` or `"private"`. The Phase-2 public view exposes full detail for
`public` work and only redacted/summarized aggregates for `private` work, so this
tag is the schema-level anti-leak control.

**The decode is private-safe by construction.** The Rust deserializer maps *only*
the exact (case-insensitive) string `"public"` to `Public`; **everything else**
maps to `Private`:

- a missing `visibility` field ⇒ `Private`;
- an unknown label (e.g. `"internal"`) ⇒ `Private`;
- a `null`, a wrong-typed scalar (bool/number), or a nested array/object ⇒
  `Private`.

A partial or older-schema record can therefore **never accidentally decode to
`public`** and leak into the public view. The Phase-2 backend MUST implement the
same rule: default anything that is not exactly `"public"` to private. Redaction
keys off this tag, never off client-side filtering.

Visibility is derived at emit time from the forge (`gh api repos/{owner}/{repo}
--jq .private`) and cached per `owner/repo` with a TTL, so it costs no per-record
API call. A probe failure resolves to `private` — the same fail-safe default.

## Record kinds

The `record` object is internally tagged on `kind`. The tag values reuse the
frozen SSE `sweep.*` topic vocabulary where they overlap, plus the epic's added
kinds. Records that reference a repository carry `repo` + `visibility`; host-level
records (`tokens.snapshot`, `host.health`) do not.

### `sweep.started`

A sweep began work on an issue.

```json
{
  "kind": "sweep.started",
  "repo": "rjwalters/loom",
  "visibility": "public",
  "issue": 4703,
  "sweep_id": "sweep-issue-4703-0",
  "started_at": "2026-07-30T12:00:00Z",
  "model": "opus",
  "effort": "high"
}
```

`model` and `effort` are omitted when unset (empty-means-unset, mirroring
`SweepInfo`).

### `sweep.phase`

A sweep advanced to a new lifecycle phase (mirrors `sweep.issue.{N}.phase`).

```json
{
  "kind": "sweep.phase",
  "repo": "rjwalters/loom",
  "visibility": "public",
  "issue": 4703,
  "sweep_id": "sweep-issue-4703-0",
  "phase": "builder",
  "entered_at": "2026-07-30T12:03:20Z"
}
```

`phase` is a lifecycle name: `curator`, `builder`, `judge`, `doctor`, `merge`.

### `sweep.completed`

A sweep reached a terminal state (the summary moment; richer detail is in the
paired `sweep.outcome`).

```json
{
  "kind": "sweep.completed",
  "repo": "rjwalters/loom",
  "visibility": "public",
  "issue": 4703,
  "sweep_id": "sweep-issue-4703-0",
  "completed_at": "2026-07-30T12:08:32Z",
  "result": "success"
}
```

`result` is one of `success`, `failure`, `cancelled`, `blocked`.

### `sweep.outcome`

The full post-hoc outcome: model/config/effort, per-phase durations, terminal
result, and PR number. (A distinct type from the daemon's internal
`sweep_outcomes::OutcomeRecord`, which #4704 maps this into for its journal.)

```json
{
  "kind": "sweep.outcome",
  "repo": "rjwalters/loom",
  "visibility": "public",
  "issue": 4703,
  "sweep_id": "sweep-issue-4703-0",
  "model": "opus",
  "effort": "high",
  "config": { "runtime": "claude" },
  "phase_durations": [
    { "phase": "curator", "duration_sec": 12 },
    { "phase": "builder", "duration_sec": 340 }
  ],
  "total_duration_sec": 512,
  "result": "success",
  "pr_number": 4710
}
```

`config` (free-form string map), `phase_durations`, `model`, `effort`, and
`pr_number` are omitted when empty/unset. `config` is a map — not fixed fields —
so operator-tunable knobs can be captured without a schema bump.

### `tokens.snapshot`

A point-in-time view of the multi-account token pool (host-level — no `repo` /
`visibility`). Matches what `loom-daemon tokens check --ranking` knows.

```json
{
  "kind": "tokens.snapshot",
  "captured_at": "2026-07-30T12:00:00Z",
  "accounts": [
    {
      "account": "agent-1",
      "rank": 0,
      "usage_fraction": 0.42,
      "limit_window_reset_at": "2026-07-30T18:00:00Z",
      "exhausted": false
    },
    { "account": "agent-2", "exhausted": true }
  ]
}
```

Per account, `rank` / `usage_fraction` / `limit_window_reset_at` are omitted when
unknown; `exhausted` is always present.

### `host.health`

Host CPU/disk headroom, daemon version, and uptime (host-level — no `repo` /
`visibility`). Every measured field is optional so an unmeasurable probe stays
absent rather than being coerced to a fake zero (the daemon's "unknown != zero"
contract; see `cpu_headroom.rs` / `disk_headroom.rs`).

```json
{
  "kind": "host.health",
  "captured_at": "2026-07-30T12:00:00Z",
  "daemon_version": "0.16.0",
  "uptime_sec": 86400,
  "logical_cpus": 28,
  "cpu_idle_fraction": 0.83,
  "load_per_core": 0.51,
  "worktree_root_free_gb": 200
}
```

`cpu_idle_fraction`, `load_per_core`, and `worktree_root_free_gb` are omitted when
unmeasurable. A consumer MUST treat an absent measurement as "unknown", never as
zero/full.
