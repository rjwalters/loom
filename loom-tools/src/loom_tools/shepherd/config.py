"""Configuration models for shepherd orchestration."""

from __future__ import annotations

import secrets
from dataclasses import dataclass, field
from enum import Enum

from loom_tools.common.config import env_int


class Phase(Enum):
    """Shepherd orchestration phases."""

    CURATOR = "curator"
    APPROVAL = "approval"
    BUILDER = "builder"
    JUDGE = "judge"
    DOCTOR = "doctor"
    MERGE = "merge"


class ExecutionMode(Enum):
    """Shepherd execution modes.

    - DEFAULT: Create PR, exit at approval (Champion handles merge)
    - FORCE_MERGE: Auto-approve, auto-merge after Judge approval
    - NORMAL: Legacy mode, same as DEFAULT (--wait was deprecated)
    """

    DEFAULT = "force-pr"
    FORCE_MERGE = "force-merge"
    NORMAL = "normal"  # Deprecated, treated same as DEFAULT


# Phase order for --from skipping
PHASE_ORDER = [Phase.CURATOR, Phase.BUILDER, Phase.JUDGE, Phase.MERGE]


def _generate_task_id() -> str:
    """Generate a 7-character lowercase hex task ID."""
    return secrets.token_hex(4)[:7]


@dataclass
class ShepherdConfig:
    """Configuration for shepherd orchestration."""

    # Required
    issue: int

    # Execution mode
    mode: ExecutionMode = ExecutionMode.DEFAULT

    # Task tracking
    task_id: str = field(default_factory=_generate_task_id)

    # Phase control
    start_from: Phase | None = None
    stop_after: str | None = None  # "curated", "pr", "approved"

    # Timeouts (seconds) - loaded from environment or defaults
    curator_timeout: int = field(
        default_factory=lambda: env_int("LOOM_CURATOR_TIMEOUT", 300)
    )
    builder_timeout: int = field(
        default_factory=lambda: env_int("LOOM_BUILDER_TIMEOUT", 1800)
    )
    judge_timeout: int = field(
        default_factory=lambda: env_int("LOOM_JUDGE_TIMEOUT", 600)
    )
    doctor_timeout: int = field(
        default_factory=lambda: env_int("LOOM_DOCTOR_TIMEOUT", 900)
    )
    poll_interval: int = field(
        default_factory=lambda: env_int("LOOM_POLL_INTERVAL", 5)
    )

    # Retry limits
    doctor_max_retries: int = field(
        default_factory=lambda: env_int("LOOM_DOCTOR_MAX_RETRIES", 3)
    )
    judge_max_retries: int = field(
        default_factory=lambda: env_int("LOOM_JUDGE_MAX_RETRIES", 1)
    )
    stuck_max_retries: int = field(
        default_factory=lambda: env_int("LOOM_STUCK_MAX_RETRIES", 1)
    )

    # Rate limiting
    rate_limit_threshold: int = field(
        default_factory=lambda: env_int("LOOM_RATE_LIMIT_THRESHOLD", 99)
    )

    # Worktree marker file name
    worktree_marker_file: str = ".loom-in-use"

    @property
    def is_force_mode(self) -> bool:
        """True if running in force mode (auto-approve, auto-merge)."""
        return self.mode == ExecutionMode.FORCE_MERGE

    def should_skip_phase(self, phase: Phase) -> bool:
        """Check if a phase should be skipped based on --from argument.

        Returns True if the phase should be skipped, False if it should run.
        """
        if self.start_from is None:
            return False

        try:
            start_idx = PHASE_ORDER.index(self.start_from)
            phase_idx = PHASE_ORDER.index(phase)
            return phase_idx < start_idx
        except ValueError:
            # Phase not in order (e.g., APPROVAL, DOCTOR) - don't skip
            return False

    def get_phase_timeout(self, phase: Phase) -> int:
        """Get timeout for a specific phase."""
        timeouts = {
            Phase.CURATOR: self.curator_timeout,
            Phase.BUILDER: self.builder_timeout,
            Phase.JUDGE: self.judge_timeout,
            Phase.DOCTOR: self.doctor_timeout,
        }
        return timeouts.get(phase, 300)
