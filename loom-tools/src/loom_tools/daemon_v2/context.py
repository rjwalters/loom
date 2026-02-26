"""Daemon runtime context."""

from __future__ import annotations

import os
import pathlib
import time
from dataclasses import dataclass, field
from typing import Any

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.models.daemon_state import DaemonState


@dataclass
class DaemonContext:
    """Runtime context for the daemon.

    Holds configuration, state, and per-iteration data. The context is
    passed to all daemon functions and updated throughout the loop.
    """

    config: DaemonConfig
    repo_root: pathlib.Path
    session_id: str = field(default_factory=lambda: f"{int(time.time())}-{os.getpid()}")

    # Loop state
    iteration: int = 0
    running: bool = True
    consecutive_stalled: int = 0

    # Per-iteration data (refreshed each iteration)
    snapshot: dict[str, Any] | None = None
    state: DaemonState | None = None

    # When False (default on startup), the daemon processes signals and pending
    # spawns but does not run run_iteration() (no auto-spawning from snapshot).
    # Set to True by a start_orchestration signal from the /loom skill.
    orchestration_active: bool = False

    # Pending spawn queue: spawn_shepherd signals that could not be fulfilled
    # immediately (no idle slot available) are held here and retried each
    # iteration until a slot opens or the issue is cancelled.
    # Each entry is a dict: {"issue": int, "mode": str, "flags": list[str]}
    pending_spawns: list[dict] = field(default_factory=list)

    # File paths (computed from repo_root)
    _log_file: pathlib.Path | None = field(default=None, repr=False)
    _state_file: pathlib.Path | None = field(default=None, repr=False)
    _metrics_file: pathlib.Path | None = field(default=None, repr=False)
    _stop_signal: pathlib.Path | None = field(default=None, repr=False)
    _pid_file: pathlib.Path | None = field(default=None, repr=False)
    _signals_dir: pathlib.Path | None = field(default=None, repr=False)

    @property
    def log_file(self) -> pathlib.Path:
        if self._log_file is None:
            self._log_file = self.repo_root / ".loom" / "daemon.log"
        return self._log_file

    @property
    def state_file(self) -> pathlib.Path:
        if self._state_file is None:
            self._state_file = self.repo_root / ".loom" / "daemon-state.json"
        return self._state_file

    @property
    def metrics_file(self) -> pathlib.Path:
        if self._metrics_file is None:
            self._metrics_file = self.repo_root / ".loom" / "daemon-metrics.json"
        return self._metrics_file

    @property
    def stop_signal(self) -> pathlib.Path:
        if self._stop_signal is None:
            self._stop_signal = self.repo_root / ".loom" / "stop-daemon"
        return self._stop_signal

    @property
    def pid_file(self) -> pathlib.Path:
        if self._pid_file is None:
            self._pid_file = self.repo_root / ".loom" / "daemon-loop.pid"
        return self._pid_file

    @property
    def signals_dir(self) -> pathlib.Path:
        """Directory where /loom writes JSON command files for the daemon."""
        if self._signals_dir is None:
            self._signals_dir = self.repo_root / ".loom" / "signals"
        return self._signals_dir

    def get_recommended_actions(self) -> list[str]:
        """Get recommended actions from the current snapshot."""
        if self.snapshot is None:
            return []
        return self.snapshot.get("computed", {}).get("recommended_actions", [])

    def get_available_shepherd_slots(self) -> int:
        """Get number of available shepherd slots from snapshot."""
        if self.snapshot is None:
            return 0
        return self.snapshot.get("computed", {}).get("available_shepherd_slots", 0)

    def get_ready_issues(self) -> list[dict[str, Any]]:
        """Get ready issues from snapshot (sorted by strategy)."""
        if self.snapshot is None:
            return []
        return self.snapshot.get("pipeline", {}).get("ready_issues", [])

    def get_promotable_proposals(self) -> list[int]:
        """Get list of promotable proposal issue numbers."""
        if self.snapshot is None:
            return []
        return self.snapshot.get("computed", {}).get("promotable_proposals", [])
