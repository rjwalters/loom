# Loom Documentation

Reference documentation for the Loom repository. Operating instructions for
agents live in [`CLAUDE.md`](../CLAUDE.md); runtime subsystem reference that
ships to consumer repos lives in [`defaults/docs/`](../defaults/docs/) (installed
as `.loom/docs/`).

## Directories

| Path | Contents |
|------|----------|
| [`adr/`](adr/) | Architecture Decision Records (0001–0013) plus a [template](adr/template.md). Start at the [ADR index](adr/README.md). |
| [`api/`](api/) | API surface reference. |
| [`design/`](design/) | Design notes for specific subsystems — config resolution tiers, the label state machine, the supervised restart primitive, Architect/Hermit cadence. |
| [`guides/`](guides/) | How-to guides: getting started, quickstart tutorial, development, dev workflow, git workflow, testing, code quality, CI/CD setup, CLI reference, daemon dev mode, fork drift, troubleshooting, styling, TypeScript conventions, common tasks. |
| [`mcp/`](mcp/) | MCP server documentation — see the [MCP README](mcp/README.md) and [loom-terminals](mcp/loom-terminals.md). |
| [`migration/`](migration/) | Completed-migration history: the [v0.10.0 shepherd deprecation](migration/v0.10.0-shepherd-deprecation.md), the [v0.10.0 daemon rebuild](migration/v0.10.0-daemon-rebuild.md), and [daemon-state consumers](migration/daemon-state-consumers.md). |
| [`notes/`](notes/) | Ad-hoc technical notes. |
| [`philosophy/`](philosophy/) | Essays on agent archetypes, AI code smell, Loom intelligence, and working with AI. |
| [`research/`](research/) | Evaluations and measurement runbooks — builder/judge fan-out, codecast, dynamic workflows. |

## Top-level documents

| File | Purpose |
|------|---------|
| [`agents.md`](agents.md) | Agent role overview. |
| [`workflows.md`](workflows.md) | Workflow reference. |
| [`autonomous-mode-e2e.md`](autonomous-mode-e2e.md) | End-to-end autonomous mode walkthrough. |
| [`measure-est-cores-per-sweep.md`](measure-est-cores-per-sweep.md) | Method for measuring estimated cores per sweep. |
| [`model-selection-retune.md`](model-selection-retune.md) | Model-selection retuning notes. |

## Related

- [`CLAUDE.md`](../CLAUDE.md) — the operating core for agents working in this repo
- [`README.md`](../README.md) — project overview and installation
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — contribution workflow
- [`CHANGELOG.md`](../CHANGELOG.md) — release history
