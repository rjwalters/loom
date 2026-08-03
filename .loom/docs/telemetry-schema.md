# Fleet Telemetry Schema (wire format)

> Epic #4702, Phase 1 — the versioned telemetry record schema the fleet
> observability pipeline is built on. Defined in Rust in
> `loom-daemon/src/telemetry/` (`mod.rs` — record kinds + envelope;
> `visibility.rs` — the repo-visibility derivation helper). This document is the
> **format-independent reference** so the Phase-2 Workers/TypeScript backend can
> parse the wire format without the Rust types.

This document defines the schema + serialization only. **#4704 (below) is the
first consumer that actually persists records** — a durable, append-only local
journal of `sweep.outcome` records, with a `loom-daemon` CLI read surface, no
exporter or cloud backend required. #4705 (still schema-only as of this
writing) will additionally push these records to a cloud backend.

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

## `/ingest` response (the bound-`host_id` echo)

A push is a bare JSON array of envelopes; the backend answers a **2xx with a
JSON object**:

```json
{ "accepted": 50, "host_id": "fleet-host-abc" }
```

| Field      | Type    | Notes |
|------------|---------|-------|
| `accepted` | integer | How many envelopes from this batch were persisted. Whole-batch semantics: a batch is either fully accepted or rejected with a non-2xx. |
| `host_id`  | string  | **The host id the authenticated ingest key is bound to** — i.e. the identity the batch's rows were actually filed under. Added by issue #4830. |

`host_id` here is *not* echoed from the request. Every record is persisted
under the identity bound to the presented key, never the envelope's own
(client-supplied, opaque) `host_id` field — so this echo is what a host was
actually recorded as, which is exactly the value that differs when the wrong
host's key file has been installed on a machine.

**How the exporter uses it.** `loom-daemon`'s exporter compares this value
against the identity the daemon resolved for itself (`$LOOM_HOST_ID`, else
`$HOSTNAME`, else `hostname`) and on a disagreement logs a WARN **once per
daemon lifetime** and reports an `observability DEGRADED` section in
`loom-daemon health`. Nothing about the export changes: the batch stays acked
and the backend keeps filing under the key's binding, which remains
authoritative. See `dashboard/docs/deploy-runbook.md` §8.

**Compatibility.** The field is purely additive — no `schema_version` rev is
involved (that integer versions the *record* envelope, not this response).
Both directions are safe:

- an exporter that ignores the response body behaves exactly as before;
- a backend that does not send `host_id` (anything predating #4830) is treated
  by the exporter as "no identity to verify" and is **silently** skipped — it
  never produces a recurring "cannot verify" warning.

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

Every field is read out of the pool's `.ranking` file, so each maps to one of its
pipe-delimited columns (`name|status|5h_util|limit_reset` — see
[`token-pool.md`](token-pool.md)): `rank` is the row's position, `usage_fraction`
is `5h_util`, `exhausted` is derived from `status`, and `limit_window_reset_at`
is `limit_reset`.

`limit_window_reset_at` is the instant the window **currently gating that
account** rolls over — the 7-day window for an `exhausted` account (when it
regains capacity), the 5-hour window otherwise (the rollover `usage_fraction` is
racing). The daemon resolves which one before writing, so a consumer never has to
know: it is always "when this account's constraint lifts". It is also the only
per-account field here that survives public redaction, aggregated across the pool
into `next_limit_window_reset_at` (the earliest reset, naming no account). A row
whose reset is absent or unparseable reports no reset at all rather than a
fabricated instant, so consumers must treat `null`/absent as *unknown* — never as
"resets now".

### `host.health`

Host CPU/disk headroom, the emitting binary's identity, and uptime (host-level —
no `repo` / `visibility`). Every measured field is optional so an unmeasurable
probe stays absent rather than being coerced to a fake zero (the daemon's
"unknown != zero" contract; see `cpu_headroom.rs` / `disk_headroom.rs`).

```json
{
  "kind": "host.health",
  "captured_at": "2026-07-30T12:00:00Z",
  "daemon_version": "0.16.0",
  "build_commit": "8c16fb5b",
  "built_at": "2026-07-30T03:09:51Z",
  "uptime_sec": 86400,
  "logical_cpus": 28,
  "cpu_idle_fraction": 0.83,
  "load_per_core": 0.51,
  "worktree_root_free_gb": 200,
  "dispatch_halted": false,
  "managed_repos": [
    { "slug": "rjwalters/loom", "visibility": "public" },
    { "slug": "2AMLogic/gf180-pll", "visibility": "private" }
  ]
}
```

`cpu_idle_fraction`, `load_per_core`, and `worktree_root_free_gb` are omitted when
unmeasurable. A consumer MUST treat an absent measurement as "unknown", never as
zero/full.

**Dispatch-attention state (`dispatch_halted` / `halt_reason`, #4975).** Whether
this host's own dispatch is currently halted for a non-idle reason, and why. A
host can be at 0% idle / high load-per-core and still report `status: "ok"` on a
naive check that only looks at token exhaustion — this is the daemon's own
authoritative "am I refusing new work right now" signal, so a consumer does not
have to re-derive it from raw CPU/load numbers.

- `dispatch_halted` is sourced from the host-distress breaker
  (`loom-daemon/src/host_breaker.rs`, Issue #4235): `true` when the breaker is
  `Open` or `CoolDown` (`BreakerPhase::suppresses_dispatch`), `false` when it is
  `Closed`, disabled, or was never registered (no work-finder loop running on
  this host — a repo that never enables autonomy always sends `false`).
  `#[serde(default)]`, so a record from a pre-#4975 daemon decodes as `false`
  ("not known to be halted"), not as a fabricated "healthy".
- `halt_reason` is the breaker's own human-readable transition message (e.g.
  `"load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)"`), present
  only while `dispatch_halted` is `true`. Omitted (not sent as `null`) while not
  halted.

Both fields are additive and pass through public redaction unchanged
(`dashboard/src/redaction.ts`) — dispatch-attention state describes the
machine, not any repo or operator.

**Binary identity (`build_commit` / `built_at`, #4956).** `daemon_version` is
`CARGO_PKG_VERSION`, so it only moves once per release: every build between two
releases reports the same string, and a day-stale daemon is indistinguishable
from current `main`. `build_commit` (the short git SHA the running binary was
compiled from) and `built_at` (when it was compiled) are the precise identity —
both come from the very same compile-time stamps `loom-daemon --version` prints
(`LOOM_DAEMON_GIT_COMMIT` / `LOOM_DAEMON_BUILD_TIME`, baked in by `build.rs`), so
the telemetry and the CLI can never disagree.

- `build_commit` is always sent. `"unknown"` is a *meaningful* value, not a
  missing measurement: it means the build host had no git (e.g. a
  release-tarball build). A record from a pre-#4956 daemon has no field at all,
  which decodes as an empty string.
- `built_at` is **omitted** when the build-time stamp was unavailable — an
  unknown build time is absent, never a fabricated instant, exactly like the
  measured fields above.

Both fields are additive and pass through public redaction unchanged (they
describe the released binary, not any repo or operator — see
`dashboard/src/redaction.ts`), so an older consumer that ignores unknown keys is
unaffected.

**`managed_repos` (#4976, redaction class: per-entry, public-visible slug
only).** This host's managed-repository roster — every workspace root the
daemon's workspace registry currently tracks, resolved to its forge
`owner/repo` slug and a [`RepoVisibility`](#repovisibility-contract--private-by-default)
tag derived exactly the way a sweep record's own `visibility` is derived
(`visibility::derive_visibility`). Sourced from the registry itself, **not**
inferred from `active_sweep_ids` — a registered-but-idle repo (no sweep ever
dispatched into it this run) still appears, which is the whole point: it lets
the dashboard show a host's quiet as "roster-shaped" (nothing ready in any of
its registered repos) rather than as an unexplained gap.

- Omitted entirely (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`)
  when the host has no registered workspaces, or on a record from a pre-#4976
  daemon — an older/partial record decodes with an empty roster rather than
  failing.
- **Unlike every other field in this record, `managed_repos` does NOT pass
  through public redaction unchanged.** Each entry names a specific
  repository, so the anti-leak contract applies per entry, not to the whole
  record: a `public`-visibility entry's `slug` survives to the public
  (unauthenticated) view; a `private` entry's `slug` is dropped but the entry
  itself is kept, so the roster's size — and therefore "how many are private"
  — stays visible without naming any of them (`dashboard/src/redaction.ts`'s
  `redactManagedRepos`). The authenticated view always sees every entry in
  full, including private slugs.

**`roles` (#5022, role-tick health).** The same transient-vs-persistent
classification `loom-daemon health`'s `roles` section already computes
(`crate::health::summarize_role_ticks`, fed by
`crate::role_runner::role_tick_records()`), carried through the telemetry
pipeline so a role dying on one host (the exact 2026-08-03 Judge outage #5004
was filed for) is observable fleet-wide rather than only to an operator who
happens to run `loom-daemon health` locally on that one host.

```json
{
  "kind": "host.health",
  "...": "every field above, plus:",
  "roles": {
    "total": 12,
    "ok": 10,
    "persistent": [
      {
        "root": "/repos/loom",
        "role": "judge",
        "failures": 2,
        "last_at": "2026-08-03T09:14:00Z",
        "detail": "no-token-pool"
      }
    ]
  }
}
```

- `total` / `ok` are the tick counts sampled from the process-global role-tick
  ring (`crate::role_runner::ROLE_TICK_RING_CAPACITY`-bounded), **not**
  windowed the way `loom-daemon health --since` is — a periodic `host.health`
  push always reports the ring's full current contents. `total: 0` means "the
  role runner sampled nothing" (idle or disabled entirely) and is a normal,
  healthy state — never render it as degraded.
- `persistent` lists only the `(root, role)` pairs whose **most recent**
  sampled tick is still a failure — the ones that make `loom-daemon health`'s
  `roles` section report `DEGRADED`. A pair that failed but has since
  recovered (its latest tick is a success) is folded into `total`/`ok` like
  every other tick and does **not** appear here, mirroring the transient
  count in `assess_roles`'s own rendered summary line.
- `#[serde(default)]` on the whole `roles` field, so a record from a
  pre-#5022 daemon decodes with the zero-value ("nothing sampled") summary
  rather than failing the whole envelope. `persistent` is additionally
  omitted from the wire when empty (`skip_serializing_if = "Vec::is_empty"`),
  mirroring `managed_repos`/`active_sweep_ids`.
- `root` is a local workspace filesystem path, not a forge `owner/repo`
  slug — it names no repository, issue, branch, or PR. But on the common
  macOS/Linux home-directory layout (`/Users/<user>/…`, `/home/<user>/…`) its
  leading segment *does* name the **operator**, the same "who runs the fleet"
  category `tokens.snapshot`'s `accounts` is held back for. So (redaction
  class: **per-entry, `root` basenamed**) the authenticated `/api/*` surface
  keeps the full path, but the public, unauthenticated view only ever gets each
  `root` truncated to its basename — mirroring the daemon's
  `RoleFailure::label()` and the frontend's `pathBasename`. `total`/`ok` and
  each failure's `role`/`failures`/`last_at`/`detail` pass through unchanged.
  Enforced by `redactRoleTickHealth` in `dashboard/src/redaction.ts` (like
  `managed_repos`, `roles` is deliberately absent from the raw allowlist and
  only reaches a public response through that derivation).

## Per-host reporting redundancy (why 3x storage is intentional, Issue #4999)

Every host in a fleet independently samples and pushes `tokens.snapshot` and
`host.health` on its own `SNAPSHOT_INTERVAL` (5 minutes —
`loom-daemon/src/observability/mod.rs`), each reporting the **same shared
account pool** every host in the fleet can see. On a 3-host fleet this means
each of those two record kinds independently accumulates roughly
`3 hosts x (90d / 5min) ~= 78k rows` per 90-day retention window — about 3x
what a single elected reporter would produce for the same information.

This redundancy was raised as a possible optimization (#4999) and
investigated as a trade study rather than assumed to be a bug:

- **The redundancy is load-bearing.** When `loom-worker-1` went dark for ~80
  minutes on 2026-08-01, the other two hosts kept reporting the same account
  pool, so the account-level series had no gap. A single elected reporter
  would have produced exactly that gap during the one window an operator most
  needs the series to stay continuous — while a host is dying.
- **Storage is nowhere near a limit that matters.** `records` is already
  bounded by both an age cutoff (`RETENTION_DAYS`, default 90) and a hard row
  cap (`MAX_RECORDS`, default 500,000 — `dashboard/src/retention.ts`). Even
  counting *all* per-host periodic sampling (`tokens.snapshot` + `host.health`
  together, ~156k rows/90d across 3 hosts) alongside per-sweep lifecycle
  records, total volume sits well under a third of the configured cap. There
  is no pressure to relieve.

**Decision: keep the current per-host independent-reporting design.**
Electing a single reporter (or deduplicating identical rows at write/read
time) would trade away a real survivability property for a storage saving
that isn't currently needed. Revisit only if `MAX_RECORDS` eviction starts
firing routinely (sustained growth actually approaching the cap) — at that
point, write-time deduplication that preserves per-account distinguishability
(still being able to tell "account exhausted" from "no host reported" for a
given account/bucket) is the option to pursue first, since electing a single
reporter reintroduces the exact single-point-of-failure this trade study
found unacceptable.

## Persistence & read surface (`sweep.outcome`, Issue #4704)

The daemon durably records one `sweep.outcome` [`TelemetryEnvelope`] per
completed sweep — success, failure, or cancellation — to a local, append-only
JSONL journal: `<workspace_root>/.loom/logs/sweep-outcome-telemetry.jsonl`
(override via `LOOM_SWEEP_OUTCOME_TELEMETRY_JOURNAL_PATH`, or per-registry via
`SweepRegistryConfig::outcome_telemetry_path`). This happens **regardless of
whether any exporter is configured** — local durability is the point: history
survives a daemon restart and outlives any cloud backend (#4705).

Written by `loom-daemon/src/sweep_registry.rs`'s
`append_outcome_telemetry_journal` at the same three terminal-transition call
sites as the older, narrower `#4644` `OutcomeRecord` journal
(`sweep-outcomes.jsonl` — see `loom-daemon/src/sweep_outcomes.rs`'s module
doc for why the two files are kept separate): the reaper's crashed/exited
handling in `reap_once`, and the operator/watchdog-initiated `finish_cancel`.
Best-effort like its sibling — a write failure is logged and never blocks
reaping. Same bounded-retention policy: rotates to a single `.1` backup once
the file exceeds 5 MiB or its oldest line is more than 30 days old.

**`phase_durations` is sampled**: the registry samples each live sweep's
checkpoint (`.loom/sweep-checkpoint/issue-<N>.json`) once per reaper tick
(≤30s, finer in practice) and records each transition, because the checkpoint
is overwritten at every phase boundary and deleted by the sweep skill on
success — nothing on disk holds a history. Durations are therefore accurate to
within one sampling interval; the trailing in-flight segment (last observed
phase completion → terminal transition) is not attributed to any phase, so the
entries sum to at most `total_duration_sec`. A daemon restart mid-sweep loses
the earlier observations: such a record falls back to a single best-effort
entry (last known phase, whole duration) or an empty list, never a fabricated
phase name. Phase names are the checkpoint markers normalized to lifecycle
names (`curator-done` → `curator`, `judge-rejected` → `judge`), and a phase
that runs twice (the Judge↔Doctor cycle) yields two entries in lifecycle order.

**`pr_number` costs no forge call**: it is captured from the same checkpoint
read (the sweep skill records `pr_number` from `builder-done` onward), so it
names the PR *this sweep produced* and survives the checkpoint's deletion on
success. A terminal transition therefore adds no GraphQL round trip to the
reaper's hot path; the only forge lookup in the write path is the cached
`owner/repo` + visibility resolution.

**How `result` is decided** (all from state the daemon already holds — no extra
forge call):

| Terminal transition | `result` |
|---|---|
| Merge phase observed to complete (`merge-done` sampled) | `success` |
| Operator/watchdog cancel (`finish_cancel`) | `cancelled` |
| Clean exit (code `0`) with no `merge-done`, not the #4366 no-progress shape | `success` |
| Everything else — non-zero exit, unobservable exit status, the #4366 clean-exit-with-zero-progress shape, or a death that left a checkpoint behind | `failure` |

An *unobservable* exit status (a reconstructed entry reaped via `kill(pid, 0)`,
which yields no code) is deliberately a `failure`, not a `success`: absence of
evidence is not evidence of a merge. The schema's fourth variant, `blocked`, is
reserved for a human-decision blocker; the daemon does not yet emit it, because
the blocker signal (`sweep.issue.{N}.blocker`) and the post-Builder build gate
are both child-side and are not routed into the registry.

### Local inspection: `loom-daemon sweep-outcomes`

```bash
# Success rate and median duration, grouped by model (the #4137 AC4 query):
loom-daemon sweep-outcomes

# Individual records, newest first:
loom-daemon sweep-outcomes --records --limit 20

# Filter by model and/or result (success | failure | cancelled | blocked):
loom-daemon sweep-outcomes --model opus --result failure --records

# Machine-readable:
loom-daemon sweep-outcomes --json
```

Purely file-based (like `loom-daemon calibrate`) — no running daemon required.
`--workspace PATH` selects a different repo root (default `.`).

## Alternative sinks: the OTLP exporter (Epic #4702, Phase 4 — issue #4858)

The native JSON-over-HTTPS push (`exporter::HttpsExporter`, above) is the
default sink and stays that way for backward compatibility. Operators with an
existing OpenTelemetry stack (a self-hosted collector, Grafana, Honeycomb, …)
can instead select `observability.exporter = "otlp"`
(`$LOOM_OBSERVABILITY_EXPORTER=otlp` overrides; **env > config > default**,
default `"https"`), which POSTs to `{observability.endpoint}/v1/logs` and
`{observability.endpoint}/v1/metrics` using the same `observability.endpoint`
+ `observability.ingestKeyFile` + `Authorization: Bearer` convention as the
HTTPS sink.

This is opt-in twice over: off by default (`observability.enabled`) *and*
gated behind the `otlp` Cargo feature — a default `loom-daemon` build never
compiles in the `opentelemetry-proto` dependency, so choosing `HttpsExporter`
costs nothing extra. The full `TelemetryEnvelope` → OTLP field mapping (which
record kinds become OTLP logs vs. metrics, and how `host_id` / `emitted_at` /
the repo-visibility tag map onto OTLP resource/record attributes) is
documented at `loom-daemon/src/observability/otlp/mod.rs`'s module doc
comment, not duplicated here — that Rust doc comment is the source of truth,
verified by `loom-daemon/src/observability/otlp/mapping.rs`'s unit tests.

[`TelemetryEnvelope`]: #envelope
