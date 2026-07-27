# Loom Daemon Reference

> **Status: ACTIVE (v0.10.0).** This page describes the Rust `loom-daemon`
> binary and its MCP-facing surface — the dispatch + pub/sub + monitoring
> tools delivered by epic #3449 (Phases A through C). The legacy Python
> `loom-daemon` brain (`loom_tools/daemon_v2/`) and the `/shepherd`
> orchestrator were deleted in the v0.10.0 deprecation epic (#3372). The
> shell-level `./.loom/scripts/daemon.sh` tmux session launcher (when
> rebuilt under epic #3449's later phases) wraps this same daemon binary.

## What the daemon is

`loom-daemon` is a Rust process that exposes a Unix-socket IPC surface
(framed JSON, line-delimited) and a paired `mcp-loom` MCP server which
maps each IPC request 1:1 to an MCP tool. The daemon is **the
coordination point** for:

- **Dispatching** `/loom:sweep` children with multi-account OAuth token
  rotation (via `defaults/scripts/spawn-claude.sh`).
- **Tracking** running sweeps in an in-memory registry (no on-disk state
  file — the forge is the source of truth for queue state).
- **Publishing** sweep-lifecycle events on an in-memory pub/sub bus, and
  **subscribing** external monitors to topic-filtered streams.
- **Cancelling** in-flight sweeps with SIGTERM → grace → SIGKILL.
- **Reaping** dead PIDs (every 30s) to maintain registry liveness and
  emit `sweep.issue.*.exited` / `sweep.issue.*.crashed` events.

**By default it is not a work generator.** With no autonomous config it
does not poll the forge for ready issues, it does not maintain a
`shepherd-N` pool, and it does not run support roles on cron — those
responsibilities live in `mcp__loom__dispatch_sweep` (operator-driven
enqueue) and the GitHub Actions cron workflows
(`.github/workflows/loom-*.yml`). Two **opt-in, default-off** surfaces
(epics #3809 and #3842) let the daemon generate and dispatch its own work
when explicitly enabled: the [autonomous work
finder](#autonomous-work-finder-3810) polls open `loom:issue` items and
auto-dispatches sweeps, and the [epic supervisor](#epic-supervisor-3842)
drives `loom:epic` fork-joins. See [Operability](#operability--config-startstop-e2e-phase-d-3813)
for enabling and tuning them.

## Architecture (Phases A-C)

```
┌────────────────────────────────────────────────────────────────┐
│                      MCP clients (Claude Code)                 │
│  - dispatch_sweep, list_sweeps                          (A)    │
│  - publish_event, subscribe_to_events                   (B)    │
│  - get_sweep_status, tail_sweep_log, cancel_sweep       (C)    │
│  - tail_event_bus                                       (C)    │
└────────────────────────────────────────────────────────────────┘
                              │ stdio JSON-RPC
                              ▼
┌────────────────────────────────────────────────────────────────┐
│                    mcp-loom (TypeScript)                       │
│  - Validates args, normalizes payloads, formats output         │
│  - One MCP tool per IPC Request variant                        │
└────────────────────────────────────────────────────────────────┘
                              │ Unix socket, line-delimited JSON
                              ▼
┌────────────────────────────────────────────────────────────────┐
│                    loom-daemon (Rust)                          │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────┐  │
│  │ SweepRegistry    │  │ EventBus         │  │ ReaperTask   │  │
│  │ (BTreeMap)       │  │ (broadcast chan) │  │ (30s tick)   │  │
│  └──────────────────┘  └──────────────────┘  └──────────────┘  │
│                              │                                  │
│                              ▼                                  │
│                    fork+exec /loom:sweep N                      │
│                    via spawn-claude.sh                          │
└────────────────────────────────────────────────────────────────┘
                              │ detached child
                              ▼
                       /loom:sweep <issue>
                       (Claude Code session)
```

## IPC surface (Request/Response variants)

The wire protocol is line-delimited JSON. Each `Request` is one line; the
daemon responds with one line per request — except `SubscribeEvents`,
which holds the connection open and streams one `EventStream` frame per
event. Connection framing matches the existing terminal-management IPC
surface; no new transport is introduced.

Source of truth: [`loom-daemon/src/types.rs`](../../loom-daemon/src/types.rs).

| Request | MCP tool | Response | Phase |
|---------|----------|----------|-------|
| `DispatchSweep`     | `dispatch_sweep`       | `SweepDispatched`   | A (#3452) |
| `ListSweeps`        | `list_sweeps`          | `SweepList`         | A (#3452) |
| `PublishEvent`      | `publish_event`        | `EventPublished`    | B (#3453) |
| `SubscribeEvents`   | `subscribe_to_events`, `tail_event_bus` | `EventStream` (stream) | B (#3453) |
| `GetSweepStatus`    | `get_sweep_status`     | `SweepStatus`       | C (#3455) |
| `TailSweepLog`      | `tail_sweep_log`       | `SweepLogTail`      | C (#3455) |
| `CancelSweep`       | `cancel_sweep`         | `SweepCancelled`    | C (#3455) |

## Event taxonomy (frozen for v0.10.0)

The bus accepts arbitrary topic strings, but the documented taxonomy is
the contract subscribers should rely on. **New topics require a follow-up
issue** — the v0.10.0 set is intentionally frozen.

| Topic | Publisher | Payload |
|-------|-----------|---------|
| `sweep.issue.{N}.phase`   | Sweep child via `publish_event` | `{phase, pr_number?, repo?}` |
| `sweep.issue.{N}.blocker` | Sweep child                     | `{reason, label_added, repo?}` |
| `sweep.issue.{N}.exited`  | Daemon reaper (or `cancel_sweep`) | `{exit_code, duration_sec, repo?}` |
| `sweep.issue.{N}.crashed` | Daemon reaper                   | `{checkpoint_phase, repo?}` |
| `sweep.global.dispatch`   | Daemon                          | `{sweep_id, kind}` |
| `sweep.global.completed`  | Daemon                          | `{sweep_id, outcome}` |
| `epic.issue.{N}.decompose` | Epic supervisor (#3842)        | `{epic, action, state}` |
| `epic.issue.{N}.expand`    | Epic supervisor (#3842)        | `{epic, action, state}` |
| `epic.issue.{N}.join`      | Epic supervisor (#3842)        | `{epic, action, state}` |
| `epic.issue.{N}.close`     | Epic supervisor (#3842)        | `{epic, action, state}` |
| `daemon.capacity.advisory` | Work finder (#3902)            | `{pressured, queued, healthy_accounts, exhausted_accounts, total_accounts, estimated_drain_minutes?, message}` |

The four `epic.issue.{N}.*` topics were authorized by **#3873** (epic #3842
Phase 4) and are documented in full under [Epic supervisor](#epic-supervisor-3842)
below. The `daemon.capacity.advisory` topic was authorized by **#3902** (epic
#3809): the autonomous work finder publishes it on a token-capacity **pressure
state change** (entered/left the token-bound state), never every tick, so the
operator gets one add-capacity advisory on the way in and one recovery on the way
out. See [Token-capacity backpressure](#token-capacity-backpressure-3902) below.
They ride the same in-memory bus as the sweep topics and are tailable via
`subscribe_to_events` / `tail_event_bus`.

The four `sweep.issue.{N}.*` payloads gained an additive **`repo`** field in
**#3929** (`(repo, issue)`-aware sweep visibility, phase c of #3926). The bus is
shared across every managed repo, and the topic string is issue-scoped only —
two managed repos can each dispatch a sweep for issue #42 onto the *identical*
`sweep.issue.42.phase` topic. `repo` carries the owning registry's
`workspace_root`, so a multi-repo-aware subscriber can disambiguate them after
matching the topic. **The topic strings are unchanged** — `repo` lives in the
payload only, so existing single-repo subscribers that filter on `sweep.issue`
or `sweep.issue.{N}` route byte-for-byte identically and simply ignore the new
field. The daemon stamps `repo` centrally when it emits each event; a sweep
child that already knows its repo (via `publish_event`) may supply it and will
not be overwritten. `sweep.global.*` events are unchanged — they already carry a
unique `sweep_id`.

In addition, the bus internally emits:

- `sweep.system.topic_lag` — synthetic event when a subscription falls
  behind the publisher past the bus capacity. Mirrors tokio's `Lagged`
  semantics; carries `{skipped: usize}`.

Topic matching is **segment-aligned prefix** (`sweep.issue` matches
`sweep.issue.123.phase` but not `sweep.issuetype.foo`). See
[`event_bus::topic_matches`](../../loom-daemon/src/event_bus.rs) for the
authoritative routing rule.

## MCP tool reference

All tools live in `mcp-loom/src/tools/sweeps.ts`. Each tool name maps
1:1 to an IPC `Request` variant.

### `dispatch_sweep` (Phase A)

Spawn a `/loom:sweep` child via the daemon's registry. The daemon shells
out to `defaults/scripts/spawn-claude.sh` for token rotation and detaches
the child. Returns the `sweep_id`, child PID, token-account name, and
per-sweep log path.

Inputs:
- `kind` (required) — `{"Issue": <N>}` or `{"PrSet": [<N>, ...]}`. Phase
  A only fully implements `Issue`; `PrSet` is rejected by the registry.
- `idempotency_key` (optional) — dedup key. Running sweeps with the same
  key return the existing `sweep_id` without spawning a new child.
- `model` (optional, issue #3477 Phase 1) — Claude model for the spawned
  child, as an alias (`sonnet`, `opus`, `haiku`) or a pinned ID
  (`claude-sonnet-4-6`). Forwarded as `--model <value>` on the
  `spawn-claude.sh` argv. When omitted (or empty), NO `--model` flag is
  emitted and the child inherits the session/CLI default. The field is
  `#[serde(default)]` on the wire, so pre-#3477 clients remain compatible.
- `depends_on` (optional, issue #3729 stacked-PR v1) — a **single** parent
  issue number this sweep is stacked on. Forwarded to the child as
  `--depends-on <N>` (mirroring the `--model`/`--effort` append-only,
  empty-means-unset contract), instructing `/loom:sweep` to branch the child
  worktree/PR off `feature/issue-<N>` instead of the default branch. When
  omitted, NO `--depends-on` flag is emitted (byte-for-byte unchanged). A
  single optional parent (not a list) makes diamonds / multi-parent stacks
  structurally unrepresentable — see "Stacked-PR dependency (v1)" below. The
  field is `#[serde(default)]` on the wire, so pre-#3729 clients remain
  compatible.
- `workspace_root` (optional, issue #3929) — target managed-workspace root.
  When omitted, the sweep is dispatched into the daemon's **default** workspace
  (byte-for-byte unchanged). When set to a registered repo root, the daemon
  resolves that repo's sweep registry via the `WorkspacePool` and dispatches into
  its working tree — the way to dispatch into a managed repo other than the
  default when two repos share issue numbers. `#[serde(default)]` on the wire.

#### `loom-daemon dispatch <issue>` — operator CLI (Issue #3952)

`loom-daemon dispatch <issue>` is the **non-MCP** operator entry point onto the
same `DispatchSweep` IPC request the `dispatch_sweep` MCP tool uses. It is a thin
client: connect to the daemon socket, send one `DispatchSweep` frame, print the
returned `sweep_id` + per-sweep log path, exit `0`. Because it flows through the
registry, the `loom:issue → loom:building` claim flip, in-flight tracking, the
reaper, and event publishing all come for free — exactly like the MCP path.

```bash
loom-daemon dispatch 3952                          # dispatch into the default workspace
loom-daemon dispatch 3952 --workspace /path/to/repo # target a registered managed repo (#3929)
loom-daemon dispatch 3952 --model sonnet --effort high
loom-daemon dispatch 3952 --depends-on 3945        # stacked-PR child (#3729)
```

| Flag | Maps to `DispatchSweep` field | Notes |
|------|-------------------------------|-------|
| `<issue>` (positional) | `kind = {"Issue": N}` | required |
| `--workspace <PATH>` | `workspace_root` | target a registered repo other than the default (#3929) |
| `--model <M>` | `model` | omit to let the daemon resolve `autonomous.model` / the shipped default (#3944) |
| `--effort <E>` | `effort` | reasoning-effort override (#3716) |
| `--depends-on <P>` | `depends_on` | single parent issue; child branches off `feature/issue-<P>` (#3729) |

**Bounded ack timeout (never hangs).** The CLI waits at most **30s** for the
daemon to ack the dispatch, then exits **nonzero** with a clear
`Daemon did not ack the dispatch within 30s (...) — is loom-daemon running?`
message rather than blocking. The 30s default mirrors the `mcp__loom__dispatch_sweep`
tool's own `DISPATCH_TIMEOUT_MS` for the identical IPC call, and exists because
`SweepRegistry::dispatch()` does real synchronous work *before* it acks — a
blocking `gh issue edit` label flip, up to a 2s dispatch stagger, and up to a 5s
token-name capture window — so a legitimate, successful dispatch can take several
seconds to ack. A tighter bound (the original 5s) would false-report those real
successes as `did not ack`. Operators on a slow forge or a heavily-loaded daemon
can *raise* the bound with `LOOM_DAEMON_IPC_TIMEOUT_MS=<ms>` (the same env var
`mcp-loom` honors); it only ever raises above the 30s floor, never lowers it (a
lower value would reintroduce the false negative). The timeout is always a
bounded, finite value: the MCP `dispatch_sweep` path once wedged for **1800s**
(#3945), and this command must never reproduce that hang.

**Replaces the hand-rolled pattern.** Before #3952 the only non-MCP alternative
was to reproduce the daemon's dispatch by hand — flip the label, export
`LOOM_SWEEP_CLAIM_OWNED=<N>` plus `LOOM_MODEL` and workaround envs, and invoke
`spawn-claude.sh -p "/loom:sweep N"` directly:

```bash
# DEPRECATED — do NOT do this. Bypasses the registry (no in-flight tracking,
# no reaper, no status visibility) and a claim-marker mismatch makes the child
# skip its own issue.
gh issue edit 3952 --remove-label loom:issue --add-label loom:building
LOOM_SWEEP_CLAIM_OWNED=3952 LOOM_MODEL=sonnet \
  ./.loom/scripts/spawn-claude.sh -p "/loom:sweep 3952"
```

Use `loom-daemon dispatch 3952` instead — it performs the claim flip, registry
tracking, and event publishing for you, with the bounded timeout as a safety net.

### `list_sweeps` (Phase A)

Return all tracked sweeps, optionally filtered by lifecycle state.
Terminal entries are garbage-collected ~1h after the transition.

Inputs:
- `state_filter` (optional) — one of `Pending`, `Running`, `Exited`,
  `Crashed`.
- `workspace_root` (optional, issue #3929) — target managed-workspace root.
  Omit to list the default workspace's sweeps (unchanged). Set to a registered
  repo root to list the sweeps tracked by that repo's registry — the way to
  observe sweeps the daemon autonomously dispatched into a non-default managed
  repo. Each returned `SweepInfo` also carries a `repo` field naming its owner,
  so a response is self-describing even without filtering. Cross-repo
  aggregation in a single call is deferred to phase d (#3930). `#[serde(default)]`
  on the wire.

The same optional `workspace_root` input (default = default workspace, unchanged)
is accepted by `get_sweep_status`, `tail_sweep_log`, and `cancel_sweep` — so a
sweep the daemon dispatched into a non-default managed repo can be inspected,
tailed, and cancelled by naming its repo root (#3929).

### `publish_event` (Phase B)

Publish a JSON event onto the in-memory bus. Operator override / test
escape hatch — production publishes happen via the sweep skill, not this
tool.

Inputs:
- `topic` (required) — should follow the frozen taxonomy.
- `payload` (required) — opaque JSON.

### `subscribe_to_events` (Phase C)

Open a long-lived subscription to the event bus, filtered by topic
prefix. Frames arrive as line-delimited JSON matching
`Response::EventStream { events: [Event] }`. The MCP layer caps each
subscription with a `duration` window so a single tool call returns
deterministically.

Inputs:
- `topics` (optional) — array of topic prefixes; empty = all events.
- `duration` (optional, default `30s`) — `<N>s`/`<N>m`/`<N>h` window.
- `max_events` (optional) — upper bound on frames returned.

### `get_sweep_status` (Phase C)

Return the `SweepInfo` for a single sweep plus up to N recent events
observed on its topics (default 10). The bus is in-memory and transient
— recent-events collection is a best-effort short subscribe window
(~200ms), not a replay log.

Inputs:
- `sweep_id` (required).
- `recent_events` (optional, default 10) — set to 0 to skip the
  subscribe window.

### `tail_sweep_log` (Phase C)

Read the last N lines of a sweep's per-sweep log file
(`.loom/logs/sweep-issue-<N>.log`). The log path is resolved from the
registry entry.

Inputs:
- `sweep_id` (required).
- `lines` (optional, default 100).

### `cancel_sweep` (Phase C)

SIGTERM → wait `grace` seconds → SIGKILL the sweep's child PID.
Transitions the registry entry from `Running` to `Exited{code: None,
at: now}` and releases the per-issue lock. Idempotent: cancelling an
already-terminal sweep returns success with `was_running: false`.

Inputs:
- `sweep_id` (required).
- `grace` (optional, default 30) — seconds between SIGTERM and SIGKILL.

### `tail_event_bus` (Phase C)

Debug-oriented fire-hose subscription that streams ALL events on the bus
regardless of topic. Added per curator risk note D — multi-child
interactions are qualitatively harder to debug than hermetic children.

Inputs:
- `since` (optional, default `10m`) — `<N>s`/`<N>m`/`<N>h` streaming
  window. **Note**: the bus is transient — `since` is a streaming
  duration, not a backward-looking replay filter.
- `max_events` (optional) — upper bound on frames returned.

## In-memory registry layout

The sweep registry (`loom-daemon/src/sweep_registry.rs`) holds a
`BTreeMap<SweepId, SweepInfo>` keyed by stable IDs of the form
`sweep-issue-<N>-<unix-secs>` or `sweep-prs-<n1>-<n2>-...-<unix-secs>`.
`SweepInfo` carries:

- `sweep_id`, `kind` (`Issue(N)` or `PrSet(Vec<u32>)`), `pid`,
  `token_name`, `log_path`.
- `idempotency_key` (optional), `started_at`.
- `state` — one of `Pending`, `Running`, `Exited{code, at}`,
  `Crashed{at}`.
- `latest_phase` (optional) — most-recent phase advertised via
  checkpoint.
- `pr_number` (optional, reserved).
- `repo` (optional, issue #3929) — the owning managed-workspace root
  (`config.workspace_root`), stamped at dispatch/reconstruct time so a
  `list_sweeps` / `get_sweep_status` response disambiguates two managed repos'
  identically-numbered issues. `#[serde(default, skip_serializing_if = "Option::is_none")]`,
  so pre-#3929 wire data and clients remain compatible.

The wire shape is pinned by `sweep_info_schema_snapshot` in
`sweep_registry.rs` — a change to the JSON shape requires deliberate
test update.

## Per-workspace registry pool (`WorkspacePool`, #3928/#3929)

`loom-daemon/src/workspace_pool.rs` holds **one independent `SweepRegistry` per
registered repo root**, keyed by `PathBuf`, so every path a registry computes
(`.loom/locks/issue-<N>`, `.loom/logs/`, `.loom/sweep-checkpoint/`, and the
spawned child's `current_dir`) is namespaced per repo — two repos each with issue
#42 never collide. The default workspace's registry is **seeded** into the pool
(shared with the IPC `dispatch_sweep` path); the autonomous work-finder and epic
supervisor provision the other managed repos' registries on demand
(`get_or_provision`). The IPC handler resolves a request's target registry from
the pool when the request carries an explicit `workspace_root` (#3929), so all
per-repo registries are observable/addressable via `list_sweeps` /
`get_sweep_status` / `tail_sweep_log` / `cancel_sweep` / `dispatch_sweep`.

**Eviction on `workspace remove` (#3929)**: `DeregisterWorkspace` (the
`workspace remove` CLI) calls `WorkspacePool::evict`, which drops the pooled
registry and **aborts its background reaper task** so it does not leak. The
**seeded default workspace is guarded** — it is owned by `main` and keeps serving
default-workspace IPC requests, so evicting it is a no-op. A live sweep child in
an evicted registry is **not** killed and its lock/log files are untouched; only
the in-memory tracking + reaper go away, so the sweep finishes normally but its
terminal state becomes unobservable via IPC after the deregister — an accepted
consequence of an explicit operator `workspace remove`.

## Token pool provisioning for managed repos (#3938)

The multi-workspace work finder measures the token pool **once per tick from the
daemon's primary workspace** (`fallback_root`) and uses it as the single global
concurrency budget — token accounts are a *machine-level* resource, so the cap is
never replicated per repo. But each dispatched sweep runs `spawn-claude.sh` with
its `current_dir` set to **its own** repo root, and `spawn-claude.sh` resolves the
token pool from that repo. A freshly-installed **consumer repo has no
`.loom/tokens/` of its own** — only the primary workspace was bootstrapped — so
every cross-repo dispatch used to hard-fail instantly with `EX_CONFIG`
("Run `loom-tokens bootstrap` …"), burning a dispatch slot per tick on children
that died in ~2s.

**Fix — a shared machine-level pool with a per-repo fallback.** Token selection
and *all* pool-state bookkeeping resolve the effective pool directory as:

1. the **per-repo** pool `<repo>/.loom/tokens/` when it holds `*.token` files
   (unchanged for the primary workspace);
2. else the **shared** machine-level pool `~/.loom/tokens/` (override
   `LOOM_SHARED_TOKENS_DIR`; set it empty to disable the fallback);
3. else the per-repo path (so a truly-unbootstrapped repo still surfaces a clear
   "run bootstrap" error).

Crucially, the **state files** (`.bad_tokens`, `.failure_counts`, `.ranking`,
`.allowlist`) are read/written in *whichever pool directory was selected* — so a
consumer repo dispatching against the shared pool shares one `.bad_tokens` /
`.ranking` truth with every other repo. Pool state is **never forked per repo**,
which is what keeps the token-capacity backpressure accounting (#3907/#3930)
consistent. The Rust `token_pool_size` (the dynamic-cap input) applies the same
per-repo→shared resolution, so the daemon's concurrency ceiling matches what the
spawn path can actually pick.

**Provisioning a managed-repo pool.** Bootstrap the shared pool once per machine:

```bash
loom-tokens bootstrap --shared      # writes ~/.loom/tokens (override LOOM_SHARED_TOKENS_DIR)
loom-tokens check --ranking         # ranks the effective pool (shared when no per-repo pool)
```

Every consumer repo the daemon dispatches into then falls back to that one pool —
no per-repo `loom-tokens bootstrap` required. A repo that *wants* its own isolated
pool can still `loom-tokens bootstrap` locally; the per-repo pool always wins.
Selection sources (`~/.claude-monitor/accounts.env`, repo-local `.env`) are
unchanged — `--shared` only redirects the *destination* of the materialized pool.

## Per-repo status breakdown + per-repo main-health gate (#3930 — phase d)

Phase d is the final phase of the multi-repo daemon (#3926/#3835). Phases b/c
already delivered the single global concurrency budget, per-workspace error
isolation, and `(repo, issue)`-aware IPC/events. Phase d closes the two remaining
gaps: making `loom-daemon status` see *every* managed repo, and making the
reactive main-health gate **per-repo** instead of one flag gating all repos.

### Per-repo status breakdown (AC1)

`build_daemon_status` (`ipc.rs`) enumerates
`WorkspaceRegistry::effective_roots(&fallback_root)` (an **empty** registry ⇒ the
single daemon workspace, byte-for-byte the pre-#3930 view) and reads each root's
own registry from the `WorkspacePool` (`get_or_provision`). It returns:

- `DaemonStatusReport.in_flight` — now the **union** of non-terminal sweeps across
  every registered repo, so a sweep the autonomous loops dispatched into a
  non-default managed repo is finally visible in `loom-daemon status`.
- `DaemonStatusReport.per_repo: Vec<RepoStatus>` — one entry per root with `root`,
  `in_flight_count`, and `health_gate_halted`. Additive + `#[serde(default)]`, so
  pre-#3930 JSON consumers round-trip unchanged (an absent `per_repo` deserializes
  to an empty vec).

`loom-daemon status` prints a **Managed repos** section (root, in-flight count,
gate state); `loom-daemon status --json` adds the `per_repo` array. The
dynamic-cap inputs (token pool, disk headroom, cpu/load headroom (#3978),
configured ceiling) remain computed once from the daemon's primary workspace —
they are *machine-level* resources, so
they stay a single global figure, not per-repo. `resolve_registry` (the
per-request `workspace_root` targeting used by `dispatch_sweep` / `list_sweeps` /
etc.) is unchanged: the cross-repo aggregation is a read-only snapshot for
`DaemonStatus` only. A merged `list_sweeps` across all repos without an explicit
`workspace_root` remains a deferred follow-up.

### Per-repo main-health gate (AC2)

The reactive gate was single-repo, single-flag: one `MainHealthState` driven by
one gate check against the daemon's own workspace, applied uniformly to every
registered repo's dispatch (a red `main` in one repo halted all repos; a red
`main` in any *other* registered repo was never even checked). Phase d replaces
the single flag with `WorkspaceHealthStates` — a `HashMap<PathBuf,
Arc<MainHealthState>>` keyed by normalized root (mirroring `WorkspacePool`'s
keying) — and adds `spawn_multi_main_health_gate_task`, which each cycle:

1. Re-reads `effective_roots(&fallback_root)` (hot-applies `workspace add|remove`).
2. Runs **one gate check per registered root**, resolving that root's own
   enablement (`autonomous.mainHealthGate.enabled`, env > config > default) and
   its own `buildGate` block — no new config schema. A root that is
   disabled / has no `buildGate` block is treated as **always-green** (its halt
   flag is cleared and no command runs). Gates run **sequentially** per tick, so
   several minutes-long per-repo builds firing together never contend (each
   `CommandGateRunner` already isolates its own `origin/main` sync + uuid temp
   file).
3. Applies each outcome to that root's own `MainHealthState`.

`work_finder::tick_multi` now takes a `halted: &[bool]` slice parallel to the
workspaces, so a **red repo skips only its own dispatch loop** (its backlog is
still counted in `seen` for logging, and its in-flight sweeps still seed the
shared global occupancy) while sibling repos keep dispatching. The epic supervisor
likewise attaches each cached per-repo supervisor to that root's own
`MainHealthState`. `DaemonStatusReport.main_health_gate_halted` keeps its pre-#3930
meaning (the daemon's own primary workspace); per-repo halt lives in
`per_repo[].health_gate_halted`.

**Empty-registry equivalence**: with a single workspace exactly one root is ever
keyed, so both the status view and the gate cadence/halt semantics reduce to the
pre-#3930 single-workspace behavior byte-for-byte. Enablement is still opt-in
(`LOOM_MAIN_HEALTH_GATE` / `autonomous.mainHealthGate`, precedence env > config >
default); the startup master switch reads the daemon's own workspace config, so
enabling the gate for a genuine multi-repo deployment is done with the machine-
global `LOOM_MAIN_HEALTH_GATE=1` env var (each repo's own `buildGate` block then
decides whether it actually gates).

## Gate verdicts: VERIFIED_RED vs UNEVALUATED (#3974)

A safety gate that fails closed on **its own** infrastructure failures converts
every environmental hiccup into a total dispatch outage — and, for the repo that
contains the gate's own source, into a bootstrap deadlock where the daemon cannot
dispatch the fix for the thing that is broken. Before #3974 *any* non-zero gate
exit (including a timeout, `sh` exit 127 because `cargo` was not on the daemon's
`PATH`, or a spawn failure) was recorded as "main is RED — HALTING".

Every gate run now resolves to exactly one of three outcomes
(`main_health_gate::GateOutcome`):

| Outcome | Meaning | Effect on dispatch |
|---------|---------|--------------------|
| `Green` | command ran to completion, exit 0 | clears any halt |
| `Red` (**VERIFIED_RED**) | command **ran to completion** and reported failure | **halts** this repo's dispatch |
| `Unevaluated` (**UNEVALUATED**) | the gate produced no verdict | **preserves** the previous verdict; logs loudly with the class |

`UnevaluatedClass` names why, and is surfaced in the daemon log and in
`loom-daemon status`:

| Class | Trigger |
|-------|---------|
| `dirty-tree` | non-ignorable local changes; the workspace was never synced |
| `not-on-main` | workspace is on another branch / detached HEAD |
| `local-ahead` | local `main` carries commits `origin/main` lacks (#3912) |
| `git-failure` | a `git` step failed (`rev-parse` / `status` / `fetch` / `rev-list` / `reset`) |
| `timeout` | the gate command exceeded `buildGate.timeoutSeconds` |
| `command-not-executable` | `sh` exit 127 (not found) or 126 (not executable) |
| `killed-by-signal` | terminated by a signal (e.g. an OOM kill) |
| `spawn-failure` | could not spawn the command, or an I/O error while capturing output |
| `contradicted-by-forge-ci` | ran and failed locally, but forge CI is green on the same commit (below) |

The discriminator is deliberately narrow so a genuinely failing build still halts:
**any other non-zero exit is trusted as VERIFIED_RED** — `cargo test`'s 101 for a
failing test still halts dispatch.

### Forge-CI corroboration of a local red

A local run can also fail *because of the host it runs on*: on the incident host
six `integration_basic` tests assert `tmux_session_exists(...)` and fail because
the tmux server is dead, while `.github/workflows/ci.yml` runs the identical
`cargo test --workspace` on the same commit and passes. The local gate measures
**this host**; forge CI measures **the commit**.

So a completed-and-failed run is cross-checked against the forge's CI conclusion
for the exact `origin/main` SHA the gate evaluated (post-sync `HEAD`), via
`gh run list --branch main --json headSha,status,conclusion,workflowName`
matched on `headSha`:

- CI **green** on that SHA → downgraded to `contradicted-by-forge-ci`
  (UNEVALUATED); logged loudly, **does not halt**.
- CI **red** on that SHA → corroborated; still VERIFIED_RED, still halts.
- CI **unknown** → fail safe: still VERIFIED_RED, still halts.

Only *positive* contrary evidence ever relaxes a halt, so "green" is the hardest
verdict to reach and everything short of an unambiguous all-clear degrades to
**unknown**:

| Runs for the evaluated SHA | Verdict |
|---|---|
| any `failure` / `timed_out` / `startup_failure` | red |
| any run not yet `completed` (`queued`, `in_progress`, …) | unknown — CI has not judged the commit yet |
| any `cancelled` / `action_required` / `stale` / unrecognized conclusion | unknown — the workflow reached no verdict about the code |
| at least one `success`, every other run `skipped` / `neutral` | **green** |
| no run for the SHA, unparseable output, `gh` missing/unauthenticated, probe timeout > 30s | unknown |

**Absence of failure is not success.** Requiring at least one genuine `success`
*and* no outstanding or indeterminate sibling run closes two ways a
"saw any completed run ⇒ green" reducer reads green on non-evidence: a
`cancel-in-progress: true` concurrency group leaves superseded runs at
`completed/cancelled` **forever** (a permanent false green for that commit), and a
fast bookkeeping workflow that finishes minutes before the real build would
otherwise vouch for the commit on its own. Neither case needs the probe to know
*which* workflow "counts", so no workflow name is hard-coded.

Corroboration is on by default and can be disabled with
`LOOM_GATE_CI_CORROBORATION=0` (for repos with no forge CI or no `gh`); it is only
probed on a local red, never on a green run.

### One shared halt state

`work_finder::tick_multi` derives `TickReport.halted` **directly** from the
`WorkspaceHealthStates` flags the gate writes, rather than accumulating it inside
the candidate-gathering loop. Previously a `list_ready_issues` error short-circuited
the loop *before* its halt check, so a repo whose forge query failed reported
`halted = false` — the incident's `work_finder: main-health gate cleared —
resuming dispatch` logged in the same window the gate logged `still RED`. The two
loops now read one source of truth and cannot disagree.

`loom-daemon status` reports the not-evaluated **cause** verbatim
(`main_health_gate.not_evaluated_reason`, `per_repo[].health_gate_not_evaluated_reason`)
instead of the pre-#3974 hard-coded "workspace tree is dirty", which misreported
timeouts, missing tools, and `git fetch` failures on a clean tree as a dirty tree.

## Per-workspace priority tiers (#3946)

By default the multi-repo work-finder and epic supervisor iterate
`effective_roots()` in **registration order**, so a deep product-repo backlog can
starve the tool repos whose fixes compound. Priority tiers add cross-repo dispatch
ordering:

- **Registry schema** — each `~/.loom/workspaces.json` entry gains an optional
  `priority` integer (`Workspace.priority`, `loom-daemon/src/workspace_registry.rs`):
  **lower = higher priority**, default `100`
  (`DEFAULT_WORKSPACE_PRIORITY`). Fully backward compatible — an entry with no
  `priority` (every pre-#3946 file) deserializes as `100` via `#[serde(default)]`,
  so an all-default registry orders exactly as before.

- **CLI** —
  - `loom-daemon workspace add <path> --priority N` registers a repo in tier `N`
    (default `100`).
  - `loom-daemon workspace set-priority <path> N` retiers an already-registered
    repo.
  - `loom-daemon workspace list` prints a `PRIO` column, sorted highest-priority
    first (mutation order on disk is preserved).

- **Work-finder ordering** — `work_finder::tick_multi` now takes a
  `priorities: &[u32]` slice parallel to the workspaces. Instead of dispatching
  each repo's backlog in registration order, it gathers **every** eligible
  candidate across all workspaces into one queue, sorts it by `candidate_cmp` —
  **(workspace priority asc, `loom:urgent` first, issue age asc/oldest-first,
  issue number asc)** — and fills the single shared concurrency budget in that
  global order. The cap/budget mechanics (#3811/#3930) are unchanged; this only
  orders the queue. `createdAt` is added to the `gh issue list --json` fields for
  the age key.

- **Epic supervisor** — `spawn_multi_supervisor_thread` reorders its cached
  per-repo supervisors by workspace priority each tick (stable within a tier) before
  `tick_multi`, so higher-tier epics advance/dispatch first.

- **Status** — `RepoStatus.priority` is surfaced in the `loom-daemon status`
  **Managed repos** table (a `PRIO` column) and the `--json` `per_repo[].priority`
  field, with the breakdown sorted highest-priority first.

**Starvation stance (v1):** strict priority is intentional — tool repos are small
queues that drain fast. A permanently-full higher tier **will** starve lower tiers;
fairness knobs (per-tier slot reservations) and cross-repo dependency awareness are
explicit follow-ups, deferred until observed to matter.

## Reaper task

The reaper (`sweep_registry::spawn_reaper_task`) ticks every 30 seconds
(env-overridable via `LOOM_SWEEP_REAPER_INTERVAL_SECS`). Each tick:

1. Snapshots live `Running`/`Pending` entries.
2. Tests each PID via `kill(pid, 0)`.
3. On dead PID:
   - If a sweep checkpoint exists at
     `.loom/sweep-checkpoint/issue-<N>.json`, marks the entry `Crashed`
     and flips the forge label `loom:building` → `loom:issue` so the
     next dispatch resumes from the checkpointed phase.
   - Otherwise marks the entry `Exited{code: None}`.
   - Emits `sweep.issue.{N}.exited` or `sweep.issue.{N}.crashed`, plus
     a global `sweep.global.completed` event.
4. Garbage-collects terminal entries older than the retention window
   (default 1 hour).

## Stale-claim reconciliation & the sweep journal (#3953, fixed #3975)

Two independent surfaces reclaim abandoned `loom:building` claims when the
sweep that owned them has died, using the same evidence source and the same
decision rule:

| Surface | Where | When it runs |
|---------|-------|---------------|
| Rust startup reconciliation | `claim_reconciliation::forge::reconcile_workspace` (called from `main.rs` at daemon startup, guarded by `LOOM_STALE_CLAIM_RECONCILE`, default on) | Once, on every daemon start, across every `effective_roots()` workspace |
| Python `loom-recover-orphans` | `loom_tools.orphan_recovery.check_untracked_building` | On demand (operator/cron invocation of `loom-recover-orphans [--recover]`) |

Both read the same machine-level **sweep journal** (`~/.loom/sweeps.json`,
override `LOOM_SWEEPS_JOURNAL_PATH`, written by `sweep_journal::record_sweep`
on every `SweepRegistry::dispatch`) and apply the same three-way decision
rule per `loom:building` issue:

1. **Journal entry with a live recorded PID** → keep (genuinely in-flight).
2. **Journal entry with a dead recorded PID** → reclaim. This is the
   strongest possible evidence — a specific PID that provably no longer
   exists — so both surfaces treat it as authoritative.
3. **No journal entry at all** → reclaim only once the claim has been stale
   longer than `LOOM_STALE_BUILDING_HOURS` (default 4h; also drives the
   Rust-side default `DEFAULT_STALE_BUILDING_HOURS`). Absence of a record
   might mean a live manual `/loom:sweep` the journal was never told about
   (a pre-journal claim, or the journal file just doesn't exist on this
   machine), so a much wider grace window applies than for a claim the
   journal has proof about.

### The #3975 bug: pruning before deciding

The journal is deliberately self-pruning — `sweep_journal::upsert` (every new
dispatch) and the Rust reconciliation pass both call `prune_dead` to drop
dead-PID entries so the file never accumulates an unbounded graveyard
(mirrors `sweep-run-registry.sh`'s `prune_dead`/`peers`, #3768).

Before #3975, `reconcile_workspace` called `prune_dead` **before** calling
`plan()`/`decide()` — which deleted the exact dead-PID entry the `DeadPid`
branch above needs to fire its immediate, unconditional reclaim. With the
entry gone, every claim silently fell through to branch 3 (no record) and
its much longer `stale_hours` grace window, even for a sweep that had *just*
died. Two claims in a downstream workspace (issues #6170/#6173: SIGTERMed
during a daemon restart) sat un-reclaimed for hours because of this —
`loom-recover-orphans --recover` found and fixed only a third, unrelated
claim (#6172) whose label happened to already be older than the stale-hours
threshold. The fix: `reconcile_workspace` now decides against the raw,
unpruned journal first; pruning happens only afterward, via the normal
per-reclaimed-issue `remove_sweep` cleanup (leftover dead entries for
untouched issues are still pruned lazily by the next `upsert` elsewhere).

The Python side (`gather_liveness_evidence` / `check_untracked_building`)
never pruned the journal itself — it only reads whatever the file currently
contains — so it was not directly bugged the same way, but it inherited the
same *symptom*: by the time an operator ran `loom-recover-orphans` by hand,
a routine daemon dispatch (or the (now-fixed) Rust startup pass) had often
already pruned the dead-PID evidence out from under it, leaving only the
weaker "no record" signal to work with.

### Never-silent staleness-gate skips (#3975)

Separately, `check_untracked_building`'s staleness gate (branch 3 above, and
the short `label_grace_period` gate that also applies to branch 2) used to
log its "SKIPPED: #N ..." line **only** under `--verbose`, so a default
(non-verbose) `loom-recover-orphans` run gave no visible trace that a claim
had been seen and excluded — indistinguishable from the tool never having
looked at that issue at all. `OrphanRecoveryResult.watched` now records
every staleness-gated skip (issue, reason, label age, threshold) regardless
of `--verbose`, and both `format_result_human` and `--json` output always
include it. A watched entry is explicitly **not** an orphan — it may still
be alive — but it is never silently dropped.

### One intentional asymmetry: dead-PID grace period

Rust's `decide()` reclaims a `DeadPid` claim **unconditionally** — no
staleness check at all (see `decide_dead_pid_overrides_label_age`). Python's
`check_untracked_building` still applies the short `label_grace_period`
(default 600s) even to a `journal_pid_dead` reason. This is intentional, not
a bug to unify: the Rust pass runs once per daemon *start*, a rarer and more
deliberate event, while the Python tool can be invoked ad hoc (including
immediately after a claim is made, before its journal entry has even been
written) — the short grace period is defense-in-depth against a race the
once-per-restart Rust pass is much less exposed to.

## Stacked-PR dependency — #3729 (v1), #3747 (v2 item 1)

Stacked-PR mode pipelines a genuine dependency: when issue B consumes issue
A's output, B is built on `feature/issue-A` so B's Curator→Builder→Judge runs
concurrently with A's review instead of serializing behind A's merge. **The
dispatch surface is opt-in, daemon-`dispatch_sweep`-only, and
linear-chains-only.**

**Dispatch a chain** — N independent `dispatch_sweep` calls, each naming its
immediate predecessor via `depends_on` (there is no multi-node planner):

```text
dispatch_sweep  kind={"Issue": A}                    # parent (independent)
dispatch_sweep  kind={"Issue": B}  depends_on=A      # child stacked on A
dispatch_sweep  kind={"Issue": C}  depends_on=B      # A→B→C linear chain
```

The daemon forwards `depends_on` to the child as `--depends-on <parent>`; the
child's Builder branches its worktree off `feature/issue-<parent>` (via
`worktree.sh --base`) and opens its PR with `--base feature/issue-<parent>`.
`depends_on` is `Option<u32>` — a **single** optional parent — so diamonds /
multi-parent stacks are structurally unrepresentable (no runtime rejection
needed). It is recorded on the `SweepInfo` entry for observability.

**Block-the-subtree on parent failure (reaper).** When a parent sweep reaches
a terminal state and its issue carries `loom:blocked`, the reaper emits
`sweep.issue.{child}.blocker` on the existing frozen topic (#3453 — no new
topic) for every live child whose `depends_on` names that parent, so the stuck
stack surfaces to the operator and the child does not auto-progress. This is
implemented via `SweepRegistry::children_of` + `block_children_of`. Auto-detach
(rebasing an orphaned child onto the default branch) is **out of scope for v1**.

**Reconciliation is triggered automatically on parent merge (v2 item 1,
#3747).** Because the repo squash-merges, after the parent squash-merges the
child branch still carries the parent's pre-squash commits. `merge-pr.sh` now
fires reconciliation automatically at its post-merge choke point (alongside the
partial-increment label reset, before branch deletion): it discovers open child
PRs via a **live forge query** (`gh pr list --base feature/issue-<parent>` — not
the daemon registry, whose terminal entries are GC'd ~1h after transition and
which only exists when `loom-daemon` is running), then per child splits
safe/unsafe on the child **issue's** `loom:building` label (fresh, uncached `gh
api` read):

- **Safe** (child issue not `loom:building`): invokes
  `./.loom/scripts/reconcile-stack.sh <child-pr> feature/issue-<parent>`
  (`git rebase --onto <default> <parent-branch> <child-branch>` +
  `--force-with-lease` + `gh pr edit --base <default>`).
- **Unsafe** (child issue still `loom:building`): a live Builder likely holds
  the child branch checked out, so the auto-rebase is **skipped** and a comment
  is posted on the child PR flagging deferred reconciliation. A later
  parent-merge-triggered pass (once the issue is no longer `loom:building`), or
  a manual run, picks it up.

The whole step is **best-effort** — a reconciliation failure (rebase conflict,
rejected force-with-lease, retarget failure) is logged as a warning and never
changes `merge-pr.sh`'s exit code (the parent merge already happened). It is
idempotent by construction: once a child's base is retargeted away from the
parent branch, the `--base` query returns zero rows on any re-run.

`reconcile-stack.sh` remains available for **manual** invocation — for the
unsafe/deferred case once the Builder finishes, or for an operator who wants to
reconcile ahead of a merge (`--dry-run` previews the git surgery).

A **pre-merge merge-ordering guard** shipped as v2 item 2 (#3747): because
`delete_branch_on_merge:true` deletes `feature/issue-<parent>` synchronously
during the merge API call — before the post-merge reconcile pass above can run —
`merge-pr.sh` now runs a guard *before* both merge paths that discovers open
child PRs (same `gh pr list --base feature/issue-<parent> --state open` query)
and by default **hard-blocks the merge** (`exit 1`, naming the child PR(s) + the
`reconcile-stack.sh` unblock command) rather than let the parent merge race the
branch deletion. It keys purely on "does an open child PR still target this
branch" (not the child's `loom:building` label). `--allow-stacked-children`
bypasses it; `--dry-run` reports the would-be block without exiting 1.

**Rebase-on-parent-amend** shipped as v2 item 3 (#3747): the standalone
`./.loom/scripts/rebase-stacked-children.sh feature/issue-<parent>` handles the
*pre-merge* case where Doctor amends a still-open stacked parent branch and a
child that branched off its pre-amend tip goes stale. It discovers open child
PRs with the same `gh pr list --base feature/issue-<parent> --state open` query,
detects staleness via `git merge-base --is-ancestor`, and rebases safe stale
children onto the parent's current tip (`git rebase` + `push --force-with-lease`,
base **not** retargeted — the child stays stacked), deferring children whose
issue is still `loom:building` with a comment. Doctor invokes it as a documented
best-effort step after pushing to a `feature/issue-<N>` branch. **Dependency
auto-detection**, **diamonds / multi-parent**, and **auto-detach** remain **out
of scope** (deferred items of the v2 epic #3747).

## Epic supervisor (#3842)

The **epic supervisor** (epic #3842) drives every open `loom:epic` issue
through a fork-join lifecycle autonomously. It runs as an opt-in loop on a
**dedicated OS thread** with its own current-thread Tokio runtime (`#3872`) —
never `tokio::spawn` on the shared daemon runtime — because each transition can
block on a minutes-long role process (`Command::status()`) while holding the
#3707 issue-creation mutex. Keeping that blocking call off the shared runtime
preserves the responsiveness of the event bus, reaper, sweep registry, and IPC
listener.

Enable it with `LOOM_EPIC_SUPERVISOR=1` (unset/false-y = OFF). Tunables:
`LOOM_EPIC_SUPERVISOR_INTERVAL_SECS` (default 300) and
`LOOM_EPIC_INFLIGHT_TTL_SECS` (default 900).

### Derived-state model

Rather than mint new GitHub labels per phase, all five supervisor states ride
the single `loom:epic` label and are **derived** — computed each tick from two
already-visible facts: the number of `### Phase` sections in the epic body, and
the open/closed status of the epic's `loom:epic-phase` children. The five states
(implemented as `EpicState` in
[`loom-daemon/src/epic_state.rs`](../../loom-daemon/src/epic_state.rs)) mirror
the `derived=True` epic lane of the authoritative Python model
([`loom-tools/src/loom_tools/state_machine.py`](../../loom-tools/src/loom_tools/state_machine.py),
#3841):

| Derived state | Condition | Enabled transition |
|---------------|-----------|--------------------|
| `epic:needs_decomp` | body has `< 2` `### Phase` sections | **decompose** — Architect enriches the body in place (no PR) |
| `epic:designed` | `≥ 2` phases, no `epic-phase` children yet | **expand** — Champion materializes phase-1 children (under the #3707 mutex) |
| `epic:active` | a current-phase child is open | per-child `/loom:sweep` dispatch (`BuildChildren`) |
| `epic:phase_join` | current phase's children all closed, more phases remain | **join** — Champion materializes phase N+1 children (mutex + barrier-gated) |
| `epic:done` | all phases' children closed, no phases remain | **close** — Champion closes the epic (terminal) |

### Transition table + phase-join barrier

The five intra-lane edges among the derived states — the "epic transition
table" — are declared explicitly in `epic_state::epic_transition_table()`:

```text
epic:needs_decomp → epic:designed    (Champion, creates_issues)   [decompose]
epic:designed     → epic:active      (Champion)                   [expand]
epic:active       → epic:phase_join  (Supervisor, barrier)        [fork-join]
epic:phase_join   → epic:active      (Supervisor, barrier)        [join/advance]
epic:phase_join   → epic:done        (Supervisor, barrier)        [close]
```

Every edge touching `epic:phase_join` is a **phase-boundary edge** and declares
a non-empty fork-join barrier
([`loom-daemon/src/phase_join.rs`](../../loom-daemon/src/phase_join.rs)): the
barrier holds — degrading the plan to a no-op — until every child of the current
phase is closed, so phase N+1 (or epic close) never fires while a current-phase
child is still open.

The lane-*entry* edge `new → epic:needs_decomp` (an Architect filing a
`loom:epic` proposal) is **not** part of the supervisor's table — the supervisor
begins its lifecycle at `epic:needs_decomp`.

**Conformance.** The Rust transition table is asserted faithful to the Python
model by
[`loom-daemon/tests/epic_conformance.rs`](../../loom-daemon/tests/epic_conformance.rs),
which **derives** its expectation by invoking
`python3 -m loom_tools.state_machine --json` and comparing the emitted epic
sub-graph (states, edges, roles, barriers, `creates_issues`) against the Rust
table — rather than hardcoding a mirrored copy that would silently drift. The
test skips gracefully when `python3` is unavailable.

### #3707 issue-creation mutex

The two issue-creating expand bursts (`decompose`'s downstream and both
`expand`/`join` Champion dispatches that run `gh issue create`) are serialized
through the global **#3707 issue-creation mutex**
([`loom-daemon/src/issue_creation_mutex.rs`](../../loom-daemon/src/issue_creation_mutex.rs)).
The supervisor holds the async guard across the whole (spawn-and-wait) dispatch
so a burst never interleaves with any other issue-creating burst anywhere in the
daemon. All epic expands share the single `CHAMPION_EPIC_DECOMP` serialization
identity.

### Event topics

Each of the four singleton action-class transitions publishes an
`epic.issue.{N}.{action}` event on the shared event bus when it fires, so the
supervisor's decisions are tailable via `subscribe_to_events` /
`tail_event_bus`:

| Topic | Fires from | Payload |
|-------|-----------|---------|
| `epic.issue.{N}.decompose` | `epic:needs_decomp` | `{epic, action: "decompose", state: "epic:needs_decomp"}` |
| `epic.issue.{N}.expand`    | `epic:designed`     | `{epic, action: "expand", state: "epic:designed"}` |
| `epic.issue.{N}.join`      | `epic:phase_join`   | `{epic, action: "join", state: "epic:phase_join"}` |
| `epic.issue.{N}.close`     | `epic:done`         | `{epic, action: "close", state: "epic:done"}` |

The `BuildChildren` transition (per-child `/loom:sweep` dispatch) has **no**
epic-action topic — those dispatches already surface on the frozen
`sweep.global.dispatch` topic. Subscribe to `epic.issue` to receive every
epic-supervisor action across all epics, or `epic.issue.{N}` for one epic
(segment-aligned prefix match, same routing rule as the sweep topics).

## Autonomous work finder (#3810)

The **work finder** (Phase A of epic #3809,
[`loom-daemon/src/work_finder.rs`](../../loom-daemon/src/work_finder.rs)) is the
daemon-native poller that turns a human-approved `loom:issue` into a dispatched
build **without an operator** — restoring the one capability the deleted v0.10.0
shepherd brain had that the daemon rebuild never replaced. It is **opt-in and
off by default**: unset `LOOM_WORK_FINDER` and the daemon's behavior is
byte-for-byte unchanged (the only sweep entry point remains the explicit
`DispatchSweep` IPC request).

Unlike the epic supervisor, the work finder runs as a plain `tokio::spawn`
interval task on the **shared daemon runtime** (the same footing as the reaper),
not a dedicated OS thread. Every call into `SweepRegistry::dispatch()` returns
promptly (fire-and-forget child spawn), so the loop never parks a runtime worker
in a long blocking call — the OS-thread machinery the epic supervisor needs for
its minutes-long spawn-and-wait role dispatches is unnecessary here.

Each tick:

1. Queries the forge for ready work — `gh issue list --label loom:issue --state
   open --limit 200 --json number,labels` (honoring `LOOM_REPO` for `--repo`).
2. Filters out issues that are **already in flight** (a live `Running` /
   `Pending` entry in the sweep registry — the authoritative dedup, robust to
   `loom:issue → loom:building` label-flip lag) or that defensively carry any
   skip label (`loom:building` / `loom:blocked` / `loom:operator-only`).
3. Dispatches the remainder through the existing `SweepRegistry::dispatch()`
   path — up to a **work-driven dynamic cap** (Phase B, #3811) recomputed every
   tick and counted against the current live sweep occupancy. `dispatch()`
   already flips `loom:issue → loom:building`, acquires the per-issue
   `mkdir`-atomic claim lock, and spawns the rotated-token child, so the finder
   reimplements none of the race guard. Each dispatch uses a
   `workfinder-<issue>` idempotency key, making a re-dispatch of an
   already-running issue a no-op.

### Dynamic concurrency scaling (Phase B, #3811; CPU/load term #3978)

The concurrency cap is **not** a fixed value resolved once at startup. Every
tick the finder recomputes

```
dynamic_cap = min(healthy-token count × per-token concurrency, disk headroom, cpu/load headroom, configured ceiling)
```

from live inputs, so pool/disk/cpu/backlog changes are honored without a
daemon restart:

| Input | Source | Bound it enforces |
|-------|--------|-------------------|
| **healthy-token count** | `available` accounts in `{workspace}/.loom/tokens/.ranking` (`capacity::token_axis_limit`), falling back to the `*.token` count (`tokens::token_pool_size`) when no ranking exists | the count of accounts safe to dispatch to — never dispatch to an exhausted/blocked one (#3902) |
| **per-token concurrency** | `LOOM_PER_TOKEN_CONCURRENCY` / `autonomous.perTokenConcurrency`, default **2** (#3947) | how many concurrent sweeps to allow **per healthy account**. A plan limit is a utilization-window token bucket, not a session count, so one healthy account can run several concurrent sessions. Before #3947 the implicit factor was `1` (one sweep per account), which collapsed the whole fleet to cap 1 when 6/7 accounts were at their weekly ceiling even though the single healthy account had ample session-window headroom |
| **disk headroom** | `floor(free_gb / LOOM_PER_WORKTREE_GB)` on the worktree-root volume (`disk_headroom::disk_headroom_limit`, a Rust port of `disk-headroom.sh` that shells to `df -Pk`) | never provision more worktrees than the scratch volume can hold |
| **cpu/load headroom** (#3978) | `max(1, floor((logical_cpus × LOOM_CPU_UTILIZATION_TARGET − 1m loadavg) / LOOM_EST_CORES_PER_SWEEP))` (`cpu_headroom::cpu_headroom_limit`) | never start more concurrent sweeps than the host's CPU/load headroom can currently absorb |
| **configured ceiling** | `LOOM_WORK_FINDER_MAX_CONCURRENT` (repurposed from Phase A's fixed target into an operator ceiling) | hard operator upper bound regardless of token/disk/cpu headroom |

**CPU/load headroom term (#3978).** The token and disk axes alone let a batch
of token accounts resetting from `exhausted` to `available` at once raise the
cap regardless of how many concurrent `cargo build`s were already saturating
the host — the incident this term fixes: 2–3 concurrent Rust builds in sweep
worktrees starved `build-gate.sh` of CPU badly enough that it hit its own 600s
timeout, which the (separately-fixed, #3974) gate misreported as a verified-red
`main`, halting all dispatch. `cpu_headroom_limit()` combines a **static**
capacity (`logical_cpus × utilization_target`, default target `0.75` — leaves
headroom for the OS, the daemon itself, and the gate's own `cargo`
invocations) with the **current** 1-minute load average (`/proc/loadavg` on
Linux, `sysctl -n vm.loadavg` on macOS) subtracted from that capacity, divided
by an estimated per-sweep core cost (`LOOM_EST_CORES_PER_SWEEP`, default
`2.0`). Like the token axis's "one healthy account is the floor, never a
halt" policy (#3902), the CPU term is floored at `1` — a load-average read
failure or a noisy reading must never by itself wedge the whole dynamic cap
to zero; disk headroom and the token axis remain the only terms allowed to
floor to a genuine `0`. Tunable via `LOOM_CPU_UTILIZATION_TARGET` (fraction,
default `0.75`) and `LOOM_EST_CORES_PER_SWEEP` (cores, default `2.0`) —
env-only, mirroring `LOOM_PER_WORKTREE_GB`'s config surface (no
`.loom/config.json` knob).

**Per-token concurrency factor (#3947).** The token axis is `healthy × factor`,
not `healthy × 1`. The factor is resolved with the standard precedence **env
(`LOOM_PER_TOKEN_CONCURRENCY`) > config (`autonomous.perTokenConcurrency`) >
default (2)**; a zero/unparseable value at any layer is ignored, and the cap
formula additionally clamps the factor to a floor of `1` so a mis-set `0` degrades
to the pre-#3947 one-sweep-per-account behavior rather than dispatching nothing.
Bounded **stacking**, not a 1:1 hard limit, is the correct response to a healthy
account with session-window headroom — the #3909 rotating selection spread still
fills **distinct** accounts first (via the persistent `.rotation_cursor`), only
stacking multiple sweeps on one account when concurrency demand exceeds the
healthy-account count. The `loom-daemon status` view spells the arithmetic out,
e.g. `= min(healthy 1 × per-token 2 = 2, disk headroom 120, cpu headroom 6,
configured max 3)`, and a separate line reports the live loadavg/core-count
detail feeding the cpu headroom term (#3978 AC4).

**Session-limit fault handling (#3947).** Stacking can occasionally trip a
**concurrent-session-limit** fault on a token (the account is healthy but cannot
start another *simultaneous* session right now). This is a **capacity** signal,
not quota exhaustion, so `classify-error.sh` classifies it distinctly as
`SESSION_LIMIT` (checked *before* `TOKEN_EXHAUSTED` so the "session limit" wording
is not swallowed by the exhaustion regex). `claude-wrapper.sh` responds by
re-selecting a **different** account and retrying **without** appending the
current token to `.bad_tokens` — a capacity fault must never poison the healthy
pool. Re-selection advances the rotation cursor so a healthy sibling is preferred,
which backs off stacking for the saturated account; a bounded
`LOOM_MAX_SESSION_LIMIT_RETRIES` (default 10) guards a fully-saturated pool from
spinning, after which it falls through to normal transient backoff (still without
marking the token bad).

The **effective** per-tick concurrency is then `min(dynamic_cap, backlog_depth)`:
`tick()` iterates the ready `loom:issue` rows and stops at the cap, so
concurrency **scales up** as the backlog grows and drains to **zero** dispatches
when the queue is empty (no capacity is pre-reserved and no idle workers are
spawned). A token pool of 0 (rotation not bootstrapped) yields a cap of 0 —
the finder dispatches nothing, matching `spawn-claude.sh`'s `EX_CONFIG`
hard-fail on a missing pool. The `df` probe runs once per tick and is negligible
on the 60s default interval. Bad-token-aware pool counting (subtracting
`.bad_tokens` entries) is a tracked follow-up; the first pass counts `*.token`
files.

The loop is **idempotent** (an issue already in the registry is never
re-dispatched) and **fail-safe**: a forge-query error aborts only that tick
(logged, retried next tick) and a single dispatch error is logged and counted,
never crashing the daemon. Dispatches surface on the frozen
`sweep.global.dispatch` topic (emitted inside `dispatch()`); the finder adds no
new event topics.

Enable it with `LOOM_WORK_FINDER=1` (unset/false-y = OFF) **or** from committed
config (`autonomous.workFinder.enabled`, see "Operability" below). Tunables:
`LOOM_WORK_FINDER_INTERVAL_SECS` (default 60 — tighter than the epic
supervisor's 300s so the `loom:issue` backlog drains promptly),
`LOOM_WORK_FINDER_MAX_CONCURRENT` (default 3 — the operator **ceiling** in the
dynamic policy above, not a fixed target), `LOOM_PER_TOKEN_CONCURRENCY` (default 2
— the per-healthy-token concurrency factor of the cap, #3947), and
`LOOM_PER_WORKTREE_GB` (default 2 — the per-worktree disk estimate the
disk-headroom bound divides by). A zero or
unparseable value for any of these falls back to its default.

> **Scope note**: the work finder dispatches **already-approved** `loom:issue`
> items; it does **not** generate new work. Architect/Hermit work-generation
> cadence remains out of scope (follow-up #3381). So "the daemon does not
> generate work" below still holds — the finder only closes the gap between an
> approved issue and its build.

### Token-capacity backpressure (#3902)

At scale, rotation accounts hit their 5h/7d rate limits and go `exhausted`.
Dispatching to an exhausted account produces startup hangs / mid-build deaths, so
the finder treats a genuine token limit as a **capacity signal** — slow down,
alert, recover — all automatic and non-blocking:

1. **Slow down (backpressure).** The token axis of the dynamic cap is the count
   of **healthy** (`available`) accounts read from `.loom/tokens/.ranking`
   (`capacity::token_axis_limit`), not the flat `*.token` count. When accounts go
   exhausted the cap backs off toward the healthy count; when *every* account is
   exhausted it drops to 0 and the finder **defers** the queue rather than
   hammering an exhausted account. A single healthy account is the throughput
   **floor**, never a halt. When no `.ranking` file exists (no probe has run) the
   axis falls back to the raw pool size — byte-for-byte the pre-#3902 behavior.
2. **Alert (add capacity).** When the token axis is the *binding* constraint
   (≤ disk and ≤ ceiling) and work is queued behind it, the finder is
   *token-bound*. On the **state change** into that state it emits an
   add-capacity advisory naming concrete levers — add accounts to
   `~/.claude-monitor/accounts.env` + `loom-tokens bootstrap`, or buy API
   credits, then `loom-tokens check --ranking` — with the current numbers
   (queued count, healthy/total accounts, exhausted count, estimated drain time
   at current capacity). The advisory surfaces on **three** channels: the daemon
   log (`warn`), the `daemon.capacity.advisory` event-bus topic, and the
   `capacity` section of `loom-daemon status`. It is **deduplicated** — one
   advisory on entry, one recovery on exit, never per-tick spam. Advisory only;
   it never blocks dispatch.
3. **Recover.** The finder re-reads the ranking every tick (bounded cadence = the
   tick interval), so as accounts reset to `available` the cap ramps back up and
   the queued `loom:issue` backlog drains automatically — no manual intervention.
   A symmetric recovery line/event fires on the way out of the pressured state.

The `estimated_drain_minutes` figure is a coarse `ceil(queued / healthy) ×
NOMINAL_SWEEP_MINUTES` (30 min nominal) aid, not a precise SLA — the daemon does
not track live per-sweep durations here. Near-ceiling granularity is limited to
the `.ranking` discrete status word (`exhausted` is already ≥ 0.95 utilization);
a finer sub-exhausted (≥ 0.90) bucket would read the richer `loom-tokens check
--json` utilization and is a tracked follow-up. Even rotation/staggering of
dispatches across the available account set (so 5h/7d windows reset in a
staggered pattern) lives in the spawn-time selector (`loom_tools.tokens.select`),
not the daemon, and is a separate follow-up.

## Operability — config, start/stop, E2E (Phase D, #3813)

Phases A–C built the autonomous *engine* (work finder, dynamic concurrency,
main-health gate) as env-var-only surfaces. Phase D (#3813) adds the
operator-facing layer: a committed config surface, safe start/stop wrappers for
the raw daemon process, and a documented end-to-end acceptance playbook.

### Config surface (`.loom/config.json → autonomous`)

Autonomous mode can be enabled and tuned entirely from committed config — no env
vars required — so a repo can declare "this workspace runs autonomous mode with
concurrency ceiling 5" and share it with the team:

```json
{
  "autonomous": {
    "model": "sonnet",
    "perTokenConcurrency": 2,
    "workFinder": {
      "enabled": true,
      "intervalSecs": 60,
      "maxConcurrent": 5,
      "quarantine": {
        "enabled": true,
        "threshold": 3,
        "ttlSecs": 3600,
        "instaCrashSecs": 60
      }
    },
    "mainHealthGate": {
      "enabled": true
    },
    "dispatchStaggerMs": 2000,
    "watchdog": {
      "enabled": true,
      "timeoutSecs": 120,
      "intervalSecs": 30,
      "reviewStall": true,
      "reviewStallTimeoutSecs": 2700
    }
  }
}
```

**Precedence is `env var > config value > built-in default` for every knob.** An
operator env var still overrides the committed config for a single run
(`LOOM_WORK_FINDER=0 loom-daemon` disables the loop even if config enables it).
An **absent `autonomous` block is byte-for-byte identical to the pre-#3813
env-only behavior** — the config read soft-fails (missing file / malformed JSON /
missing block all resolve to "no config value → fall through to env/default"),
exactly like `main_health_gate::read_build_gate_config`.

| Config key | Env override | Default | Notes |
|------------|--------------|---------|-------|
| `autonomous.model` | *(per-dispatch `dispatch_sweep` `model` param)* | `sonnet` | Model pinned on **every** daemon-dispatched child (work-finder, epic supervisor, and `dispatch_sweep` when its `model` param is absent). See below (#3944) |
| `autonomous.workFinder.enabled` | `LOOM_WORK_FINDER` | `false` | Master on/off for the finder loop |
| `autonomous.workFinder.intervalSecs` | `LOOM_WORK_FINDER_INTERVAL_SECS` | `60` | Zero/invalid → default |
| `autonomous.workFinder.maxConcurrent` | `LOOM_WORK_FINDER_MAX_CONCURRENT` | `3` | Operator **ceiling**, not a fixed target |
| `autonomous.workFinder.quarantine.enabled` | `LOOM_WORK_FINDER_QUARANTINE` | `true` | Insta-crash quarantine on/off (#3939). A safety backstop — defaults on |
| `autonomous.workFinder.quarantine.threshold` | `LOOM_WORK_FINDER_QUARANTINE_THRESHOLD` | `3` | Consecutive insta-crashes before an issue is quarantined. Zero/invalid → default |
| `autonomous.workFinder.quarantine.ttlSecs` | `LOOM_WORK_FINDER_QUARANTINE_TTL_SECS` | `3600` | How long a quarantine entry persists before auto-release. Zero/invalid → default |
| `autonomous.workFinder.quarantine.instaCrashSecs` | `LOOM_WORK_FINDER_QUARANTINE_INSTA_CRASH_SECS` | `60` | Checkpoint-less death within this window of dispatch counts as an insta-crash. Zero/invalid → default |
| `autonomous.perTokenConcurrency` | `LOOM_PER_TOKEN_CONCURRENCY` | `2` | Concurrent sweeps **per healthy token** in the cap (#3947). Zero/invalid → default; clamped to a floor of 1 |
| `autonomous.mainHealthGate.enabled` | `LOOM_MAIN_HEALTH_GATE` | `false` | Gate loop on/off |
| `autonomous.dispatchStaggerMs` | `LOOM_SWEEP_DISPATCH_STAGGER_MS` | `2000` | Min gap between consecutive child spawns (#3887). `0` disables |
| `autonomous.watchdog.enabled` | `LOOM_SWEEP_WATCHDOG` | `true` | Startup watchdog on/off (#3887). Also the master switch for the tick — mid-build-death (#3895) + review-stall (#3910) run in the same task |
| `autonomous.watchdog.timeoutSecs` | `LOOM_SWEEP_WATCHDOG_TIMEOUT_SECS` | `120` | No-progress window before auto-restart |
| `autonomous.watchdog.intervalSecs` | `LOOM_SWEEP_WATCHDOG_INTERVAL_SECS` | `30` | Watchdog probe cadence (shared by all three backstops) |
| `autonomous.watchdog.reviewStall` | `LOOM_SWEEP_REVIEW_STALL` | `true` | Review-phase stall watchdog on/off (#3910) |
| `autonomous.watchdog.reviewStallTimeoutSecs` | `LOOM_SWEEP_REVIEW_STALL_TIMEOUT_SECS` | `2700` | Log-silence window before a hung Judge/Doctor sweep is re-dispatched |

**Autonomous dispatch model (`autonomous.model`, #3944).** A daemon-dispatched
child is a headless `claude -p "/loom:sweep N"` process. Without an explicit
`--model`, it inherits whatever model the operator last configured in their
**interactive** CLI — which on the v0.15.0 canary was a premium tier that meters
premium usage credits and hard-failed every spawn with "out of usage credits". To
stop an autonomous fleet from ever silently inheriting a premium interactive
default, the daemon now pins an explicit model on **every** auto-dispatch
(work-finder sweeps, epic-supervisor role/child dispatches) and on
`dispatch_sweep` when its `model` param is absent. The resolved model is chosen by
this precedence, highest first:

1. **`dispatch_sweep` `model` param** — an explicit per-dispatch request always
   wins (unchanged from #3477).
2. **`autonomous.model`** in `.loom/config.json` — the per-repo default for all
   autonomous dispatch.
3. **Shipped default `sonnet`** — a deliberately **non-premium** tier (fast +
   cost-appropriate for the bulk of build work). Never a premium tier.

Empty/whitespace values are treated as unset at every tier. The resolved model
and the tier that supplied it are named in the daemon dispatch log line
(`… model=<m> (source=param|config|default)`), and the model is forwarded to the
child via the existing `--model` plumbing (#3705). Set `autonomous.model` to
`opus` (or any valid model id) to raise the autonomous default per-repo.

**Startup-race mitigation (#3887).** Rapid back-to-back dispatch (the work
finder draining a backlog in one tick) could wedge some `claude` children at
startup in a 0-HTTPS MCP-init race: the sweep log showed only the spawn header,
no worktree was created, and the issue never left `loom:building`. Two layers
now guard against it: the **dispatch stagger** spaces consecutive child spawns
out of the simultaneous-startup window (prevention), and the **startup
watchdog** probes each running sweep for progress (worktree created / checkpoint
written / log output past the spawn header) and auto-cancels + re-dispatches —
**exactly once, bounded, never a loop** — any sweep hung with no progress past
`timeoutSecs`. Both the auto-cancel and the retry log loudly and reuse the
frozen `sweep.issue.{N}.exited` / `sweep.global.completed` / `sweep.global.dispatch`
topics (no new event topics). The watchdog defaults **on**; disable it with
`LOOM_SWEEP_WATCHDOG=0` or `autonomous.watchdog.enabled = false`.

**Review-phase stall watchdog (#3910).** The startup watchdog rescues a sweep
that shows *no* progress, and the mid-build-death watchdog (#3895) rescues one
that made progress then *died*. Neither covers a sweep that is **still alive**
but wedged in a hung role subagent — the observed failure was a `/loom:sweep`'s
internal Judge or Doctor `Task` running **49–66 minutes (multi-hour in the worst
cases) emitting zero output until the very end**, silently blocking the sweep's
back half with no self-heal. The third backstop, running in the same watchdog
tick, closes that gap: for each still-running daemon-dispatched sweep that has
already made startup progress, it measures **log silence** (how long the
per-sweep log file's mtime has gone un-advanced — a live sweep flushes tool
output continuously, a hung one does not) and, past `reviewStallTimeoutSecs`
(default 45 min), auto-cancels the wedged child and re-dispatches the issue
**exactly once, bounded, never a loop**. The re-dispatch resumes from the sweep
checkpoint, so the hung review phase is re-run — not the whole build. A second
stall resolves to give-up and surfaces on the frozen `sweep.issue.{N}.crashed`
topic (no new event topics). Gated to sweeps past startup so it never
double-acts with the startup watchdog on the same tick. Defaults **on**; disable
with `LOOM_SWEEP_REVIEW_STALL=0` or `autonomous.watchdog.reviewStall = false`.

> **Root cause & scope (#3910).** The stall is a *harness-side* artifact — a
> role subagent (`loom-judge`/`loom-doctor`) dispatched via the Claude Code
> `Task` tool that hangs on an opaque long-running / wedged tool call, producing
> no output until it eventually returns (or the sweep is killed). The
> **subagent-path** orchestrator (in-session `/loom:sweep`) cannot bound this
> from outside: it blocks awaiting each subagent's `TaskOutput` and the harness
> exposes no per-`Task` timeout or kill (see the "async-only dispatch" note in
> `sweep.md`), so the only in-session mitigation is prompt-level time-budget
> discipline in the role prompts. This watchdog is the **daemon-path** backstop
> — it works precisely because a daemon-dispatched sweep is an isolated OS
> process whose log file is observable and whose PID is cancelable, which the
> in-session `Task` is not.

The **gate's behavior** (which command runs against `main`, its timeout) still
comes from the separate top-level `buildGate` block (#3749); `autonomous.mainHealthGate`
is purely the on/off surface, so Phase C's already-tested `buildGate` semantics
are untouched. `LOOM_MAIN_HEALTH_GATE` remains the master override; the config
key just lets a repo turn the gate on without exporting an env var.

**Insta-crash quarantine (#3939).** The startup watchdog (#3887) and mid-build-death
watchdog (#3895) both rescue a sweep that made *some* observable progress before
dying. Neither covers the **insta-crash**: a child that dies within seconds of
spawn — e.g. a missing token pool or a selector import failure (#3938) — is
reaped, its `loom:building` claim restored to `loom:issue`, and the issue simply
re-qualifies on the very next work-finder tick. Left unchecked this occupies a
global concurrency slot forever and starves healthy candidates in other repos.
The reaper now counts **consecutive** insta-crashes per issue — a terminal
transition that wrote no phase checkpoint (never reached real work) and landed
within `instaCrashSecs` of dispatch. A death *with* a checkpoint, or a clean/slow
exit, resets the tally, so a genuine one-off failure never accretes toward
quarantine. After `threshold` consecutive insta-crashes the issue is
**quarantined**: the work finder skips it in-memory (no forge round-trip needed
for the load-bearing behavior) and, best-effort, flips the forge labels
(`loom:issue` → `loom:blocked`) with an explanatory comment so the pause is also
visible to a human. Quarantine is visible per-repo in `loom-daemon status`
(`quarantined (insta-crash, #3939): #123, #456`) and auto-releases after `ttlSecs`
— a transient breakage (e.g. a re-provisioned token pool) recovers without
operator action. To release a quarantine **immediately** (rather than waiting for
the TTL), run `loom-daemon quarantine clear <issue>`: it clears the daemon's
in-memory quarantine + insta-crash tally over IPC AND restores `loom:issue` on
the forge, so the issue re-qualifies on the next tick. **Note:** the in-memory
quarantine is the load-bearing state — manually flipping `loom:blocked` →
`loom:issue` on the forge alone does **not** release it (the work finder skips
the issue until the CLI clear or the TTL fires). In `tick_multi`, a quarantined
candidate is dropped **before** the global slot-fill pass, so a workspace whose
only candidates are quarantined never reserves a shared dispatch slot — healthy
sibling work in other repos gets it instead. Defaults **on**; disable with
`LOOM_WORK_FINDER_QUARANTINE=0` or `autonomous.workFinder.quarantine.enabled = false`.

### Prerequisite: a fresh token ranking (#3894, self-refreshed by the daemon since #3969)

**When you run autonomous mode against a multi-account token pool, keep
`.loom/tokens/.ranking` fresh.** The spawn-time selector (`loom_tools.tokens.select`)
is 3-tier — ranking → allowlist → random — and the ranking file is only
considered fresh for **10 minutes**. When it is absent or stale, tier-1 declines
and selection falls to the lower tiers. The work finder dispatches in bursts, so
a stale ranking means the daemon can steadily hand out accounts a recent probe
already flagged `exhausted`/`blocked`, whose sweeps then wedge at startup (spawn
header logged, no worktree, ~0% CPU) — the exact failure the startup watchdog
(#3887) then has to self-heal, one hang at a time.

As of #3969 the running `loom-daemon` **keeps this fresh itself** — see
[Token-ranking self-refresh](#token-ranking-self-refresh-3969) below. A
standalone operator cron (the historical requirement) is now an **optional
fallback** for setups that don't run the daemon at all (a bare `/loom:sweep`
subagent-dispatch install with no `loom-daemon` process); see that section for
the cron example. Two things keep a burst of issues from wedging on a stale
ranking regardless of which refresher is running:

- **A refresher running on a `<10`-min cadence** — the daemon's built-in loop
  (default 10 minutes, comfortably inside the freshness window) or an operator
  cron. One-shot before a run: `loom-tokens check --ranking`.

- **Stale-ranking fail-safe (selector-side, #3894).** Even without a fresh
  probe, a stale-but-present `.ranking` is no longer discarded. The selector
  treats its `exhausted`/`blocked` entries as an **advisory exclusion set** for
  the allowlist and random tiers, so it stops degrading to fully-random
  selection into known-exhausted accounts. If those exclusions would empty the
  pool (a stale "everything exhausted" ranking), selection retries ignoring them
  so a live pool never hard-fails on stale advice. This is a safety net, **not**
  a replacement for keeping the ranking fresh — a stale ranking still can't see
  an account that recovered.

### Token-ranking self-refresh (#3969)

The daemon runs its own periodic refresher for `.loom/tokens/.ranking` instead
of depending on an operator-managed cron for `probe-tokens.sh --ranking` — the
manual step documented above through #3894 is now automatic whenever
`loom-daemon` is running. It is the natural home for this loop: the daemon
already owns dispatch cadence and consumes the ranking (via `spawn-claude.sh`
selection) on every sweep it spawns.

**What it does.** On a configurable cadence (default 10 minutes) the loop
shells out to each registered repo's `probe-tokens.sh --ranking` (the same
script an operator cron would have called), which probes every bootstrapped
account for its current rate-limit headers and atomically rewrites `.ranking`
in whichever pool `loom_tools.tokens` resolves for that repo — the per-repo
`.loom/tokens/`, or the shared machine-level pool (#3938) when the per-repo pool
is absent/empty. Pool-location resolution is left entirely to the existing
Python selector; the daemon-side loop never reimplements it.

**Default-on, unlike the work finder / main-health gate.** Those two loops are
opt-in because they have dispatch-affecting side effects (spawning sweeps,
halting dispatch). This loop only ever reads rate-limit headers and rewrites a
bookkeeping file nothing else consults synchronously, so it ships on by
default — an absent refresher would silently regress every install back to the
stale-ranking failure mode #3894/#3969 exist to fix. It still has a full opt-out
knob for a repo that wants it off (e.g. no tokens bootstrapped at all):

```json
{
  "autonomous": {
    "tokenRankingRefresh": { "enabled": true, "intervalSecs": 600 }
  }
}
```

| Env var | Config key | Precedence | Default |
|---------|-----------|------------|---------|
| `LOOM_TOKEN_RANKING_REFRESH` | `autonomous.tokenRankingRefresh.enabled` | env > config > default | `true` (on) |
| `LOOM_TOKEN_RANKING_REFRESH_INTERVAL_SECS` | `autonomous.tokenRankingRefresh.intervalSecs` | env > config > default | `600` (10 min) |

**Never fatal, never double-writes unsafely.** A probe failure (network hiccup,
`gh`/`python3` missing, every account exhausted, no tokens bootstrapped at all)
is logged and skipped — it never panics the loop or the daemon. Because
`loom-tokens check --ranking` writes `.ranking` atomically (temp file +
rename), an operator's cron running the identical script concurrently is
harmless: the two refreshers can race to *schedule* a write but never to a
*torn* file, so keeping an existing cron alongside the daemon costs nothing but
a redundant API call.

**Multi-workspace.** Like the work finder / main-health gate, the loop re-reads
the workspace registry each tick and refreshes every registered repo's own
pool, gated by that repo's own config (an empty registry reduces to the single
daemon workspace). See `loom-daemon/src/token_ranking_refresh.rs` for the
implementation.

### Safe start / stop (raw daemon process)

`.loom/bin/loom start|stop` manage the **tmux Manual-Orchestration-Mode pool** —
a different process model from the `loom-daemon` binary that hosts the
work-finder / health-gate loops. Two dedicated wrappers manage the raw daemon
process:

```bash
# Plain start = FLAGS-OFF reliability daemon: BOTH autonomous loops OFF, no
# auto-dispatch. This is the safe default (#3911), consistent with the
# ecosystem-wide opt-in / default-off contract (LOOM_WORK_FINDER unset => off,
# LOOM_MAIN_HEALTH_GATE unset => off, precedence env > config > default):
./.loom/scripts/cli/loom-daemon-start.sh

# Opt in to autonomous loops explicitly:
./.loom/scripts/cli/loom-daemon-start.sh --work-finder   # work finder on
./.loom/scripts/cli/loom-daemon-start.sh --health-gate   # main-health gate on
./.loom/scripts/cli/loom-daemon-start.sh --work-finder --health-gate   # both on

# Enable strictly per .loom/config.json → autonomous (no env forcing):
./.loom/scripts/cli/loom-daemon-start.sh --from-config

# Explicit-off / foreground variants:
./.loom/scripts/cli/loom-daemon-start.sh --no-work-finder   # force finder off (explicit; same as default)
./.loom/scripts/cli/loom-daemon-start.sh --no-health-gate   # force gate off (explicit; same as default)
./.loom/scripts/cli/loom-daemon-start.sh --foreground       # run attached, no PID file

# Clean shutdown:
./.loom/scripts/cli/loom-daemon-stop.sh            # SIGTERM → grace → SIGKILL
./.loom/scripts/cli/loom-daemon-stop.sh --force    # immediate SIGKILL
```

`loom-daemon-start.sh`:
- **defaults FLAGS-OFF (#3911)**: a bare invocation exports `LOOM_WORK_FINDER=0`
  and `LOOM_MAIN_HEALTH_GATE=0`, so a plain start is a **reliability daemon** that
  does **not** auto-dispatch sweeps — consistent with the ecosystem-wide opt-in /
  default-off contract. An already-exported env var always wins; `--work-finder`
  / `--health-gate` force the respective loop on; `--from-config` leaves both
  unset so `.loom/config.json → autonomous` drives (precedence env > config >
  default),
- locates the `loom-daemon` binary (`LOOM_DAEMON_BIN` → `PATH` → `target/{release,debug}`),
- runs the **advisory** host-sleep check (`check-host-sleep.sh`, #3350) — never blocks the start,
- **on macOS, backgrounds the daemon as a `gui/<uid>` LaunchAgent** instead of a
  plain `nohup … &` (#3972 — see "macOS session-bootstrap hazard" below); on
  Linux it stays a plain nohup background job,
- backgrounds the daemon and writes a PID file at `.loom/.daemon.pid` (gitignored),
- refuses a second start when the PID file points at a live process, and surfaces
  the daemon's own **singleton-guard** refusal (#3806) — if the backgrounded
  process exits immediately it prints the startup-log tail instead of leaving a
  silently-dead process.

`loom-daemon-stop.sh` sends **SIGTERM** (not just Ctrl-C/SIGINT — the daemon now
handles both, #3813), waits `LOOM_DAEMON_STOP_GRACE_SECS` (default 10s), then
escalates to SIGKILL. On macOS it additionally `launchctl bootout`s the
LaunchAgent job definition once the process is confirmed dead (see below).

**Shutdown decision — sweeps survive, they are not drained.** A clean daemon stop
removes the Unix socket and exits, but **does not cancel in-flight `/loom:sweep`
children**. Those are independent detached processes that survive a daemon
restart by design — killing the dispatcher must not kill dispatched work — and
the registry reconciles their state on the next start (`SweepRegistry::reconstruct`
re-admits live-lock owners). To actively cancel a sweep, use
`mcp__loom__cancel_sweep` against a running daemon *before* stopping it.

### macOS session-bootstrap hazard (#3972)

**Incident (2026-07-26).** The daemon was started at 21:48 via
`loom-daemon-start.sh` from inside a Claude Code session. That session crashed
at 02:49. From the very next work-finder tick (02:50:21), **every** `gh` call
in the daemon's process tree failed with `tls: failed to verify certificate:
x509: OSStatus -26276` and `git fetch` failed with `No user exists for uid 501`
— while the identical commands worked perfectly from any fresh shell. The
daemon ran blind for ~35 minutes: the work finder saw 0 issues (4 errors/tick),
the main-health gate went RED on purely environmental failures, and in-flight
sweep children couldn't reach the forge either.

**Root cause.** The pre-#3972 `loom-daemon-start.sh` detached the process with
a plain `nohup "$DAEMON_BIN" … &`. `nohup` makes the process immune to
`SIGHUP`, but it does **not** move the process out of the *launching session's*
Mach bootstrap namespace — the process stays registered under whichever
terminal/Claude-Code-session/SSH-connection Mach service happened to spawn it.
When that session's context is torn down, XPC lookups the daemon (and every
child it spawns) depend on start failing with no crash and no obvious log
signal:

- **`trustd`** (certificate verification) — the underlying cause of the `gh`
  TLS/OSStatus errors, since Go's Darwin certificate verifier round-trips
  through `trustd` via XPC.
- **`opendirectoryd`** (`getpwuid`) — the underlying cause of git's
  "No user exists for uid N", since `git` resolves the current user via
  `getpwuid()`, which is backed by `opendirectoryd` on macOS.

This is why **"start it from a terminal that might die" is unsafe on macOS**.
Linux does not have this failure mode — a systemd user session (or a plain
`nohup` under `init`) does not tie a background process's XPC-equivalent
identity to the shell that spawned it.

**Fix.** `loom-daemon-start.sh` now generates a `launchd` LaunchAgent plist and
loads it with `launchctl bootstrap gui/<uid>` instead of `nohup`-backgrounding
in-process (Darwin only — Linux is unaffected and keeps the plain nohup path).
This was validated during the incident itself: relaunching the identical
binary as a launchd agent immediately restored `gh`/`git` — the first tick
after migration reported `13 seen, 3 dispatched, 0 error(s)`.

- **Plist location & label**: `~/Library/LaunchAgents/com.rjwalters.loom-daemon.plist`
  (override the label with `LOOM_LAUNCHD_LABEL`). Regenerated and reloaded
  (`bootout` the old definition, then a fresh `bootstrap` + `kickstart -k`) on
  **every** start, so a later invocation's flags/env always win over a stale
  loaded definition.
- **Environment forwarding**: the plist's `PATH` is the current `PATH` plus a
  fallback set (`~/.local/bin`, `~/.cargo/bin`, Homebrew, standard bin dirs) so
  `gh`, `git`, `cargo`, and `python3` resolve even inside launchd's minimal
  environment. Every already-exported `LOOM_*` / `GH_TOKEN` / `GITEA_TOKEN` /
  `FORGE_TOKEN` var is forwarded verbatim, so the FLAGS-OFF / `--work-finder` /
  `--health-gate` / `--from-config` semantics are preserved exactly — the
  launchd job never sees a wider or narrower autonomy configuration than a
  plain nohup start would have resolved.
- **`RunAtLoad=true` / `KeepAlive=false`**: mirrors the hand-written plist that
  validated this fix during the incident. `RunAtLoad=true` means the daemon
  also survives a reboot/re-login, not just the death of one particular
  session — strictly *more* durable than the pre-#3972 nohup contract (which
  didn't survive a reboot either). `KeepAlive=false` means launchd does **not**
  auto-respawn a crashed daemon; that responsibility stays with the reaper /
  operator, unchanged from before. `loom-daemon-stop.sh` `bootout`s the loaded
  definition after confirming the process is dead, specifically so an explicit
  stop is honored at the next login too (otherwise `RunAtLoad=true` would
  silently relaunch it).
- **Escape hatch**: `--no-launchd` (or `LOOM_DAEMON_LAUNCHD=0`) forces the
  legacy nohup path even on Darwin — e.g. for a sandboxed/headless macOS runner
  with no GUI login session where `gui/<uid>` may not resolve.
- **Inspection without side effects**: `--print-plist` renders the exact plist
  XML this invocation would install and exits — no `launchctl` call, no file
  write to `~/Library/LaunchAgents`. Useful for auditing exactly what
  environment/flags a given invocation would forward.
- **Linux**: unaffected — `nohup` stays the mechanism, since a systemd user
  session doesn't tie process identity to the spawning shell the way macOS's
  Mach bootstrap does. Operators who want equivalent extra hardening on Linux
  (e.g. surviving a `systemd --user` session teardown in an unusual setup) can
  optionally wrap the start in `systemd-run --user --scope
  ./.loom/scripts/cli/loom-daemon-start.sh` — not required for the documented
  failure mode, since it doesn't reproduce on Linux, but available as a
  belt-and-suspenders option.

### macOS TCC hygiene under launchd (#3980)

**Why launchd changed the TCC picture.** Under the pre-#3972 nohup model, the
daemon inherited whatever TCC (Transparency, Consent, and Control) grants the
launching terminal app already had — so folder-access prompts, if any, belonged
to Terminal.app/iTerm/Claude Code, not to `loom-daemon`. As a `gui/<uid>`
LaunchAgent (see above), the daemon is its **own** TCC-responsible process:
any touch of a protected location (`~/Desktop`, `~/Documents`, `~/Downloads`,
`~/Pictures` (Photos), `~/Music` (Media & Apple Music),
`~/Library/Mobile Documents` (iCloud Drive), network/removable volumes, …) by
the daemon **or any sweep child it spawns** now prompts fresh, once per
protected category. One operator report saw ~10 prompts in a single session,
including Photos / Media & Apple Music / iCloud — evidence of something
enumerating the top level of `$HOME` itself rather than touching those folders
individually (macOS bundles the per-category checks into one burst when a
process lists `$HOME`'s immediate contents).

**The daemon's legitimate working set never needs a protected folder.** Audited
surfaces — the daemon core (Rust) and `.loom/scripts/*` — only ever touch
`~/GitHub/*` (or wherever a workspace lives), `~/.loom`, `~/.claude*`, and
`/private/tmp`; disk-headroom checks use `df -Pk <workspace>`, not a directory
walk. `defaults/scripts/claude-wrapper.sh`'s crash-recovery path
(`recover_cwd()`, used when a worktree is deleted out from under a running
sweep child, e.g. by `loom-clean` or `merge-pr.sh`) previously fell back to
`cd "$HOME"` as a last resort before `/tmp` — landing a respawned `claude`
child in `$HOME` risked exactly the kind of out-of-bounds enumeration described
above. Fixed in #3980: both the last-resort `cd` and the script's initial
`WORKSPACE` fallback (when `pwd` fails at wrapper startup) now go straight to
`/tmp`, which is on the TCC-safe allowlist and serves the same "always exists,
always cd-able" purpose. `$HOME` is no longer a reachable recovery target
anywhere in the wrapper.

**Sweep children's working-set contract.** Every `/loom:sweep`-dispatched
child (Curator/Builder/Judge/Doctor/Champion subagents, and any test suite or
tool subprocess they invoke) is expected to stay within: the workspace root
it was dispatched into, `.loom/` (worktrees, logs, tokens, checkpoints),
`.claude*` config dirs, and `$TMPDIR`/`/private/tmp` scratch space. Recursive
scans that escape this contract — `find ~`, `du -sh ~`, `grep -r` rooted at
`$HOME`, a script that `cd`'d to the wrong place before globbing, a test suite
that writes fixtures to `~/Documents` instead of a tmpdir, or a tool that
resolves an iCloud-synced path — are **out-of-scope defects**, not ambient
behavior to route around with a broader macOS grant. If you write a role
prompt, hook, or test fixture, scope its filesystem footprint to this
contract explicitly rather than relying on `$HOME`-relative expansion.

**What to click when macOS prompts.** **Deny is always safe.** The daemon's
and sweep children's legitimate working set contains no protected folder, so a
genuine prompt means something reached outside the contract — denying it may
surface a sweep-child failure (a file-not-found / permission-denied on the
out-of-bounds path), which is the **diagnostic signal**, not a bug to route
around. Use that failure to identify and fix the offending script/tool per the
contract above, the same way any other out-of-scope access would be fixed.

**Why Full Disk Access is never the right answer.** FDA (or per-category
Allow) is not the fix even as a convenience, for two independent reasons: (1)
it papers over a real out-of-scope access instead of fixing it, and (2) it
doesn't survive the deployment model. TCC identity is keyed to the binary's
code signature (or, for an ad-hoc/unsigned binary, its cdhash), and
`loom-daemon` is rebuilt from source on every `loom-daemon-update.sh` self-update
roll (#3968) — each rebuild produces a **new** cdhash, so any grant attached to
the previous build silently evaporates. Chasing that with FDA produces a
recurring popup storm *and* a standing over-grant that provides no lasting
benefit. If a grant is ever clicked by mistake, walk it back at System
Settings → Privacy & Security → \<category\> (Photos / Media & Apple Music /
Files and Folders / …) → remove `loom-daemon` — the next self-update rebuild
would have revoked it anyway via the cdhash change, so removing it manually
just does that sooner.

**Ad-hoc code signing (follow-up, not in #3980's scope).** A stable
ad-hoc signature (`codesign -s - --identifier com.rjwalters.loom-daemon`,
applied at build time) would let an *intentional*, narrowly-scoped future TCC
grant survive a rebuild that doesn't change the binary's identifier — useful
if a legitimate future daemon capability needs one specific protected
category. Wiring that into the `cargo build --release` / `loom-daemon-update.sh`
provisioning path is real but separable infrastructure work (build-script
changes, verifying `codesign` behavior across the self-update rebuild path,
deciding whether the identifier needs to be stable across machines) and is
intentionally left as a follow-up (#4016) rather than bundled into this
issue's audit + working-set-contract + crash-recovery fix.

### Self-update (rebuild + provision + restart, #3968)

The daemon's self-repair loop can file **and fix** its own defects — proven
during the 2026-07-25/26 canary rollout, which produced 16 self-filed daemon
fixes — but every merged fix historically only took effect after an operator
manually rebuilt the Rust binary, reprovisioned it, and restarted the process.
`loom-daemon-update.sh` is the single operator command that closes that gap:

```bash
./.loom/scripts/cli/loom-daemon-update.sh              # detect, rebuild if stale, provision, restart (preserving flags)
./.loom/scripts/cli/loom-daemon-update.sh --check       # detect only; exit 0 (up to date) / 3 (update available); no writes
./.loom/scripts/cli/loom-daemon-update.sh --dry-run     # print the plan without building/provisioning/restarting
./.loom/scripts/cli/loom-daemon-update.sh --force       # rebuild + provision + restart even if already up to date
./.loom/scripts/cli/loom-daemon-update.sh --no-restart  # rebuild + provision only; leave the running daemon untouched
```

**Staleness detection** is primary-local, zero-network: it compares the git
commit **baked into** the currently-resolved `loom-daemon` binary (embedded at
build time via `build.rs` → `LOOM_DAEMON_GIT_COMMIT`, the same value folded
into `loom-daemon --version`) against the **local source tree's** current
`HEAD` short commit — directly answering "would rebuilding right now produce a
different binary?". Separately, and purely **advisory** (it never gates the
rebuild decision), the script bounded-fetches `origin/<default-branch>` and
warns when local `HEAD` itself is behind, mirroring
`check-main-freshness.sh`'s pattern — so a cron-scheduled run distinguishes
"you're current with local HEAD" from "local HEAD is itself stale".

**Flag preservation (the FLAGS-OFF/opt-in contract, never widened)**:
`loom-daemon-start.sh` now persists its resolved invocation flags to
`.loom/.daemon.flags` (gitignored, one flag per line) on every start attempt —
`--foreground`/`--help` are filtered out (script-only, not autonomy state);
`--from-config`, `--work-finder`, `--health-gate`, `--no-work-finder`,
`--no-health-gate` are kept verbatim. `loom-daemon-update.sh` reads this file
and replays it **exactly** on restart — a daemon started bare (FLAGS-OFF)
restarts bare; a daemon started `--work-finder` restarts `--work-finder`,
never gaining `--health-gate` it didn't have. A missing flags file (a daemon
started before #3968, or manually) falls back to a bare FLAGS-OFF restart
rather than guessing, with a loud warning.

**A daemon that was NOT running is never started.** If `.loom/.daemon.pid`
has no live process at update time, the script rebuilds and provisions but
prints "was not running — nothing to restart" and stops — it never widens the
system state by starting autonomy (or anything) that wasn't already running.
Combined with the "in-flight sweeps survive a stop" shutdown decision above,
a rebuild-and-restart window never kills active dispatched work and never
silently upgrades a stopped daemon into a running one.

**Provisioning** targets wherever the resolved binary lives: an explicit
`LOOM_DAEMON_BIN` override is provisioned in place; otherwise the fresh binary
is installed to the machine-level location via
`scripts/install/provision-daemon.sh`'s `provision_machine_daemon` (default
`~/.local/bin/loom-daemon`, override `LOOM_DAEMON_BIN_DIR`) — the same
convention `loom-daemon-start.sh` already resolves through `command -v
loom-daemon`.

**Read-only "update available" surface (`loom-daemon --status`)**: separately
from the update script, `loom-daemon --status` / `--status --json` now prints
a purely local, read-only self-update line — the same built-commit-vs-source-HEAD
comparison, computed in-process (`self_update::check()`) with at most one `git
rev-parse` subprocess and zero network calls. It never triggers a rebuild or
restart on its own; it is advisory-only, matching the required "no auto-restart
without opt-in" contract. Example:

```
Self-update: built from ab12cd3 — UPDATE AVAILABLE (source checkout HEAD is de45f67); run `./.loom/scripts/cli/loom-daemon-update.sh` to rebuild + provision + restart
```

`loom-daemon-update.sh` requires an actual Loom source checkout
(`loom-daemon/Cargo.toml` must exist) — it rebuilds from source and refuses to
run against a binary-only / release-tarball install.

### End-to-end acceptance playbook

The goal state — "file a `loom:triage` issue, watch it build" with zero operator
dispatch — is validated by the E2E playbook at
[`docs/autonomous-mode-e2e.md`](../../docs/autonomous-mode-e2e.md): it walks a
throwaway issue from `loom:triage` → Curator → `loom:issue` → work-finder
dispatch → PR → merge, with a scripted label-transition assertion, and confirms
the operator only ever created the issue.

## Locks and lifecycle

Each dispatched sweep acquires a directory lock under
`.loom/locks/issue-<N>/` via `mkdir` (POSIX-atomic). The lock dir
contains an `owner.json` with the dispatching daemon PID and the sweep
ID. The reaper releases the lock when a child dies; `cancel_sweep`
releases it explicitly. On daemon startup, `SweepRegistry::reconstruct`
admits live-lock owners back into the registry and drops stale locks
whose owner PID is dead.

## What this page does NOT describe

The legacy schema and tuning advice that historically lived here — the
Python `daemon-state.json` schema, `MAX_SHEPHERDS`/`ISSUE_THRESHOLD`
tunables, work-generation cooldowns, `shepherd-N` pool sizing — described
a Python brain that no longer exists. **None of that exists post-v0.10.0.**

- The daemon **does not** generate work. Architect and Hermit cadence
  is out of scope and tracked under follow-up #3381.
- The daemon **does not maintain a shepherd-N pool**. Each issue
  detaches its own `claude -p "/loom:sweep N"` child; concurrency is
  bounded by the daemon's dispatch handling and is operator-controlled
  via separate `dispatch_sweep` MCP calls.
- The daemon **does not track** `pipeline_state`, `warnings`,
  `completed_issues`, or `last_*_trigger`. The forge is the source of
  truth for pipeline state.
- Support roles run as **cron-driven GitHub Actions workflows**, not as
  long-running daemon-managed processes. There is no `JUDGE_INTERVAL`
  or `CHAMPION_INTERVAL` to tune from daemon config.

The decision to delete rather than re-implement the legacy state file
is documented in `docs/migration/daemon-state-consumers.md` §"Conclusion:
what Phase 3 deletes vs preserves".

## Related resources

- **Architecture epic**: [#3449](https://github.com/rjwalters/loom/issues/3449)
  (rebuild of the daemon backend).
- **Phase A** (dispatch surface): #3452 / PR #3459.
- **Phase B** (event bus): #3453 / PR #3460.
- **Phase C** (monitoring + subscription tools): #3455.
- **Migration guide**:
  [`docs/migration/v0.10.0-shepherd-deprecation.md`](../../docs/migration/v0.10.0-shepherd-deprecation.md).
- **Source**:
  - [`loom-daemon/src/types.rs`](../../loom-daemon/src/types.rs) — IPC types.
  - [`loom-daemon/src/sweep_registry.rs`](../../loom-daemon/src/sweep_registry.rs) — registry + reaper.
  - [`loom-daemon/src/event_bus.rs`](../../loom-daemon/src/event_bus.rs) — pub/sub bus.
  - [`loom-daemon/src/ipc.rs`](../../loom-daemon/src/ipc.rs) — request dispatcher.
  - [`mcp-loom/src/tools/sweeps.ts`](../../mcp-loom/src/tools/sweeps.ts) — MCP tool definitions.
