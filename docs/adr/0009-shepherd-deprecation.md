# ADR-0009: Deprecate and Delete Shepherd Brain and Python Daemon (Phase 3)

## Status

Accepted

## Context

Loom's original orchestration architecture concentrated intelligence in two Python packages:

- **`loom_tools/shepherd/`** (~16.8k LOC): the per-issue orchestrator ("shepherd brain"). Each issue's lifecycle (Curator → Builder → Judge → Doctor → Merge) was managed by a long-running shepherd process that held lifecycle state in memory and wrote progress to `.loom/progress/shepherd-<id>.json`.

- **`loom_tools/daemon_v2/`** (~4.7k LOC): the coordination daemon ("daemon brain"). A persistent Python process that polled the forge for `loom:issue` items, spawned shepherd children, ran periodic support roles (Judge, Curator, Champion, Auditor, Guide) in-process on a timer loop, and maintained a `daemon-state.json` file as its source of truth.

This concentrated architecture created compounding problems:

1. **Single point of failure**: if the daemon process crashed or got confused, all orchestration stopped. Recovery required manual intervention to clean up the daemon state file and restart.

2. **Opaque internal state**: the daemon held significant state in memory and in `daemon-state.json`. Debugging required understanding the daemon's internal data model; the forge (GitHub labels, issue/PR state) was not sufficient to reconstruct what the daemon believed was happening.

3. **Coupling**: the daemon owned the scheduling policy for all roles. Changing a role's cadence required daemon changes. Adding a new role required daemon registration. Worker roles were not independently runnable.

4. **Process-level concurrency limits**: the daemon serialized role dispatching through a single Python event loop. Shepherd children ran as subprocesses but were tracked by the daemon, creating a dependency between child lifecycle and daemon health.

5. **Maintenance burden**: ~21k LOC of Python orchestration code that had to be kept consistent with the role markdown files, the forge API, and the Claude CLI invocation patterns. The code duplicated coordination logic that could be expressed entirely through forge labels and standard shell scripts.

The shepherd/daemon deprecation was proposed in #3317, refined and tracked as epic #3372, and implemented in a phased sequence documented in `docs/migration/daemon-state-consumers.md`.

## Decision

Delete `loom_tools/shepherd/`, `loom_tools/daemon_v2/`, the `/shepherd` slash command, and all related shell scripts and skill files. Replace the orchestration responsibilities with three daemon-free mechanisms:

**1. Per-issue lifecycle: `/loom:sweep <issue>`**

A Claude Code slash command that runs the full Curator → Builder → Judge → Doctor → Merge pipeline for one issue as a single-session process. State is checkpointed to `.loom/sweep-checkpoint/issue-<N>.json` for crash recovery. No persistent process. No daemon state file. The forge is the source of truth.

**2. Multi-issue claiming: `./.loom/scripts/spawn-loop.sh`**

A minimal shell script that polls `loom:issue`, atomically claims ready issues using a `mkdir`-based lock, and detaches one `claude -p "/loom:sweep N"` child per issue. Concurrency is bounded by `MAX_PARALLEL`. The spawn loop has no work-generation logic, no support-role triggers, and no pipeline state beyond a list of currently-running PIDs.

**3. Periodic support roles: `.github/workflows/loom-*.yml`**

GitHub Actions workflows that run the periodic support roles (Judge, Curator, Champion, Auditor, Guide) on cron schedules as fresh `claude -p "/<role>"` invocations. No Loom-side state file. No long-running process. Each tick is independent and idempotent.

The shell-level daemon surface (`./.loom/scripts/daemon.sh`) is preserved as a user-facing convenience wrapper that launches the spawn loop + GitHub Actions cron + token-rotated tmux panes. The Python `loom-daemon` CLI entry point is removed.

### Phase sequencing (PRs 3.1.1–3.7)

The deletion was staged to minimize risk and allow incremental validation:

| PR | What was deleted |
|----|-----------------|
| 3.1.1–3.1.9 | Nine targeted shepherd/daemon subsystem removals |
| 3.2 | Python daemon brain (`daemon_v2/`) deletion |
| 3.3 | Shepherd brain (`shepherd/`) deletion |
| 3.4 | `daemon-state.json` fallback paths from ported CLIs |
| 3.5 | Remaining `/shepherd` skill file and related shell scripts |
| 3.6 | Mechanical docs search-and-replace (guides, quickstarts) |
| 3.7 | Architecture narrative rewrites + this ADR (final Phase 3 PR) |

## Consequences

### Positive

- **Simpler architecture**: ~21k LOC of Python orchestration deleted. The coordination logic is now ~500 lines of shell script plus forge labels.
- **Transparent state**: all orchestration state is visible in the forge (labels, issue/PR status, comments). No hidden daemon state to debug.
- **Independent evolution**: worker roles, sweep lifecycle, spawn loop, and GH Actions schedules can change independently. Adding a new role is a markdown file + optional GH Actions workflow.
- **Fault tolerance**: a crashed sweep child restarts from its checkpoint; other sweeps are unaffected; the spawn loop re-queues on the next tick.
- **No persistent process required**: operators can run Loom without a long-lived daemon. Single-issue sweeps (`/loom:sweep <N>`) work without the spawn loop; the spawn loop works without GH Actions workflows; GH Actions workflows work without the spawn loop.
- **Multi-account scalability**: each `/loom:sweep` child picks its own OAuth token via `spawn-claude.sh`. Token rotation is handled at spawn time, not inside a shared daemon process.

### Negative

- **Breaking change for downstream automation**: any script or sphere install that invokes `loom-shepherd`, `loom-daemon` (Python CLI), or the `/shepherd` slash command must migrate to `/loom:sweep`. A migration guide is provided at `docs/migration/v0.10.0-shepherd-deprecation.md`.
- **No preserve-compat shim**: unlike some deprecation paths, no compatibility shim was provided for the deleted Python entry points. The per-CLI replacement table in the migration guide covers all known invocation patterns.
- **Architect/Hermit cadence**: the daemon's work-generation triggers (Architect and Hermit on a timer) are not yet replicated in GH Actions workflows. This is tracked as follow-up #3381 (Phase 2d). Until that ships, Architect and Hermit require manual invocation.

## Alternatives Considered

**Preserve the daemon brain, add the spawn loop alongside it**

Rejected: running both the daemon brain and the spawn loop creates a split-brain scenario where two coordinators compete for `loom:issue` items. The daemon's internal state would drift from the forge state managed by sweep checkpoints. Complexity would increase, not decrease.

**Rewrite the daemon brain in a more maintainable language (Rust, TypeScript)**

Rejected: the core problem was not the Python implementation but the architectural pattern of a persistent stateful coordinator. Rewriting in another language would preserve the fragility and opacity. The shell + forge-as-state approach solves the root cause.

**Keep the shepherd brain, replace only the daemon brain**

Considered during Phase 1 scoping. Rejected: the shepherd brain (~16.8k LOC) and daemon brain (~4.7k LOC) were tightly coupled. The shepherd's per-issue state management duplicated what sweep checkpoints provide more simply. Keeping shepherd would have required maintaining the per-issue state protocol alongside the sweep protocol.

**Gradual migration with a compatibility shim**

The soft-deprecation warnings (Phase 2b, #3376) provided a warning window. A full compat shim was considered but rejected: the two orchestration models (daemon-state-centric vs forge-state-centric) are architecturally incompatible, not just interface-incompatible. A shim would mislead operators into thinking the old behavior was preserved when the internal model had fundamentally changed.

## References

- Original proposal: #3317 (closed — superseded by #3372)
- Refining epic: #3372 (shepherd/daemon deprecation)
- Phase 3 meta tracker: #3378
- Phase inventory and rationale: `docs/migration/daemon-state-consumers.md`
- Migration guide: `docs/migration/v0.10.0-shepherd-deprecation.md`
- Related ADRs: [ADR-0008: tmux + Rust Daemon Architecture](0008-tmux-daemon-architecture.md) (Rust daemon preserved; Python brain deleted)
- Related ADRs: [ADR-0006: Label-Based Workflow Coordination](0006-label-based-workflow-coordination.md) (forge-as-state-machine, foundational to this decision)
