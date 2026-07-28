# ADR-0012: Multi-Runtime Worker Support via a Single Runtime Adapter Contract

## Status

Accepted

> **Note on numbering.** The originating issue (#4168) proposed the filename
> `0010-runtime-adapter-contract.md`, but ADR-0010 (`0010-daemon-rebuild.md`) and
> ADR-0011 (`0011-ci-runner-platform.md`) were already assigned. Per the ADR
> README's "number sequentially / stable references" rule, this decision takes
> the next free number, **0012**.

## Context

Loom is hardwired to Claude Code at every layer: the spawn path
(`defaults/scripts/spawn-claude.sh`), the token pool (`.loom/tokens/`),
instruction packaging (`CLAUDE.md`, `.claude/` skills), guard hooks
(`guard-destructive.sh`, `guard-loom-workflow.sh`, `guard-worktree-paths.sh`),
error classification (`defaults/scripts/lib/classify-error.sh`), model aliasing
(`defaults/scripts/resolve-model.sh`, `sweep.modelAliases`), and the daemon's
terminal management.

Graham Peyton's fork (gpeyton/loom, tracked in **#4165**) both proved the demand
and exposed the cost: Codex support works there, but as a parallel special case
— a separate `spawn-codex.sh`, a `defaults/.codex/` config tree, Codex-specific
CI. Adding a third runtime that way would triple the special-casing. Each new
runtime touched every hardwired layer independently, with no shared seam.

The operator direction (2026-07-27, recorded on **#4167**) is to support
multiple CLI agent tools as first-class worker runtimes — Claude Code, OpenAI
Codex CLI, Amp, oh-my-pi (omp), and future tools — and to do it **in
collaboration with Graham**, via upstream PRs from his fork rather than one-way
cherry-picks.

## Decision

Introduce **one runtime adapter contract** that every worker runtime implements,
rather than a set of per-runtime parallel scripts. The contract has seven points
— spawn, model mapping, error classification, usage accounting, instruction
format, permission/sandbox mapping, and capability declaration — specified
normatively in [`defaults/docs/runtime-adapters.md`](../../defaults/docs/runtime-adapters.md).

Two decisions are load-bearing:

1. **A single contract, not parallel scripts.** The Claude Code behavior is
   extracted *behind* the interface (a `spawn-worker.sh`-style dispatcher with
   the adapter implemented for Claude Code only, zero behavior change), and each
   subsequent runtime is a new adapter satisfying the same contract. The seven
   contract points are the shared seams; a runtime supplies its own internals
   (its spawn entry point, its per-provider error pattern table, its tier→ID
   model map, its `GUARDRAIL-PARITY.md`) behind them. Adding a runtime becomes
   "implement the contract", not "special-case every layer".

2. **Collaboration via upstream PRs from the fork, not cherry-picks.** The
   fork's runtime-neutral work (spawn dispatcher, restructured error tables,
   Codex runner, `AGENTS.md` codegen, provider-aware pool, guardrail parity,
   reusable CI role workflow) lands as PRs from gpeyton/loom onto the contract,
   preserving attribution and reducing re-divergence. `defaults/docs/runtime-adapters.md`
   carries the fork-PR → contract-slot mapping table.

**Tier policy.** Claude Code is adapter #1, the default, and tier-1 with zero
regression. Non-Claude runtimes are tier-2 (CI-gated, no operator dogfooding) by
default, and are promoted to tier-1 only when someone commits to tier-1
ownership of the adapter, its parity doc, and its CI leg. Tier is a
maintainership statement, not a capability statement.

## Consequences

### Positive

- **Adding a runtime is a bounded change** — implement seven contract points
  behind existing seams, instead of editing every hardwired layer.
- **Claude Code is unchanged.** Extraction is behind the interface with the
  adapter implemented for Claude Code only; existing installs see zero behavior
  change and Claude Code remains the default.
- **The trust boundary is documented, not assumed.** Every adapter ships a
  guardrail-parity doc with an explicit residual-gap section; no runtime is
  admitted without one.
- **Attribution and drift are managed.** Upstreaming the fork's work as PRs
  keeps authorship intact and reduces the fork/upstream divergence #4165 tracks.
- **Instructions never fork per-runtime** — `AGENTS.md` (the AAIF/Linux
  Foundation standard) is the single cross-runtime source, generated alongside
  the richer `CLAUDE.md`.

### Negative

- **The Claude-Code extraction is a real refactor** carrying risk of regression
  on the one path that must not regress; it ships with a single runtime before
  any second runtime is added.
- **A shared category contract constrains each runtime.** Every adapter must map
  its error wording onto the *same* `classify-error.sh` category set and its
  models onto the *same* logical tiers, even where a runtime's native model is a
  poor fit.
- **Model mapping is not yet single-source across runtimes.** `sweep.modelAliases`
  has a known Rust/Python divergence (the Rust dispatch resolver is tiered; the
  Python `model_tiers` resolver is not). This ADR does **not** resolve it — it is
  deferred to #4167 Phase 4 (pool + tiering integration) as an explicit open
  reconciliation item, and adapter authors are warned of it in the contract doc.
- **Tier-2 runtimes carry a maintenance expectation** (CI leg + parity doc) that
  must be owned before promotion, so capability alone does not make a runtime
  trusted.

## Alternatives Considered

**Per-runtime parallel scripts (the fork's current shape).** Keep adding
`spawn-<runtime>.sh` + `defaults/.<runtime>/` + runtime-specific CI as parallel
special cases. Rejected: this is exactly the pattern that would triple the
special-casing with a third runtime; there is no shared seam, so every hardwired
layer is edited independently per runtime.

**Claude Code as a privileged default rather than adapter #1.** Leave Claude
Code hardwired and bolt other runtimes on beside it. Rejected: it preserves the
coupling this effort exists to remove and makes the abstraction shaped around
Claude's assumptions rather than a neutral contract. Claude Code stays the
*default* and tier-1, but it is *an adapter*, implemented behind the same
interface as the others (proving the interface is real).

**Cherry-pick the fork's Codex work instead of upstreaming PRs.** Rejected: it
discards attribution, re-diverges immediately, and forfeits the collaboration —
the fork already runs upstream drift monitoring pointed at this repo, so PRs are
the lower-friction path for both sides.

## References

- Related GitHub Issues: #4167 (multi-runtime worker support proposal — the
  seven contract points, phasing, and fork PR list), #4165 (fork divergence
  triage / harvest tracking), #4168 (this ADR + the contract doc)
- Related ADRs: [ADR-0009](0009-shepherd-deprecation.md) (forge-as-state-machine
  + stateless components), [ADR-0010](0010-daemon-rebuild.md) (Rust daemon +
  MCP-tool dispatch surface)
- Contract specification: [`defaults/docs/runtime-adapters.md`](../../defaults/docs/runtime-adapters.md)
- Fork: https://github.com/gpeyton/loom · `AGENTS.md` standard: https://agents.md
</content>
