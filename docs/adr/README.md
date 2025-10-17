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
  - **Status**: Accepted
  - **Summary**: Use Observer Pattern with Map-based store for decoupled state management
  - **Key Decision**: Prefer Observer Pattern over Redux/MobX for simplicity and learning value

- [ADR-0002: Vanilla TypeScript over React/Vue/Svelte](0002-vanilla-typescript-over-frameworks.md)
  - **Status**: Accepted
  - **Summary**: Build frontend with Vanilla TypeScript using direct DOM manipulation
  - **Key Decision**: Prioritize performance, learning value, and simplicity over framework features

- [ADR-0008: tmux + Rust Daemon Architecture](0008-tmux-daemon-architecture.md)
  - **Status**: Accepted
  - **Summary**: Two-tier architecture with Rust daemon managing tmux sessions
  - **Key Decision**: Use tmux for persistence and Rust for performance over Node.js or embedded terminals

### Configuration & State

- [ADR-0003: Separate Configuration and State Files](0003-config-state-file-split.md)
  - **Status**: Accepted
  - **Summary**: Split `.loom/config.json` (user preferences) and `.loom/state.json` (runtime state)
  - **Key Decision**: Separate concerns for safer restarts and independent schema evolution

- [ADR-0007: Tauri IPC for Filesystem Operations](0007-tauri-ipc-for-filesystem-operations.md)
  - **Status**: Accepted
  - **Summary**: Use Rust backend IPC commands for filesystem access instead of Tauri FS API
  - **Key Decision**: Full filesystem access and better validation via Rust backend

### Workflows & Coordination

- [ADR-0004: Git Worktree Paths Inside Workspace](0004-worktree-paths-inside-workspace.md)
  - **Status**: Accepted
  - **Summary**: Create all git worktrees inside `.loom/worktrees/` for sandbox compatibility
  - **Key Decision**: Sandbox-safe paths inside workspace over external directories

- [ADR-0006: Label-Based Workflow Coordination](0006-label-based-workflow-coordination.md)
  - **Status**: Accepted
  - **Summary**: Use GitHub labels as state machine for agent workflow coordination
  - **Key Decision**: Leverage GitHub labels over database, message queue, or file-based queue

### UI & Interaction

- [ADR-0005: HTML5 Drag API over Mouse Events](0005-html5-drag-api-over-mouse-events.md)
  - **Status**: Accepted
  - **Summary**: Use HTML5 Drag and Drop API for terminal card reordering
  - **Key Decision**: Native browser drag behavior over custom mouse event implementation

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
