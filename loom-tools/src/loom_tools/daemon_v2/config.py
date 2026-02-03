"""Daemon configuration."""

from __future__ import annotations

import os
from dataclasses import dataclass

from loom_tools.common.config import env_bool, env_int


# Configuration defaults (same as existing daemon.py and snapshot.py)
DEFAULT_POLL_INTERVAL = 120  # seconds
DEFAULT_ITERATION_TIMEOUT = 300  # seconds
DEFAULT_MAX_SHEPHERDS = 3
DEFAULT_ISSUE_THRESHOLD = 3
DEFAULT_MAX_PROPOSALS = 5
DEFAULT_ARCHITECT_COOLDOWN = 1800  # seconds
DEFAULT_HERMIT_COOLDOWN = 1800  # seconds
DEFAULT_GUIDE_INTERVAL = 900  # seconds
DEFAULT_CHAMPION_INTERVAL = 600  # seconds
DEFAULT_DOCTOR_INTERVAL = 300  # seconds
DEFAULT_AUDITOR_INTERVAL = 600  # seconds
DEFAULT_JUDGE_INTERVAL = 300  # seconds


@dataclass
class DaemonConfig:
    """Configuration for the daemon.

    Configuration is loaded from LOOM_* environment variables with sensible defaults.
    """

    # Core daemon settings
    poll_interval: int = DEFAULT_POLL_INTERVAL
    iteration_timeout: int = DEFAULT_ITERATION_TIMEOUT
    force_mode: bool = False
    debug_mode: bool = False

    # Shepherd configuration
    max_shepherds: int = DEFAULT_MAX_SHEPHERDS
    issue_threshold: int = DEFAULT_ISSUE_THRESHOLD
    issue_strategy: str = "fifo"

    # Proposal configuration
    max_proposals: int = DEFAULT_MAX_PROPOSALS

    # Work generation cooldowns
    architect_cooldown: int = DEFAULT_ARCHITECT_COOLDOWN
    hermit_cooldown: int = DEFAULT_HERMIT_COOLDOWN

    # Support role intervals
    guide_interval: int = DEFAULT_GUIDE_INTERVAL
    champion_interval: int = DEFAULT_CHAMPION_INTERVAL
    doctor_interval: int = DEFAULT_DOCTOR_INTERVAL
    auditor_interval: int = DEFAULT_AUDITOR_INTERVAL
    judge_interval: int = DEFAULT_JUDGE_INTERVAL

    @classmethod
    def from_env(
        cls,
        *,
        force_mode: bool = False,
        debug_mode: bool = False,
    ) -> DaemonConfig:
        """Create config from environment variables.

        Args:
            force_mode: Enable force mode (auto-promote, auto-merge)
            debug_mode: Enable debug logging
        """
        return cls(
            poll_interval=env_int("LOOM_POLL_INTERVAL", DEFAULT_POLL_INTERVAL),
            iteration_timeout=env_int("LOOM_ITERATION_TIMEOUT", DEFAULT_ITERATION_TIMEOUT),
            force_mode=force_mode or env_bool("LOOM_FORCE_MODE", False),
            debug_mode=debug_mode or env_bool("LOOM_DEBUG_MODE", False),
            max_shepherds=env_int("LOOM_MAX_SHEPHERDS", DEFAULT_MAX_SHEPHERDS),
            issue_threshold=env_int("LOOM_ISSUE_THRESHOLD", DEFAULT_ISSUE_THRESHOLD),
            issue_strategy=os.environ.get("LOOM_ISSUE_STRATEGY", "fifo"),
            max_proposals=env_int("LOOM_MAX_PROPOSALS", DEFAULT_MAX_PROPOSALS),
            architect_cooldown=env_int("LOOM_ARCHITECT_COOLDOWN", DEFAULT_ARCHITECT_COOLDOWN),
            hermit_cooldown=env_int("LOOM_HERMIT_COOLDOWN", DEFAULT_HERMIT_COOLDOWN),
            guide_interval=env_int("LOOM_GUIDE_INTERVAL", DEFAULT_GUIDE_INTERVAL),
            champion_interval=env_int("LOOM_CHAMPION_INTERVAL", DEFAULT_CHAMPION_INTERVAL),
            doctor_interval=env_int("LOOM_DOCTOR_INTERVAL", DEFAULT_DOCTOR_INTERVAL),
            auditor_interval=env_int("LOOM_AUDITOR_INTERVAL", DEFAULT_AUDITOR_INTERVAL),
            judge_interval=env_int("LOOM_JUDGE_INTERVAL", DEFAULT_JUDGE_INTERVAL),
        )

    def mode_display(self) -> str:
        """Return a display string for the current mode."""
        if self.force_mode and self.debug_mode:
            return "Force + Debug"
        elif self.force_mode:
            return "Force"
        elif self.debug_mode:
            return "Debug"
        return "Normal"
