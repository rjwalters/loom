# Loom Documentation

Reference documentation for the Loom repository. Operating instructions for
agents live in [`CLAUDE.md`](../CLAUDE.md); runtime subsystem reference that
ships to consumer repos lives in [`defaults/docs/`](../defaults/docs/) (installed
as `.loom/docs/`).

## Directories

| Path | Contents |
|------|----------|
| [`adr/`](adr/) | Architecture Decision Records plus a [template](adr/template.md). Numbering is sparse — some numbers were never used. Start at the [ADR index](adr/README.md), which is authoritative for the current set. |
| [`api/`](api/) | API surface reference. |
| [`design/`](design/) | Design notes for specific subsystems — [config resolution tiers](design/config-resolution-tiers.md), [the revised label state machine](design/issue-332-label-state-machine.md), [the supervised restart primitive](design/supervised-restart-primitive.md), [Architect/Hermit work-generation cadence](design/architect-hermit-cadence.md), [token pool provider identity](design/token-pool-provider-identity.md), [code-intel-lite](design/code-intel-lite.md) (blast-radius helper evaluation for Judge/Hermit), [repo knowledge digest](design/repo-knowledge-digest.md), [retrospective pattern mining](design/retrospective-pattern-mining.md), [fleet cross-repo summary](design/fleet-cross-repo-summary.md) (decides the multi-repo dashboard rollup is already solved), [persistence inventory](design/persistence-inventory.md) (every persisted store plus the target one-journal-per-concern design, epic #9908 Phase 1), [daemon roll pause-and-resume](design/daemon-roll-pause-resume.md) (contract for in-flight work across a version roll), [daemon roll restore](design/daemon-roll-restore.md) (restore the previous binary when a candidate fails to start), [fleet captain dispatch](design/fleet-captain-dispatch.md) (workers ask, the captain assigns), the [prompt library & template system](design/issue-179-prompt-library.md), [issue sizing](design/issue-sizing.md) (story points as the only up-front signal), [shrinking the `loom-daemon` lib-test compile unit](design/loom-daemon-lib-test-shrink.md), [loom-guard extraction](design/loom-guard-extraction.md) (the generic guard as a shared component), the [loom tools CLI boundary and migration map](design/loom-tools-cli-boundary.md), the [operator activity signal](design/operator-activity-signal.md), and [work-finder step dispatch](design/work-finder-step-dispatch.md) (item first, then role). |
| [`experiments/`](experiments/) | Dated machine-readable receipts from one-off experiment runs (e.g. the native-harness canary and the Kimi harness probe) — evidence artifacts, not prose. |
| [`guides/`](guides/) | How-to guides: getting started, quickstart tutorial, development, dev workflow, git workflow, testing, code quality, CI/CD setup, CLI reference, daemon dev mode, fork drift, troubleshooting, styling, TypeScript conventions, common tasks. |
| [`mcp/`](mcp/) | MCP server documentation — see the [MCP README](mcp/README.md) and [loom-terminals](mcp/loom-terminals.md). |
| [`migration/`](migration/) | Completed-migration history: the [v0.10.0 shepherd deprecation](migration/v0.10.0-shepherd-deprecation.md), the [v0.10.0 daemon rebuild](migration/v0.10.0-daemon-rebuild.md), and [daemon-state consumers](migration/daemon-state-consumers.md). |
| [`notes/`](notes/) | Ad-hoc technical notes. |
| [`merge-queue-pilot/`](merge-queue-pilot/) | Desired-state example for the merge queue pilot. |
| [`observability/`](observability/) | Observability notes — e.g. the [landed-size (LSI) feasibility result](observability/landed-size-dialect-feasibility.md) for SigNoz/ClickHouse queries. |
| [`philosophy/`](philosophy/) | Essays on agent archetypes, AI code smell, Loom intelligence, working with AI, and [what Loom optimizes for — and when it's the wrong tool](philosophy/latency-vs-reliability.md). |
| [`research/`](research/) | Evaluations and measurement runbooks — builder/judge fan-out, codecast, dynamic workflows. |

## Top-level documents

| File | Purpose |
|------|---------|
| [`agents.md`](agents.md) | Agent role overview. |
| [`ROADMAP.md`](ROADMAP.md) | Roadmap of planned work. |
| [`operating-modes.md`](operating-modes.md) | Fleet, local daemon and operator-directed session support contract; infrastructure boundaries and planned CLI separation. |
| [`forge-contract.md`](forge-contract.md) | Forge contract — identity, outcomes, evidence, profiles. |
| [`workflows.md`](workflows.md) | Workflow reference. |
| [`autonomous-mode-e2e.md`](autonomous-mode-e2e.md) | End-to-end autonomous mode walkthrough. |
| [`measure-est-cores-per-sweep.md`](measure-est-cores-per-sweep.md) | Method for measuring estimated cores per sweep. |
| [`model-selection-retune.md`](model-selection-retune.md) | Model-selection retuning notes. |

## Related

- [`CLAUDE.md`](../CLAUDE.md) — the operating core for agents working in this repo
- [`README.md`](../README.md) — project overview and installation
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — contribution workflow
- [`CHANGELOG.md`](../CHANGELOG.md) — release history
