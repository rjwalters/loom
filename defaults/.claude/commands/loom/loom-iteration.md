# Loom Daemon - Iteration Mode (DEPRECATED)

**This file is deprecated.** The Loom daemon is now implemented in Python.

## Execution

This skill is superseded. Use `/loom:sweep <issue>` for the single-issue
lifecycle, or `mcp__loom__dispatch_sweep` against the Rust `loom-daemon` for
multi-account dispatch. See `.loom/docs/daemon-reference.md` for the current
daemon surface.

## Migration

The two-tier LLM architecture (parent/iteration) has been replaced with a deterministic Python implementation:

- **Old**: `/loom iterate` -> `loom-iteration.md` with full gh commands
- **New**: `loom_tools.daemon.iteration.run_iteration()` in Python

Key changes:
- Snapshot capture via `build_snapshot()` (not LLM-interpreted gh commands)
- Completion checking via Python code (not Task() subagents)
- Shepherd spawning via `spawn_agent()` (not LLM-interpreted agent-spawn.sh)
- Deterministic action execution based on snapshot recommendations

See `loom-tools/src/loom_tools/daemon/` for the implementation.
