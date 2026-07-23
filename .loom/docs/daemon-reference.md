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
| `sweep.issue.{N}.phase`   | Sweep child via `publish_event` | `{phase, pr_number?}` |
| `sweep.issue.{N}.blocker` | Sweep child                     | `{reason, label_added}` |
| `sweep.issue.{N}.exited`  | Daemon reaper (or `cancel_sweep`) | `{exit_code, duration_sec}` |
| `sweep.issue.{N}.crashed` | Daemon reaper                   | `{checkpoint_phase}` |
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

### `list_sweeps` (Phase A)

Return all tracked sweeps, optionally filtered by lifecycle state.
Terminal entries are garbage-collected ~1h after the transition.

Inputs:
- `state_filter` (optional) — one of `Pending`, `Running`, `Exited`,
  `Crashed`.

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

The wire shape is pinned by `sweep_info_schema_snapshot` in
`sweep_registry.rs` — a change to the JSON shape requires deliberate
test update.

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

### Dynamic concurrency scaling (Phase B, #3811)

The concurrency cap is **not** a fixed value resolved once at startup. Every
tick the finder recomputes

```
dynamic_cap = min(healthy-token count, disk headroom, configured ceiling)
```

from three live inputs, so pool/disk/backlog changes are honored without a
daemon restart:

| Input | Source | Bound it enforces |
|-------|--------|-------------------|
| **healthy-token count** | `available` accounts in `{workspace}/.loom/tokens/.ranking` (`capacity::token_axis_limit`), falling back to the `*.token` count (`tokens::token_pool_size`) when no ranking exists | never over-subscribe a rotated OAuth account, and never dispatch to an exhausted/blocked one (#3902) — one live sweep per **healthy** account |
| **disk headroom** | `floor(free_gb / LOOM_PER_WORKTREE_GB)` on the worktree-root volume (`disk_headroom::disk_headroom_limit`, a Rust port of `disk-headroom.sh` that shells to `df -Pk`) | never provision more worktrees than the scratch volume can hold |
| **configured ceiling** | `LOOM_WORK_FINDER_MAX_CONCURRENT` (repurposed from Phase A's fixed target into an operator ceiling) | hard operator upper bound regardless of pool/disk headroom |

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
dynamic policy above, not a fixed target), and `LOOM_PER_WORKTREE_GB` (default 2
— the per-worktree disk estimate the disk-headroom bound divides by). A zero or
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
    "workFinder": {
      "enabled": true,
      "intervalSecs": 60,
      "maxConcurrent": 5
    },
    "mainHealthGate": {
      "enabled": true
    },
    "dispatchStaggerMs": 2000,
    "watchdog": {
      "enabled": true,
      "timeoutSecs": 120,
      "intervalSecs": 30
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
| `autonomous.workFinder.enabled` | `LOOM_WORK_FINDER` | `false` | Master on/off for the finder loop |
| `autonomous.workFinder.intervalSecs` | `LOOM_WORK_FINDER_INTERVAL_SECS` | `60` | Zero/invalid → default |
| `autonomous.workFinder.maxConcurrent` | `LOOM_WORK_FINDER_MAX_CONCURRENT` | `3` | Operator **ceiling**, not a fixed target |
| `autonomous.mainHealthGate.enabled` | `LOOM_MAIN_HEALTH_GATE` | `false` | Gate loop on/off |
| `autonomous.dispatchStaggerMs` | `LOOM_SWEEP_DISPATCH_STAGGER_MS` | `2000` | Min gap between consecutive child spawns (#3887). `0` disables |
| `autonomous.watchdog.enabled` | `LOOM_SWEEP_WATCHDOG` | `true` | Startup watchdog on/off (#3887) |
| `autonomous.watchdog.timeoutSecs` | `LOOM_SWEEP_WATCHDOG_TIMEOUT_SECS` | `120` | No-progress window before auto-restart |
| `autonomous.watchdog.intervalSecs` | `LOOM_SWEEP_WATCHDOG_INTERVAL_SECS` | `30` | Watchdog probe cadence |

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

The **gate's behavior** (which command runs against `main`, its timeout) still
comes from the separate top-level `buildGate` block (#3749); `autonomous.mainHealthGate`
is purely the on/off surface, so Phase C's already-tested `buildGate` semantics
are untouched. `LOOM_MAIN_HEALTH_GATE` remains the master override; the config
key just lets a repo turn the gate on without exporting an env var.

### Prerequisite: a fresh token ranking (#3894)

**When you run autonomous mode against a multi-account token pool, keep
`.loom/tokens/.ranking` fresh — a periodic `probe-tokens.sh --ranking` is a
required part of the setup, not an optional nicety.** The spawn-time selector
(`loom_tools.tokens.select`) is 3-tier — ranking → allowlist → random — and the
ranking file is only considered fresh for **10 minutes**. When it is absent or
stale, tier-1 declines and selection falls to the lower tiers. The work finder
dispatches in bursts, so a stale ranking means the daemon can steadily hand out
accounts a recent probe already flagged `exhausted`/`blocked`, whose sweeps then
wedge at startup (spawn header logged, no worktree, ~0% CPU) — the exact failure
the startup watchdog (#3887) then has to self-heal, one hang at a time.

Two things now keep this from wedging a burst of issues:

- **Wire the probe on a `<10`-min cadence.** Add a cron entry so the ranking is
  always fresh under the daemon's dispatch rate:

  ```cron
  */5 * * * * cd /path/to/repo && ./.loom/scripts/probe-tokens.sh --ranking >> .loom/logs/probe-tokens.log 2>&1
  ```

  (Use `*/5`, comfortably inside the 10-minute freshness window, rather than the
  `*/10` boundary case from the single-key example.) One-shot before a run:
  `loom-tokens check --ranking`.

- **Stale-ranking fail-safe (selector-side, #3894).** Even without a fresh
  probe, a stale-but-present `.ranking` is no longer discarded. The selector
  treats its `exhausted`/`blocked` entries as an **advisory exclusion set** for
  the allowlist and random tiers, so it stops degrading to fully-random
  selection into known-exhausted accounts. If those exclusions would empty the
  pool (a stale "everything exhausted" ranking), selection retries ignoring them
  so a live pool never hard-fails on stale advice. This is a safety net, **not**
  a replacement for the probe cron — a stale ranking still can't see an account
  that recovered, so keep it fresh.

### Safe start / stop (raw daemon process)

`.loom/bin/loom start|stop` manage the **tmux Manual-Orchestration-Mode pool** —
a different process model from the `loom-daemon` binary that hosts the
work-finder / health-gate loops. Two dedicated wrappers manage the raw daemon
process:

```bash
# Bring up autonomous mode (work finder + health gate) as a backgrounded process:
./.loom/scripts/cli/loom-daemon-start.sh

# Enable strictly per .loom/config.json → autonomous (no env forcing):
./.loom/scripts/cli/loom-daemon-start.sh --from-config

# Selective / foreground variants:
./.loom/scripts/cli/loom-daemon-start.sh --no-work-finder   # gate only
./.loom/scripts/cli/loom-daemon-start.sh --no-health-gate   # finder only
./.loom/scripts/cli/loom-daemon-start.sh --foreground       # run attached, no PID file

# Clean shutdown:
./.loom/scripts/cli/loom-daemon-stop.sh            # SIGTERM → grace → SIGKILL
./.loom/scripts/cli/loom-daemon-stop.sh --force    # immediate SIGKILL
```

`loom-daemon-start.sh`:
- locates the `loom-daemon` binary (`LOOM_DAEMON_BIN` → `PATH` → `target/{release,debug}`),
- runs the **advisory** host-sleep check (`check-host-sleep.sh`, #3350) — never blocks the start,
- backgrounds the daemon and writes a PID file at `.loom/.daemon.pid` (gitignored),
- refuses a second start when the PID file points at a live process, and surfaces
  the daemon's own **singleton-guard** refusal (#3806) — if the backgrounded
  process exits immediately it prints the startup-log tail instead of leaving a
  silently-dead process.

`loom-daemon-stop.sh` sends **SIGTERM** (not just Ctrl-C/SIGINT — the daemon now
handles both, #3813), waits `LOOM_DAEMON_STOP_GRACE_SECS` (default 10s), then
escalates to SIGKILL.

**Shutdown decision — sweeps survive, they are not drained.** A clean daemon stop
removes the Unix socket and exits, but **does not cancel in-flight `/loom:sweep`
children**. Those are independent detached processes that survive a daemon
restart by design — killing the dispatcher must not kill dispatched work — and
the registry reconciles their state on the next start (`SweepRegistry::reconstruct`
re-admits live-lock owners). To actively cancel a sweep, use
`mcp__loom__cancel_sweep` against a running daemon *before* stopping it.

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
