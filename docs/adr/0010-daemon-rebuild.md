# ADR-0010: Rebuild Daemon Mode as Rust Binary with MCP-Tool Surface (v0.10.0)

## Status

Accepted

Partially reverses [ADR-0009](0009-shepherd-deprecation.md). The
shepherd brain and Python daemon brain remain deleted; daemon mode as
a *user-facing capability* is rebuilt at a different architectural
layer.

## Context

[ADR-0009](0009-shepherd-deprecation.md) deleted the Python shepherd
brain (`loom_tools/shepherd/`), the Python daemon brain
(`loom_tools/daemon_v2/`), the `/shepherd` slash command, and the
related shell wrappers. The replacement was three daemon-free
mechanisms: `/loom:sweep` for per-issue lifecycle (in-process subagent
dispatch), `./.loom/scripts/spawn-loop.sh` for multi-issue claiming
(shell script polling the forge), and `.github/workflows/loom-*.yml`
for periodic support roles (cron-driven, no persistent process).

That deletion was correct for the Python brains — they were ~21k LOC
of fragile coordination code duplicating what forge labels expressed
more simply. But the "no persistent process at all" stance turned out
to be an overcorrection. Three operational realities pushed back:

1. **Token rotation only works at process-spawn boundaries.** Claude
   Code subagents inherit the parent session's `CLAUDE_CODE_OAUTH_TOKEN`.
   For multi-day autonomous runs spreading load across multiple Pro/Max
   accounts, every sweep child must be a separate `claude -p` process
   with its own rotated token. The `/loom:sweep` skill (one Claude Code
   session driving a subagent dispatch tree) cannot rotate tokens. A
   separate dispatch surface is needed.

2. **Operators want named dispatch and observability, not a polling
   loop.** `spawn-loop.sh` (Phase 1 of epic #3372) reads the forge for
   `loom:issue` items every 30 seconds and detaches one `claude -p
   "/loom:sweep N"` child per ready issue. It works, but provides no
   way to dispatch a specific issue on demand, no way to observe a
   running sweep's lifecycle from outside the per-child log file, no
   way to cancel a stuck child, and no way for sweep children to
   coordinate with each other (e.g., Judge waiting on Builder's
   `loom:review-requested` label flip without polling the forge).

3. **The shell-level "preserved daemon mode" promised by ADR-0009 was
   never built.** ADR-0009's "Decision" section said:
   `./.loom/scripts/daemon.sh` "is preserved as a user-facing convenience
   wrapper that launches the spawn loop + GitHub Actions cron +
   token-rotated tmux panes." That `daemon.sh` wrapper does not exist
   on `main`. The documentation referenced it through v0.9.x, but
   `defaults/scripts/start-daemon.sh` and `stop-daemon.sh` were broken
   stubs `exec`ing a script that was never written. The "preserved
   daemon mode" promise was doc fiction.

The Rust `loom-daemon` binary (~20k LOC, originally built for terminal
lifecycle management per [ADR-0008](0008-tmux-daemon-architecture.md))
already existed and was already running on operator machines. It owned
the Unix socket IPC surface and the `mcp-loom` MCP server. Extending it
with sweep dispatch, an event bus, and monitoring tools costs ~2,000–2,500
LOC of net additions — not a new process, not a new persistent state
file, just three new capabilities (named dispatch, inter-agent
pub/sub, MCP-driven monitoring) layered onto the existing daemon.

This rebuild was proposed in epic #3449 and implemented in six phases.

## Decision

Rebuild daemon mode at the **MCP-tool level** by extending the
existing Rust `loom-daemon` binary with three new capabilities:

**1. Named sweep dispatch (Phase A, #3459)**

`mcp__loom__dispatch_sweep` enqueues a sweep for an issue. The daemon
fork-execs `claude -p "/loom:sweep N"` via `defaults/scripts/spawn-claude.sh`
(the existing token-rotation wrapper), registers the child pid in an
in-memory `SweepRegistry`, and returns the `sweep_id` to the caller.
`mcp__loom__list_sweeps` enumerates running sweeps. No forge polling
— dispatch is operator-driven (or skill-delegated via Stage -1
backend detection).

**2. Inter-agent pub/sub event bus (Phase B, #3460)**

`mcp__loom__publish_event` / `subscribe_to_events` provide a topic-routed
event channel over `tokio::sync::broadcast`. Six topics are frozen for
v0.10.0: `sweep.issue.{N}.phase`, `sweep.issue.{N}.blocker`,
`sweep.issue.{N}.exited`, `sweep.issue.{N}.crashed`,
`sweep.global.dispatch`, `sweep.global.completed`. Events are advisory
— the forge (GitHub labels, issue/PR state) remains the authoritative
source of coordination truth. Events accelerate inter-agent reaction
time; they do not replace forge state.

**3. MCP monitoring tools (Phase C, #3463)**

`mcp__loom__get_sweep_status`, `tail_sweep_log`, `cancel_sweep`,
`tail_event_bus` provide observability and control over running
sweeps. `cancel_sweep` implements SIGTERM → grace → SIGKILL with a
configurable grace window. `tail_event_bus` is a fire-hose view of all
events regardless of topic, for post-hoc debugging (per the curator's
risk note D on #3449).

**Backend detection** (`/loom:sweep` Stage -1, Phase D, #3462) probes
on every skill invocation whether the daemon is reachable AND a
multi-account pool exists. **Strict AND** — either probe failing falls
through to the existing in-process subagent dispatch path. This
guarantees no behaviour change for solo-token operators while making
the daemon path available to multi-account operators with zero
configuration on the skill side.

The v0.9.x spawn loop is **deprecated, not deleted** (Phase E, #3465).
It emits a stderr warning on every invocation but remains functional
through the v0.10.x cycle so downstream forks have one minor release of
migration runway (per the curator's risk note C on #3449). Deletion is
deferred to v0.11.0.

The shell-level `daemon.sh` wrapper that ADR-0009 promised is **not
rebuilt**. Operators run `loom-daemon` directly under their service
manager of choice (systemd, launchd, foreman, or a background shell).
The user-facing API is the MCP tools, not a shell command.

## Consequences

### Positive

- **Multi-account dispatch is restored.** The rebuilt daemon supports
  the long-running multi-account autonomous runs that motivated the
  original v0.9.x spawn loop, with a cleaner operator surface (named
  dispatch instead of forge polling).
- **Inter-agent coordination becomes possible.** The pub/sub bus lets
  Judge react to a Builder PR creation without polling the forge.
  Reaction time drops from "next 5-minute cron tick" to sub-second.
- **Operator observability improves.** `list_sweeps`,
  `get_sweep_status`, `tail_sweep_log`, `tail_event_bus`, and
  `subscribe_to_events` give a structured view of running sweeps that
  the spawn loop could not provide (it only wrote a `pid` and
  `started_at` to a state file).
- **Cancellation is supported.** `cancel_sweep` is the first
  first-class way to stop a stuck sweep without `pkill` or a manual
  signal cascade.
- **The doc-fiction gap is closed.** The Phase E rewrite of
  `CLAUDE.md` / `defaults/CLAUDE.md` / `defaults/roles/loom.md`
  removes references to the nonexistent `daemon.sh` and points to the
  actual MCP-tool surface.
- **No process count growth.** The capabilities are added to the
  existing `loom-daemon` binary, not a new process. Operators who
  already run `loom-daemon` for terminal management gain the sweep
  surface for free.
- **No on-disk daemon state file.** The sweep registry and event bus
  live in-memory only. The forge remains the authoritative source of
  coordination state. Restarting the daemon is safe — in-flight
  sweeps survive (they are detached children) and re-register
  themselves via the reaper on the first 30-second tick post-restart.

### Negative

- **Reputational risk: "didn't we just delete the daemon?"** The
  curator's risk note B on #3449 addressed this; the framing this ADR
  uses is: ADR-0009 deleted the *Python brain*, ADR-0010 adds three
  *new capabilities* (named dispatch, pub/sub, MCP monitoring) the
  prior brain never had. The architecture is genuinely different, not
  a reversal. But the framing requires operator attention to land
  correctly.
- **Two dispatch surfaces.** Operators now have two valid ways to
  drive a sweep: in-process subagent dispatch via `/loom:sweep`, and
  multi-process daemon dispatch via `mcp__loom__dispatch_sweep`.
  Stage -1 routes between them automatically, but operators reading
  the docs see both paths and may need to think about which one applies.
  The `/loom:sweep` skill (in either Mode A/B or daemon-delegated)
  remains the recommended entry point for most workflows.
- **LOC growth in the Rust daemon.** Phases A–C added ~2,000–2,500
  LOC to `loom-daemon/src/`. The daemon was already ~20k LOC; the
  growth is ~10–12%. Test coverage for the new modules
  (`sweep_registry`, `event_bus`, MCP surface) is maintained at the
  pre-rebuild bar.
- **The Phase A "preserved daemon mode" promise from ADR-0009 is now
  fulfilled at a different layer than originally claimed.** ADR-0009
  said `daemon.sh start` would be the preserved API. ADR-0010 says
  `mcp__loom__dispatch_sweep` is the preserved API. Operators who
  scripted against the original (never-built) `daemon.sh` surface
  must migrate to the MCP tools. This is documented in
  [`docs/migration/v0.10.0-daemon-rebuild.md`](../migration/v0.10.0-daemon-rebuild.md).

## Alternatives Considered

**Restore the Python daemon brain (`loom_tools/daemon_v2/`)**

Rejected. The Python brain's problems (single point of failure,
opaque internal state, coupled scheduling, ~21k LOC maintenance
burden) were architectural, not implementational. Restoring it would
restore the problems ADR-0009 fixed. The MCP-tool surface on the Rust
daemon avoids all five of ADR-0009's "Context" failure modes: no
hidden state file (in-memory only), no scheduling policy in the daemon
(operator-driven dispatch only), no role registration (the daemon does
not know what `/curator` is), no event-loop serialization (`tokio`
broadcast channels are concurrent), and ~10% of the LOC.

**Keep daemon-free; build `spawn-loop.sh` v2 with named dispatch**

Rejected. `spawn-loop.sh` is a polling loop — its primary mechanism is
"every 30 seconds, scan the forge for `loom:issue` items." Adding
named-dispatch on top requires either a second polling channel
(operator-written request files in `.loom/dispatch-queue/`) or a
network surface (Unix socket). Once you add the network surface, you
have a daemon — at which point the question becomes "do we want the
daemon to be 500 lines of shell or 2,000 lines of Rust?" The Rust
daemon already exists for terminal management; extending it is cheaper
than a parallel shell-daemon hybrid.

**Build the shell-level `daemon.sh` wrapper ADR-0009 promised**

Rejected. ADR-0009 said `daemon.sh` would "launch the spawn loop + GitHub
Actions cron + token-rotated tmux panes." But: (a) GitHub Actions cron
runs on GitHub's infrastructure, not via a local wrapper; (b)
token-rotated tmux panes are an operator preference, not a Loom
responsibility; and (c) `spawn-loop.sh` is being deprecated. The
wrapper would have been a thin convenience layer over capabilities
that mostly live elsewhere. The MCP surface is a more honest
abstraction: it owns dispatch, observability, and lifecycle, with no
pretence of also owning GH Actions schedules.

**Defer the rebuild to v1.0.0**

Rejected. The multi-account dispatch use case is the load-bearing
operational pattern for autonomous overnight runs against the
`loom:issue` queue. Deferring it would leave operators on
`spawn-loop.sh` indefinitely while the docs continued to refer to a
nonexistent `daemon.sh`. The doc-fiction gap (curator's risk note E on
#3449) was already actively misleading operators. Closing the gap
required either deleting the doc references entirely or building the
backend they referred to. Building the backend was cheaper than
reframing the entire "preserved daemon mode" narrative.

## References

- Original proposal: epic #3449 (daemon rebuild)
- Phase issues: #3459 (A), #3460 (B), #3463 (C), #3462 (D), #3465 (E), #3457 (F — this ADR)
- Stop-gap doc patch: #3458
- Migration guide: [`docs/migration/v0.10.0-daemon-rebuild.md`](../migration/v0.10.0-daemon-rebuild.md)
- Daemon surface reference: [`.loom/docs/daemon-reference.md`](../../.loom/docs/daemon-reference.md)
- `/loom:sweep` skill (Stage -1 backend detection): [`defaults/.claude/commands/loom/sweep.md`](../../defaults/.claude/commands/loom/sweep.md)
- Partially reversed: [ADR-0009: Shepherd Deprecation](0009-shepherd-deprecation.md)
- Related: [ADR-0008: tmux + Rust Daemon Architecture](0008-tmux-daemon-architecture.md) (the daemon binary this ADR extends)
- Related: [ADR-0006: Label-Based Workflow Coordination](0006-label-based-workflow-coordination.md) (forge-as-state-machine, foundational to both ADR-0009 and this one — the pub/sub bus does not displace forge labels as authoritative state)
