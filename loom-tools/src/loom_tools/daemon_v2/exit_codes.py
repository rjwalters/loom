"""Daemon exit codes."""

from __future__ import annotations

from enum import IntEnum


class DaemonExitCode(IntEnum):
    """Exit codes for the daemon process."""

    SUCCESS = 0  # Graceful shutdown
    STARTUP_FAILED = 1  # Pre-flight checks failed
    SESSION_CONFLICT = 2  # Another daemon is running
    SIGNAL_SHUTDOWN = 3  # Stop signal received
    ERROR = 4  # Unhandled error
