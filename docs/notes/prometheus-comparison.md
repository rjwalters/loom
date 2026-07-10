# What Loom Can Learn From EuniAI/Prometheus

**Type**: Research / comparison note
**Status**: Complete (exploratory — no code change)
**Source issue**: [#3524](https://github.com/rjwalters/loom/issues/3524)
**Subject**: [EuniAI/Prometheus](https://github.com/EuniAI/Prometheus) — a knowledge-graph-backed, multi-agent software-engineering platform ([arXiv:2507.19942](https://arxiv.org/abs/2507.19942))

> This is the first entry in `docs/notes/` — a lightweight home for
> "what can Loom learn from X?" comparisons that are not formal
> [ADRs](../adr/README.md). ADRs record a decision Loom has made; notes
> like this record an evaluation of an external system and the
> verdicts that fell out of it. Any verdict here that turns into a
> decision should graduate to an ADR or a spike issue.

## TL;DR

Prometheus and Loom solve overlapping problems (autonomous, multi-agent
software engineering) with almost opposite architectural bets.
Prometheus is a **centralized, stateful, knowledge-graph-backed
single-repo issue-resolution engine**; Loom is a **forge-native,
label-coordinated, worktree-isolated fleet**. Studying the divergence
is the point — most of Prometheus's headline features are things Loom
[deliberately rejected](../adr/0009-shepherd-deprecation.md) when it
deleted its own central-state "brain."

Of the five questions in #3524, the verdicts are:

| # | Question | Verdict |
|---|----------|---------|
| 1 | Knowledge graph as shared memory | **Reject** (violates forge-as-state; re-litigates ADR-0009/0010) |
| 2 | Reproduce-before-fix (DRRV) gate | **Spike** (genuine gap; scope carefully to bug-type issues) |
| 3 | Issue-classification routing | **Spike** (lightweight; Curator already ~80% there) |
| 4 | Containerized reproduction/validation | **Reject** for now (host worktrees are a deliberate choice; revisit only if #2 spikes well) |
| 5 | What Loom does better | **N/A — documented below** |

Two spike candidates are flagged at the end for a human to file. No
spike is implemented in this note (per #3524's acceptance criteria).

## What Prometheus is

Attributing to the [Prometheus README](https://github.com/EuniAI/Prometheus)
and its [architecture doc](https://github.com/EuniAI/Prometheus/blob/main/docs/Multi-Agent-Architecture.md)
(verified 2026-07-09):

- **Multi-agent pipeline on LangGraph state machines with checkpointing.**
  An *Issue Classification Agent* routes each issue into one of four
  pipelines — bug / feature / question / doc — and each pipeline is a
  sequence of specialized agents (Bug: Reproduction → Resolution;
  Feature: Analysis → Patch Generation; Question: Context Retrieval →
  Analysis).
- **A Neo4j knowledge graph as long-term memory** (branded "Athena"):
  a unified representation combining **Tree-sitter AST** structure with
  semantic relationships, enabling cross-file / cross-commit /
  repository-wide understanding and graph-based semantic search.
- **A Detect → Reproduce → Repair → Verify (DRRV) loop**: for bugs, the
  system *reproduces the failure in an isolated Docker container* before
  a patch is generated, then runs multi-level validation and optional
  regression testing.
- **Stack**: Python 3.11+, LangGraph, Neo4j (KG), PostgreSQL
  (checkpointing), Docker Compose (isolation), a FastAPI REST API, and
  multi-model LLM support (OpenAI / Anthropic / Gemini).
- **Results**: the README claims **TOP 5 / TOP 1** on the
  [SWE-bench leaderboard](https://www.swebench.com/) for gpt-5 agents
  (Nov 2025). Treat this as a vendor claim; it is not independently
  verified here.

The Prometheus README explicitly positions the knowledge graph and
long-term memory as its differentiators versus SWE-Agent, Lingxi, TRAE,
and OpenHands — i.e. "persistent structured memory" is the bet the
project is built around.

## Where it diverges from Loom

| Dimension | Prometheus | Loom |
|-----------|-----------|------|
| Coordination substrate | Neo4j KG + Postgres checkpoints (central state) | Forge labels + git worktrees ([ADR-0006](../adr/0006-label-based-workflow-coordination.md)) |
| Repo understanding | Persistent AST/semantic knowledge graph | Per-task LLM exploration of the tree |
| Bug workflow | Reproduce-in-container **before** patching (DRRV) | Builder implements; Judge/Auditor validate after |
| State / memory | Long-lived graph memory across issues | Stateless per sweep; per-issue checkpoints only |
| Orchestration | LangGraph state machine + Postgres | `/loom:sweep` + `loom-daemon` + GH Actions cron ([ADR-0010](../adr/0010-daemon-rebuild.md)) |
| Isolation | Docker containers | Git worktrees on the host ([ADR-0004](../adr/0004-worktree-paths-inside-workspace.md)) |
| Deployment | Runs the target repo *into* Prometheus | Runs *inside* the target repo, forge-native |

The single most important structural difference: **Prometheus pulls a
repository into its own stateful engine; Loom installs itself into a
repository and uses that repository's forge as its state.** Almost
every "should Loom adopt X?" question reduces to "does X require Loom to
grow central state it deliberately shed?"

## Question-by-question verdicts

### Q1 — Knowledge graph as shared memory → **Reject**

**The idea**: give Curator / Builder / Judge a persistent, queryable
code graph (AST + semantic relationships) so repo understanding is
computed once and shared, instead of every task re-exploring the tree.

**Verdict: reject**, and this is the highest-confidence verdict in the
note because Loom has *already run this experiment in the opposite
direction*. [ADR-0009](../adr/0009-shepherd-deprecation.md) deleted
~21k LOC of Python orchestration whose core sin was exactly a
**persistent, opaque, central-state coordinator** (the shepherd +
daemon brains with `daemon-state.json` as source of truth). The stated
failure modes were: single point of failure, opaque internal state that
the forge could not reconstruct, coupled scheduling, and a large
maintenance burden. A Neo4j knowledge graph is a *bigger* version of
the same bet — more infrastructure (a graph database to operate), a
second source of truth that can drift from the actual repo, and a
staleness/invalidation problem (the graph must be rebuilt on every
merge or it lies).

[ADR-0006](../adr/0006-label-based-workflow-coordination.md) is the
direct prior art: it evaluated database / message-queue / file-queue
backends for coordination and rejected all of them in favor of forge
labels, precisely to avoid "state hidden from the forge" and "extra
infrastructure to manage." A KG is the "database" alternative wearing a
graph hat.

There is also an empirical reason the cost/benefit is worse for Loom
than for Prometheus. Prometheus amortizes graph-construction cost across
*many issues on one long-lived repo* — the graph is built once and
queried repeatedly. Loom's unit of work is a **short-lived sweep on a
worktree**; a KG that must be built or refreshed per sweep pays the
construction cost without the amortization. And modern coding agents
(including the Claude models Loom's roles run on) are already effective
at on-demand tree exploration with ripgrep/read, which is the cheap,
stateless substitute the KG would replace.

**What Loom should do instead**: nothing structural. If per-task
exploration ever becomes a measured bottleneck, the forge-native answer
is a *disposable, per-sweep* index (e.g. a scratch ctags/Tree-sitter
symbol map in the worktree that dies with the worktree), never a
persistent shared graph database. That is a much smaller idea and does
not need a spike today.

### Q2 — Reproduce-before-fix (DRRV) → **Spike**

**The idea**: for bug-type issues, require a *reproduction* that fails
before the fix and passes after, as an explicit gate — mirroring
Prometheus's Detect → Reproduce → Repair → Verify loop.

**Verdict: spike.** This is the one Prometheus idea that targets a
*real, nameable gap* in Loom rather than a design axis Loom already
chose against. Today Loom validates **after** a PR exists: `builder.md`
implements a fix, then `judge.md` reviews it and `auditor.md`
([.loom/roles/auditor.md](../../.loom/roles/auditor.md)) validates the
integrated result on `main`. Nothing requires a *failing reproduction
before the fix is attempted*. The Doctor
([.loom/roles/doctor.md](../../.loom/roles/doctor.md)) reacts to review
feedback but likewise does not gate on a repro. Prometheus's insight —
"a bug fix you cannot reproduce failing is a bug fix you cannot prove" —
is sound and does not depend on any of the central-state machinery this
note rejects elsewhere.

Crucially, DRRV is separable from Prometheus's KG and Docker: a
reproduce-first discipline is a *workflow gate*, and Loom already has a
workflow-gate mechanism (the sweep lifecycle + the build gate described
in `builder.md`). A "repro-first" mode could live entirely in the
Builder/Doctor role prompt plus an optional check that a
newly-added test fails at the pre-fix commit and passes at the post-fix
commit — no database, no container required.

**Why spike rather than adopt outright**: scope and false-positive risk.
Not every issue is a reproducible bug (features, docs, refactors have no
"failing repro"), so the gate must be *conditional on issue type* (which
ties into Q3) and must degrade gracefully when a repro is genuinely
impractical (flaky, environment-bound, or UI-level bugs — exactly the
class `auditor.md` already flags as hard to validate). A spike should
prototype the gate on a narrow slice (clearly reproducible unit-level
bugs) and measure whether it raises patch quality enough to justify the
added builder latency, before committing to it in the role prompts.

→ **Spike candidate #1** (see below).

### Q3 — Issue-classification routing → **Spike**

**The idea**: route issues into bug / feature / question / docs
pipelines up front, as Prometheus's Issue Classification Agent does.

**Verdict: spike (lightweight).** Loom's Curator
([.loom/roles/curator.md](../../.loom/roles/curator.md)) is already the
closest analogue to Prometheus's classification agent — it enriches
issues and *already branches its guidance by type* ("For bugs:
reproduction steps and expected behavior; For features: user stories and
use cases"; it breaks large features into phases). So Loom is ~80% of
the way to classification *content* without formal *routing*. What Loom
lacks is a machine-readable type signal that a downstream role (notably a
DRRV gate from Q2) could branch on.

The forge-native way to add this is not a new agent or a state machine —
it is a **label** (or a structured Curator field), consistent with
[ADR-0006](../adr/0006-label-based-workflow-coordination.md). The
project's own memory is emphatic that the label set is intentionally
minimal and new labels should not be minted casually, so a spike here
must first ask whether an existing signal (issue title prefix `bug:` /
`feat:`, which `builder.md` already parses for commit-message
derivation) is sufficient before proposing any new label.

This verdict is deliberately coupled to Q2: classification's main *new*
value is enabling type-conditional workflow (repro-first for bugs,
skip-repro for docs). On its own, hard routing adds process for little
gain, because Curator's prose already carries the type-specific
guidance. Spike them together.

→ folded into **Spike candidate #1** (type signal is a prerequisite for
a conditional DRRV gate).

### Q4 — Containerized reproduction / validation → **Reject (for now)**

**The idea**: use Docker isolation for safe test execution, as
Prometheus does, particularly for validation-heavy roles like Auditor.

**Verdict: reject for now.** Loom's isolation model is git worktrees on
the host ([ADR-0004](../adr/0004-worktree-paths-inside-workspace.md)),
which is a *deliberate* choice: worktrees are cheap, sandbox-path-safe,
require no container runtime, and let every role use the developer's
real toolchain without image maintenance. Adopting Docker would add a
hard dependency (a container runtime on every operator/CI host), an
image-maintenance burden (the environment must be encoded and kept in
sync with the repo's real build), and startup latency — costs that land
on *every* sweep, not just the bug-repro slice that might benefit.

Prometheus needs containers because it runs *arbitrary external repos*
it did not install into and must sandbox their untrusted build/test
commands. Loom runs *inside* a repo it was installed into, executing
that repo's own trusted build — the threat model that justifies
containers for Prometheus mostly does not apply to Loom.

This is a "for now," not a "never": *if* the DRRV spike (Q2) shows value
and the reproductions turn out to need stronger isolation than a
worktree provides (e.g. tests that mutate global host state), a
follow-up could evaluate containerizing *just the repro step* — not all
of Loom. That is downstream of Q2 succeeding and is not worth a spike
today.

### Q5 — What Loom does better → see next section

Documented in full below (the counter-argument the issue asked for).

## What Loom already does better

A fair comparison has to name where Loom's bets pay off, not just where
Prometheus has features Loom lacks:

- **Forge-native state, zero infrastructure to operate.** Loom's
  coordination substrate is GitHub/Gitea labels + git worktrees
  ([ADR-0006](../adr/0006-label-based-workflow-coordination.md)). There
  is no Neo4j to run, no Postgres to back up, no `daemon-state.json` to
  corrupt. Every piece of orchestration state is visible in the forge UI
  and reconstructible from it. Prometheus's power comes from central
  state; that state is also its operational cost and its single point of
  failure — the exact failure mode Loom paid down in
  [ADR-0009](../adr/0009-shepherd-deprecation.md).
- **Transparency and auditability.** Anyone can read an issue's label
  history and reconstruct what every agent believed and did. A KG +
  LangGraph checkpoint store is opaque by comparison — debugging
  requires understanding the engine's internal data model, which
  ADR-0009 called out explicitly as a reason to delete Loom's own brain.
- **Human-in-the-loop by construction.** The `loom:issue` label is an
  explicit human approval gate between curation and work
  (ADR-0006). Prometheus's pipeline is designed to run end-to-end
  autonomously; Loom's default is "curate → *human approves* → build."
- **Git-worktree parallelism with no runtime dependency.** Multiple
  sweeps run in isolated worktrees on the host with no container
  runtime, no image builds, and full access to the repo's real
  toolchain ([ADR-0004](../adr/0004-worktree-paths-inside-workspace.md)).
- **Multi-account token rotation.** Loom spreads load across multiple
  Claude accounts at the process-spawn boundary
  ([ADR-0010](../adr/0010-daemon-rebuild.md)) — a scaling axis that a
  single centralized engine does not naturally offer.
- **Fault isolation.** A crashed sweep restarts from its per-issue
  checkpoint and leaves other sweeps untouched; there is no shared brain
  whose failure halts all work (ADR-0009). Prometheus's LangGraph +
  Postgres design centralizes exactly the state whose loss is most
  expensive.

The honest counter-counter-argument: Loom pays for all of this with
**repeated per-task repo exploration** (no shared memory) and **weaker
formal verification of bug fixes** (no repro gate). Q1 argues the first
cost is worth paying; Q2 argues the second is the one worth spiking.

## Spike candidates (for a human to file — not implemented here)

Per #3524's acceptance criteria, "adopt/spike" verdicts are recorded
here for a human to file as separate issues; this note does **not**
implement them.

### Spike candidate #1 — Reproduce-first ("DRRV-lite") mode for bug sweeps

- **Motivation**: Q2 + Q3 above. Loom validates after a PR exists; it
  has no gate requiring a bug to be reproduced (fail → pass) before/after
  the fix.
- **Scope**: a *conditional, opt-in* workflow gate for clearly
  reproducible bug-type issues only. Prerequisite: a lightweight issue
  **type signal** (reuse the existing `bug:` title prefix / Curator
  guidance before proposing any new label — respect the minimal-label
  policy). Mechanism: extend the Builder/Doctor role prompt to add a
  failing test first, plus an optional check that the test fails at the
  pre-fix commit and passes at the post-fix commit. **No KG, no Docker.**
- **Explicitly out of scope**: features, docs, refactors (no repro);
  flaky/environment/UI bugs (degrade gracefully — flag, don't block).
- **Success metric**: measure whether the gate raises patch quality
  (fewer Judge change-requests / Doctor cycles on bug PRs) enough to
  justify added Builder latency, on a narrow slice, before touching the
  shipped role prompts.
- **Grounds**: `.loom/roles/{builder,doctor,judge,auditor}.md`, the
  build gate in `builder.md`, the sweep lifecycle in `CLAUDE.md`.

### Spike candidate #2 — Disposable per-sweep symbol index (only if exploration is measured as a bottleneck)

- **Motivation**: the *steelman* of Q1 without its central-state cost.
  If per-task tree exploration is ever measured as a real latency/quality
  bottleneck, a **worktree-local, disposable** symbol map (ctags or
  Tree-sitter) that is built at sweep start and dies with the worktree
  could help — without any persistent shared graph database.
- **Scope guard**: this is *not* a knowledge graph and must not become
  one. No cross-sweep persistence, no external database, no second
  source of truth that outlives the worktree. If the design starts
  needing a server, stop — that is the ADR-0009 mistake.
- **Priority**: low. Only worth filing if profiling shows exploration
  cost is material; otherwise leave it as a documented non-goal.

## References

- Subject: [EuniAI/Prometheus](https://github.com/EuniAI/Prometheus)
  ([README](https://github.com/EuniAI/Prometheus/blob/main/README.md),
  [architecture](https://github.com/EuniAI/Prometheus/blob/main/docs/Multi-Agent-Architecture.md),
  [paper](https://arxiv.org/abs/2507.19942))
- Loom prior art:
  [ADR-0004](../adr/0004-worktree-paths-inside-workspace.md) (worktree isolation),
  [ADR-0006](../adr/0006-label-based-workflow-coordination.md) (forge-as-state-machine),
  [ADR-0009](../adr/0009-shepherd-deprecation.md) (central-brain deletion),
  [ADR-0010](../adr/0010-daemon-rebuild.md) (MCP daemon rebuild)
- Loom roles grounding the comparison:
  [`.loom/roles/curator.md`](../../.loom/roles/curator.md),
  [`.loom/roles/builder.md`](../../.loom/roles/builder.md),
  [`.loom/roles/judge.md`](../../.loom/roles/judge.md),
  [`.loom/roles/doctor.md`](../../.loom/roles/doctor.md),
  [`.loom/roles/auditor.md`](../../.loom/roles/auditor.md)
- Origin: issue [#3524](https://github.com/rjwalters/loom/issues/3524)
