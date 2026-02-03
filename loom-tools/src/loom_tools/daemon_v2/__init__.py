"""Loom Daemon - Python implementation for autonomous orchestration.

This package implements a deterministic daemon loop that orchestrates
the Loom development pipeline, replacing the LLM-interpreted /loom skill.

The daemon:
- Captures system state via build_snapshot()
- Spawns shepherds for ready issues
- Triggers support roles on interval/demand
- Auto-promotes proposals in force mode
- Handles graceful shutdown via stop signals

Usage:
    loom-daemon              # Start daemon (runs until cancelled)
    loom-daemon --force      # Enable force mode (auto-promote, auto-merge)
    loom-daemon --status     # Check if daemon is running
    loom-daemon --health     # Show daemon health status
"""

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.exit_codes import DaemonExitCode
from loom_tools.daemon_v2.loop import run

__all__ = [
    "DaemonConfig",
    "DaemonContext",
    "DaemonExitCode",
    "run",
]
