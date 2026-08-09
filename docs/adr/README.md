# Architecture Decision Records (ADRs)

This directory contains Architecture Decision Records (ADRs) for the Loom project. ADRs document significant architectural decisions, their context, consequences, and alternatives considered.

## What is an ADR?

An Architecture Decision Record captures an important architectural decision made along with its context and consequences. It helps:
- New contributors understand "why" decisions were made
- Track the evolution of architectural thinking
- Reference specific design choices in issues and PRs
- Avoid re-litigating past decisions

## ADR Index

### Core Architecture

- [ADR-0001: Observer Pattern for State Management](0001-observer-pattern-state-management.md)
  - **Status**: Superseded (frontend removed in v0.9)
  - **Summary**: Use Observer Pattern with Map-based store for decoupled state management
  - **Key Decision**: Prefer Observer Pattern over Redux/MobX for simplicity and learning value

- [ADR-0008: tmux + Rust Daemon Architecture](0008-tmux-daemon-architecture.md)
  - **Status**: Accepted
  - **Summary**: Two-tier architecture with Rust daemon managing tmux sessions
  - **Key Decision**: Use tmux for persistence and Rust for performance over Node.js or embedded terminals

### Configuration & State

- [ADR-0003: Separate Configuration and State Files](0003-config-state-file-split.md)
  - **Status**: Accepted
  - **Summary**: Split `.loom/config.json` (user preferences) and `.loom/state.json` (runtime state)
  - **Key Decision**: Separate concerns for safer restarts and independent schema evolution

### Workflows & Coordination

- [ADR-0004: Git Worktree Paths Inside Workspace](0004-worktree-paths-inside-workspace.md)
  - **Status**: Accepted
  - **Summary**: Create all git worktrees inside `.loom/worktrees/` for sandbox compatibility
  - **Key Decision**: Sandbox-safe paths inside workspace over external directories

- [ADR-0006: Label-Based Workflow Coordination](0006-label-based-workflow-coordination.md)
  - **Status**: Accepted
  - **Summary**: Use GitHub labels as state machine for agent workflow coordination
  - **Key Decision**: Leverage GitHub labels over database, message queue, or file-based queue

### Orchestration Architecture

- [ADR-0009: Deprecate and Delete Shepherd Brain and Python Daemon (Phase 3)](0009-shepherd-deprecation.md)
  - **Status**: Accepted
  - **Summary**: Delete `loom_tools/shepherd/` (~16.8k LOC) and `loom_tools/daemon_v2/` (~4.7k LOC); replace with spawn loop + GitHub Actions cron + `/loom:sweep`
  - **Key Decision**: Forge-as-state-machine + stateless components over persistent Python orchestration brain

- [ADR-0010: Rebuild Daemon Mode as Rust Binary with MCP-Tool Surface (v0.10.0)](0010-daemon-rebuild.md)
  - **Status**: Accepted
  - **Summary**: Extend the existing Rust `loom-daemon` binary with named sweep dispatch, a pub/sub event bus, and MCP monitoring tools instead of restoring the deleted Python brain
  - **Key Decision**: MCP-tool surface on the existing daemon binary over a restored Python brain or a shell-level `daemon.sh` wrapper

- [ADR-0013: Retire the Python `loom-tools` Package — One Rust Binary Plus Bash](0013-loom-tools-python-retirement.md)
  - **Status**: Accepted
  - **Summary**: Delete the ~31.8k-line Python `loom_tools` package over four phases (epic #4081), moving its load-bearing functionality into `loom-daemon` subcommands while every shell entry point keeps its name and flags; motivated by #4079, where a stale editable pip install's frozen console scripts shadowed the Rust binary on PATH
  - **Key Decision**: One commit-stamped Rust artifact plus bash over a maintained pip install, with a byte-compatible on-disk state contract holding across every phase so no cutover needed a flag day; `loom-search` carved out of the deletion (opt-in, no native port, no test would have caught its removal)

- [ADR-0014: Decouple Forge API Cost From Coordination Chatter — Local Evaluation Memo, Safehouse as Accelerator Only](0014-forge-coordination-decoupling.md)
  - **Status**: Accepted (design decision; implementation phased into follow-up issues)
  - **Summary**: The forge's GraphQL/REST quota is spent on repeat evaluation of unchanged state, not on the inherent label transitions of a normal issue lifecycle; answers the four open questions from #5057 on where an "already evaluated" memo lives, whether a webhook-fed Worker becomes a control-plane participant, what the memo's input-hash should be, and whether the label protocol changes
  - **Key Decision**: A daemon-local evaluation memo (per-role content hash, not `updated_at`) is the store of record, with safehouse as an optional best-effort cross-host broadcast accelerator — never the store itself; claims stay forge-authoritative and the label protocol is untouched; defer webhook/Worker-as-control-plane (Lever C) until the local memo + safehouse broadcast are measured and shown insufficient

- [ADR-0015: In-Builder Test-First Checkpoint — PR-Body Signal, Advisory on Absence, Blocking on Contradiction](0015-builder-test-first-checkpoint.md)
  - **Status**: Accepted
  - **Summary**: Adapts damusix/atomic-claude's maker/checker TDD split (#5844/#5849) into a required `TDD:` line in the PR body's Test Plan section, checkable by Judge against the diff
  - **Key Decision**: A PR-body prose line, not a commit-order check (Loom's squash-merge workflow makes commit order frequently unobservable); Judge notes an absent line or a plausible `TDD: no` reason advisory-only, but treats a `TDD: yes` claim contradicted by the diff as blocking — the same class of finding as any other inaccurate PR claim; not a `buildGate` extension, since classifying plausibility requires judgment `buildGate` is designed to exclude

### CI Infrastructure

- [ADR-0011: CI Runner Platform — Speedup Ceiling and Decision](0011-ci-runner-platform.md)
  - **Status**: Accepted
  - **Summary**: Measured that compile is only ~10% of `ci.yml`'s critical path while one serial-locked `loom-daemon` test binary is ~67%, and that its ~127s serial-lock floor is not reducible by adding cores
  - **Key Decision**: Reject new CI hardware on speed grounds alone; default to Graviton (arch parity) if a runner is ever provisioned (#4057) while treating full macOS parity as a separate, independently-costed decision; prioritize de-serializing `#[serial]` tests over buying cores

### Worker Runtime

- [ADR-0012: Multi-Runtime Worker Support via a Single Runtime Adapter Contract](0012-runtime-adapter-contract.md)
  - **Status**: Accepted
  - **Summary**: Support multiple CLI agent runtimes (Claude Code, Codex, Amp, oh-my-pi) through one seven-point adapter contract instead of per-runtime parallel scripts; collaborate via upstream PRs from the gpeyton/loom fork, not cherry-picks
  - **Key Decision**: A single runtime adapter contract (`defaults/docs/runtime-adapters.md`) with Claude Code as adapter #1/default/tier-1 (zero regression) and non-Claude runtimes tier-2 (CI-gated) over parallel per-runtime special-casing

## Creating a New ADR

When making a significant architectural decision:

1. **Copy the template**:
   ```bash
   cp template.md NNNN-short-title.md
   ```

2. **Number sequentially**: Use the next available number (e.g., 0009)

3. **Fill in all sections**:
   - **Context**: What problem are we solving?
   - **Decision**: What did we decide?
   - **Consequences**: What are the tradeoffs?
   - **Alternatives**: What else did we consider and why reject it?

4. **Update this README**: Add your ADR to the index above

5. **Reference in code**: Link to ADR in relevant files using comments

## ADR Status

- **Proposed**: Under discussion, not yet accepted
- **Accepted**: Decision approved and implemented
- **Deprecated**: No longer recommended, but not yet superseded
- **Superseded**: Replaced by a newer ADR (link to replacement)

## Format

ADRs use a lightweight format:
- Markdown for easy reading and version control
- Numbered sequentially for stable references
- Grouped by topic in this index for discoverability

See [template.md](template.md) for the full ADR template.

## References

- Michael Nygard's ADR: http://thinkrelevance.com/blog/2011/11/15/documenting-architecture-decisions
- GitHub ADR Organization: https://adr.github.io/
