# Loom Daemon - Parent Mode (DEPRECATED)

**This file is deprecated.** The Loom daemon is now implemented in Python.

## Execution

This skill is superseded. Use `/loom:sweep <issue>` for the single-issue
lifecycle, or `mcp__loom__dispatch_sweep` against the Rust `loom-daemon` for
multi-account dispatch. See `.loom/docs/daemon-reference.md` for the current
daemon surface.

The historical Python daemon handled all orchestration internally:
- Main event loop
- Iteration logic
- Shepherd spawning
- Support role management
- State management

## Migration

The two-tier LLM architecture (parent/iteration) has been replaced with a deterministic Python implementation:

- **Old**: `/loom` -> `loom-parent.md` -> Task() -> `loom-iteration.md`
- **New**: `/loom:sweep <issue>` (Tier 1) or `mcp__loom__dispatch_sweep` -> Rust `loom-daemon` (Tier 2)

See `loom-tools/src/loom_tools/daemon/` for the implementation.
