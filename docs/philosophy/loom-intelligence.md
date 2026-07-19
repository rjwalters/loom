# Loom Intelligence: Emergent Coordination

*How a composition of simple, stateless components produces intelligent development behavior*

---

## The Core Insight

Loom's "intelligence" does not live in a single brain. There is no central planner, no persistent daemon loop that holds state and makes decisions. Instead, intelligence emerges from the composition of four simple, independently-replaceable layers:

```
GitHub Actions cron schedules   — periodic support roles (Judge, Curator, Champion…)
           +
  loom-daemon dispatch           — operator-driven /loom:sweep dispatch (mcp__loom__dispatch_sweep)
           +
  /loom:sweep <issue>            — single-issue lifecycle (Curator → Builder → Judge → Doctor → Merge)
           +
  Worker Roles                   — focused task executors (Builder, Judge, Doctor…)
```

The forge (GitHub or Gitea) is the shared state and coordination layer. Labels are the state machine. No component needs to talk to another component directly — they all read and write the same forge API.

This is the architecture that replaced the Python daemon brain (`loom_tools/daemon_v2/`) and the shepherd orchestrator (`loom_tools/shepherd/`) in v0.10.0. See [ADR-0009](../adr/0009-shepherd-deprecation.md) for the decision record.

---

## How Intelligence Emerges

### Layer 1: Worker Roles (Task Execution)

Each worker role — Builder, Judge, Curator, Doctor, Architect, Hermit, Champion, Guide, Auditor — is a focused prompt that performs one kind of work. Workers are stateless: they read the forge, do work, write back to the forge, and exit.

Workers don't coordinate with each other. Coordination is entirely mediated by labels.

**Intelligence at this layer**: domain expertise encoded in role prompts. A Builder knows how to claim an issue, create a worktree, implement a feature, write tests, and open a PR. A Judge knows how to evaluate code quality. Neither needs to know the other exists.

### Layer 2: /loom:sweep (Single-Issue Lifecycle)

`/loom:sweep <issue>` runs the full Curator → Builder → Judge → Doctor → Merge pipeline for one issue. It dispatches each phase to the appropriate worker role in sequence, checks the forge state between phases, and handles failures (Doctor fixes, re-Judge after Doctor, etc.).

Sweep is also stateless between issues. Its only persistent state is a checkpoint file (`.loom/sweep-checkpoint/issue-<N>.json`) that allows crash recovery within a single issue's lifecycle.

**Intelligence at this layer**: lifecycle orchestration. Sweep knows the correct order of operations, when to involve Doctor, and when to call Merge. This is the "workflow" layer — not a brain, but a well-defined protocol.

### Layer 3: loom-daemon Dispatch (Multi-Issue Batching)

The Rust `loom-daemon` binary dispatches sweeps on demand. An operator (or MCP client) enqueues work with `mcp__loom__dispatch_sweep --issue N`, and the daemon detaches one `claude -p "/loom:sweep N"` child per issue with multi-account token rotation via `spawn-claude.sh`. It holds the sweep registry, event bus, and reaper task in memory.

The daemon has no work-generation logic, no support-role triggers, no pipeline state, and it does not poll the forge — dispatch is operator-driven. It is intentionally minimal; the forge is the source of truth for everything else. (The v0.9.x `spawn-loop.sh` polling launcher and its `.loom/spawn-loop-state.json` state file were removed in v0.11.0.)

**Intelligence at this layer**: parallelism and resource management. The daemon turns operator-enqueued approved issues into concurrent, isolated, self-contained sweeps. Multi-account token rotation (`spawn-claude.sh`) distributes load across Claude OAuth accounts.

### Layer 4: GitHub Actions Cron (Periodic Support Roles)

`.github/workflows/loom-*.yml` run the periodic support roles — Judge (5 min), Curator (5 min), Champion (10 min), Auditor (10 min), Guide (15 min) — on cron schedules as fresh `claude -p "/<role>"` invocations. No Loom-side state file. No long-running process. Each tick is independent.

**Intelligence at this layer**: continuous maintenance. Even when no human is present, the forge state is kept healthy: new issues get curated, open PRs get reviewed, approved PRs get merged, and stale items get flagged.

---

## The Forge as Shared Brain

The forge (GitHub or Gitea) is not just a code host — it is the external memory that makes all of this work without a central coordinator:

- **Issues** record approved work items with full context (curator notes, acceptance criteria)
- **Labels** encode the workflow state machine (`loom:issue` → `loom:building` → PR → `loom:pr` → merged)
- **Pull Requests** record completed work, review feedback, and merge decisions
- **Comments** preserve institutional memory — why a decision was made, what was tried

Every component reads from and writes to this shared state. No component needs to ask another "what is the current state?" — it just queries the forge. This is why the system is resilient to crashes, restarts, and multi-account concurrency: the forge is always consistent.

---

## Why This Architecture is Smarter Than a Central Brain

The previous architecture concentrated intelligence in a Python daemon brain (`daemon_v2/`) and shepherd orchestrator (`shepherd/`). That concentration created fragility:

- **Single point of failure**: if the daemon crashed or got confused, everything stopped
- **Opaque state**: the daemon's internal state was hard to inspect and debug
- **Coupling**: changing the daemon affected all workflows; changing a worker role required daemon updates
- **Scalability limits**: the daemon serialized work through a single Python process

The emergent architecture distributes intelligence:

- **Fault tolerance**: a crashed sweep child restarts from its checkpoint; the spawn loop re-queues it on the next tick; other sweeps are unaffected
- **Transparent state**: all state is visible in the forge — labels, comments, PR status; nothing is hidden in memory
- **Independent evolution**: worker roles, sweep, spawn loop, and GH Actions workflows can change independently
- **Horizontal scalability**: parallel sweeps run as separate processes; multi-account token rotation distributes load

---

## The Learning Dimension

The forge accumulates a rich history of every issue's lifecycle: curator notes, builder approaches, judge feedback, doctor fixes, merge decisions. This history is machine-readable.

Future Loom intelligence layers can analyze this history to:

- Identify which prompt patterns correlate with faster PR approval
- Detect common failure modes (builds that break after certain file patterns change)
- Recommend issue decompositions based on historical cycle time
- Surface agent effectiveness metrics (see `./.loom/scripts/agent-metrics.sh`)

The key difference from the daemon era: this analysis layer reads from the forge, not from a proprietary daemon state file. Any tool that can query the GitHub/Gitea API can participate. The intelligence is not locked in Loom's process.

---

## Alignment with Loom's Philosophy

From [working-with-ai.md](working-with-ai.md):

> "To get humans out of the debug loop, you need to create a surface that AI agents can read directly."

The forge IS that surface. Labels, issues, and PRs are machine-readable coordination primitives. Agents don't need a special protocol or a shared daemon — they read the same forge that humans read.

> "Software development is transitioning from: 'Can I write this code fast enough?' To: 'Can I specify what I actually want clearly enough?'"

Loom's architecture embodies this: the work of specifying (Curator, Architect), building (Builder), and reviewing (Judge, Doctor) is distributed across specialized roles. The coordination overhead is handled by the forge's state machine. Human judgment enters at the right moments — proposal approval, merge decisions, blocked-issue triage — without requiring humans to manage the pipeline mechanics.

---

**See Also:**
- [Agent Archetypes](agent-archetypes.md) — the roles that compose this architecture
- [ADR-0009: Deprecate and Delete Shepherd Brain and Python Daemon](../adr/0009-shepherd-deprecation.md) — the architectural decision record
- [Working with AI](working-with-ai.md) — the philosophy that motivates this design
- [CLAUDE.md](../../CLAUDE.md) — operational guide for running Loom
