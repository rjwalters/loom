# Changelog

All notable changes to Loom will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-01-24

### Summary

This release introduces the **Three-Layer Architecture** with the new `/loom` daemon as the centerpiece. Loom has evolved from a manual orchestration tool to a fully autonomous development system capable of generating its own work, scaling shepherds, and maintaining continuous operation.

### Architecture

#### Three-Layer Orchestration Model

Loom now operates across four distinct layers:

- **Layer 3: Human Observer** - Oversight, proposal approval, and strategic direction
- **Layer 2: Loom Daemon (`/loom`)** - System-wide orchestration and work generation
- **Layer 1: Shepherds (`/shepherd <issue>`)** - Per-issue lifecycle orchestration
- **Layer 0: Workers** - Single task execution (Builder, Judge, Curator, Doctor, etc.)

#### Role Restructuring

- Renamed `loom.md` to `shepherd.md` (now Layer 1)
- Created new `loom.md` for Layer 2 daemon role
- Updated all command references and documentation

### Added

#### Fully Autonomous Daemon (`/loom`)

- Continuous loop with configurable polling interval (default 30 seconds)
- Auto-spawns shepherds when `loom:issue` issues are available
- Auto-triggers Architect/Hermit when issue backlog falls below threshold
- Auto-ensures Guide and Champion roles keep running
- State persistence in `.loom/daemon-state.json` for crash recovery
- Graceful shutdown via `.loom/stop-daemon` signal file

#### Status Observation (`/loom status`)

- Read-only Layer 3 observation interface
- Shell script helper: `.loom/scripts/loom-status.sh`
- JSON output option for scripting and automation
- Shows shepherd assignments, issue counts, and daemon health

#### Cleanup Mechanisms

- Task artifact archival (`./scripts/archive-logs.sh`)
- Safe worktree cleanup (`./scripts/safe-worktree-cleanup.sh`)
- Event-driven cleanup (`./scripts/daemon-cleanup.sh`)

#### Resilience Features

- Stuck agent detection and recovery system
- Circuit breaker pattern for daemon IPC resilience
- Automatic recovery from `daemon-state.json` on restart

#### Observability

- Agent effectiveness metrics tracking
- LLM resource usage tracking (tokens and cost)
- Test outcome tracking in activity database
- Prompts linked to GitHub issues, PRs, and commits

#### Builder Enhancements

- Parallel claiming workflow for faster issue claiming
- Pre-implementation review section in role guidelines
- Worktree merge graceful handling

#### Other Features

- Auto-configuration of missing terminals in force mode
- Graceful shutdown signal script for Loom agents
- Dependency unblocking in Guide role
- Auto-unblock for dependent issues when Champion merges PR
- Extend command to claim system for long-running work
- CLI fallback mode for `/loom` orchestrator when MCP unavailable

### Changed

- Documentation updated to reflect three-layer architecture
- Clarified daemon execution model: background process vs interactive mode
- `/loom --force-merge` now runs Judge phase instead of skipping

### Fixed

- Worktree merge handling in Loom orchestrator
- Force-merge mode properly runs Judge phase

### Configuration

#### Daemon Configuration Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `ISSUE_THRESHOLD` | 3 | Trigger Architect/Hermit when `loom:issue` count below this |
| `MAX_PROPOSALS` | 5 | Maximum pending proposal issues |
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd processes |
| `ISSUES_PER_SHEPHERD` | 2 | Scale factor: target = ready_issues / ISSUES_PER_SHEPHERD |
| `POLL_INTERVAL` | 60 | Seconds between daemon loop iterations |

### Migration Notes

Existing v0.1.x installations can upgrade cleanly:

1. The installation script automatically injects the correct version
2. Role references are backward-compatible
3. Existing workflows continue to work unchanged

### Related Issues

- #1040 - Implement fully autonomous daemon loop for /loom
- #1039 - Add /loom status command for Layer 3 observation
- #1038 - Clarify daemon execution model
- #1034 - Add cleanup mechanisms for task artifacts and worktrees
- #1031 - Update documentation to three-layer architecture
- #1029 - Add stuck agent detection and recovery system
- #1030 - Add circuit breaker pattern for daemon IPC resilience
- #1028 - Add basic agent effectiveness metrics
- #1020 - Link prompts to GitHub issues and PRs
- #1018 - Add LLM resource usage tracking
- #1016 - Update /loom to run continuously with parallel subagents
- #1008 - Create Layer 2 loom.md daemon role
- #1005-#1007 - Rename loom.md to shepherd.md
- #1004 - Add parallel claiming workflow to Builder
- #1003 - Handle worktree merge gracefully
- #1002 - Add agent status reporting script
- #1001 - Add auto-configuration of missing terminals
- #1000 - Add Pre-Implementation Review to Builder
- #998 - Add graceful shutdown signal script
- #997 - Add dependency unblocking to Guide
- #988 - Add auto-unblock for dependent issues
- #987 - Add extend command to claim system
- #986 - Fix /loom --force-merge Judge phase
- #984 - Add CLI fallback mode to /loom

## [0.1.0] - 2025-12-01

### Added

- Initial release of Loom
- Multi-terminal GUI with Tauri + xterm.js
- Role-based terminal configuration
- GitHub label-based workflow coordination
- Worker roles: Builder, Judge, Curator, Doctor, Champion, Architect, Hermit, Guide
- Git worktree isolation for concurrent work
- Manual Orchestration Mode (MOM) with Claude Code
- Tauri App Mode for automated orchestration
- MCP servers for programmatic control (loom-terminals, loom-ui, loom-logs)
- Installation script for target repositories
- Quickstart templates for webapp, desktop, and API projects

[Unreleased]: https://github.com/rjwalters/loom/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/rjwalters/loom/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rjwalters/loom/releases/tag/v0.1.0
