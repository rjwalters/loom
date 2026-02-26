"""Daemon configuration."""

from __future__ import annotations

import os
from dataclasses import dataclass

from loom_tools.common.config import env_bool, env_int


# Configuration defaults (same as existing daemon.py and snapshot.py)
DEFAULT_POLL_INTERVAL = 30  # seconds
DEFAULT_ITERATION_TIMEOUT = 300  # seconds
DEFAULT_MAX_SHEPHERDS = 10
DEFAULT_ISSUE_THRESHOLD = 3
DEFAULT_MAX_PROPOSALS = 5
DEFAULT_ARCHITECT_COOLDOWN = 1800  # seconds
DEFAULT_HERMIT_COOLDOWN = 1800  # seconds
DEFAULT_GUIDE_INTERVAL = 900  # seconds
DEFAULT_CHAMPION_INTERVAL = 600  # seconds
DEFAULT_DOCTOR_INTERVAL = 300  # seconds
DEFAULT_AUDITOR_INTERVAL = 600  # seconds
DEFAULT_JUDGE_INTERVAL = 300  # seconds
DEFAULT_CURATOR_INTERVAL = 300  # seconds
DEFAULT_STARTUP_GRACE_PERIOD = 120  # seconds before early warning
DEFAULT_NO_PROGRESS_GRACE_PERIOD = 300  # seconds before hard reclaim
DEFAULT_STALL_DIAGNOSTIC_THRESHOLD = 3  # consecutive stalled iterations
DEFAULT_STALL_RECOVERY_THRESHOLD = 5
DEFAULT_STALL_RESTART_THRESHOLD = 10


@dataclass
class DaemonConfig:
    """Configuration for the daemon.

    Configuration is loaded from LOOM_* environment variables with sensible defaults.
    """

    # Core daemon settings
    poll_interval: int = DEFAULT_POLL_INTERVAL
    iteration_timeout: int = DEFAULT_ITERATION_TIMEOUT
    force_mode: bool = False
    auto_build: bool = False
    debug_mode: bool = False
    timeout_min: int = 0  # 0 = no timeout

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
    curator_interval: int = DEFAULT_CURATOR_INTERVAL

    # Shepherd startup detection thresholds
    startup_grace_period: int = DEFAULT_STARTUP_GRACE_PERIOD
    no_progress_grace_period: int = DEFAULT_NO_PROGRESS_GRACE_PERIOD

    # Stall escalation thresholds
    stall_diagnostic_threshold: int = DEFAULT_STALL_DIAGNOSTIC_THRESHOLD
    stall_recovery_threshold: int = DEFAULT_STALL_RECOVERY_THRESHOLD
    stall_restart_threshold: int = DEFAULT_STALL_RESTART_THRESHOLD

    @classmethod
    def from_env(
        cls,
        *,
        force_mode: bool = False,
        auto_build: bool = False,
        debug_mode: bool = False,
        timeout_min: int = 0,
    ) -> DaemonConfig:
        """Create config from environment variables.

        Args:
            force_mode: Enable force mode (auto-promote, auto-merge)
            auto_build: Enable automatic shepherd spawning from loom:issue queue
            debug_mode: Enable debug logging
            timeout_min: Stop daemon after N minutes (0 = no timeout)
        """
        resolved_force = force_mode or env_bool("LOOM_FORCE_MODE", False)
        # --merge/--force implies --auto-build; also check LOOM_AUTO_BUILD env var
        resolved_auto_build = auto_build or resolved_force or env_bool("LOOM_AUTO_BUILD", False)
        return cls(
            poll_interval=env_int("LOOM_POLL_INTERVAL", DEFAULT_POLL_INTERVAL),
            iteration_timeout=env_int("LOOM_ITERATION_TIMEOUT", DEFAULT_ITERATION_TIMEOUT),
            force_mode=resolved_force,
            auto_build=resolved_auto_build,
            debug_mode=debug_mode or env_bool("LOOM_DEBUG_MODE", False),
            timeout_min=timeout_min or env_int("LOOM_TIMEOUT_MIN", 0),
            max_shepherds=env_int("LOOM_MAX_SHEPHERDS", DEFAULT_MAX_SHEPHERDS),
            issue_threshold=env_int("LOOM_ISSUE_THRESHOLD", DEFAULT_ISSUE_THRESHOLD),
            issue_strategy=os.environ.get("LOOM_ISSUE_STRATEGY", "fifo"),
            max_proposals=env_int("LOOM_MAX_PROPOSALS", DEFAULT_MAX_PROPOSALS),
            startup_grace_period=env_int("LOOM_STARTUP_GRACE_PERIOD", DEFAULT_STARTUP_GRACE_PERIOD),
            no_progress_grace_period=env_int("LOOM_NO_PROGRESS_GRACE_PERIOD", DEFAULT_NO_PROGRESS_GRACE_PERIOD),
            architect_cooldown=env_int("LOOM_ARCHITECT_COOLDOWN", DEFAULT_ARCHITECT_COOLDOWN),
            hermit_cooldown=env_int("LOOM_HERMIT_COOLDOWN", DEFAULT_HERMIT_COOLDOWN),
            guide_interval=env_int("LOOM_GUIDE_INTERVAL", DEFAULT_GUIDE_INTERVAL),
            champion_interval=env_int("LOOM_CHAMPION_INTERVAL", DEFAULT_CHAMPION_INTERVAL),
            doctor_interval=env_int("LOOM_DOCTOR_INTERVAL", DEFAULT_DOCTOR_INTERVAL),
            auditor_interval=env_int("LOOM_AUDITOR_INTERVAL", DEFAULT_AUDITOR_INTERVAL),
            judge_interval=env_int("LOOM_JUDGE_INTERVAL", DEFAULT_JUDGE_INTERVAL),
            curator_interval=env_int("LOOM_CURATOR_INTERVAL", DEFAULT_CURATOR_INTERVAL),
            stall_diagnostic_threshold=env_int(
                "LOOM_STALL_DIAGNOSTIC_THRESHOLD", DEFAULT_STALL_DIAGNOSTIC_THRESHOLD
            ),
            stall_recovery_threshold=env_int(
                "LOOM_STALL_RECOVERY_THRESHOLD", DEFAULT_STALL_RECOVERY_THRESHOLD
            ),
            stall_restart_threshold=env_int(
                "LOOM_STALL_RESTART_THRESHOLD", DEFAULT_STALL_RESTART_THRESHOLD
            ),
        )

    def mode_display(self) -> str:
        """Return a display string for the current mode."""
        parts = []
        if self.force_mode:
            parts.append("Force")
        elif self.auto_build:
            parts.append("Auto-build")
        if self.debug_mode:
            parts.append("Debug")
        return " + ".join(parts) if parts else "Support-only"
