"""Configuration models for shepherd orchestration."""

from __future__ import annotations

import secrets
from dataclasses import dataclass, field
from enum import Enum

from loom_tools.common.config import env_int, env_str


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

    DEFAULT = "default"
    FORCE_MERGE = "force-merge"
    NORMAL = "normal"  # Deprecated, treated same as DEFAULT


# Phase order for --from skipping
PHASE_ORDER = [Phase.CURATOR, Phase.BUILDER, Phase.JUDGE, Phase.MERGE]


class QualityGateLevel(Enum):
    """Severity level for quality gate checks.

    INFO: Log as informational, never blocks
    WARN: Log as warning, never blocks
    BLOCK: Log as error, blocks builder phase
    """

    INFO = "info"
    WARN = "warn"
    BLOCK = "block"


def _parse_quality_gate_level(value: str, default: QualityGateLevel) -> QualityGateLevel:
    """Parse a quality gate level from string.

    Args:
        value: String value (info, warn, block) - case insensitive
        default: Default value if parsing fails

    Returns:
        Parsed QualityGateLevel or default if invalid
    """
    try:
        return QualityGateLevel(value.lower())
    except ValueError:
        return default


@dataclass
class QualityGates:
    """Configuration for quality gate severity levels.

    Each quality check can be configured to:
    - INFO: Log informational message, never blocks
    - WARN: Log warning, never blocks
    - BLOCK: Log error and block builder phase

    Configure via environment variables:
    - LOOM_QUALITY_TEST_PLAN: Test plan section check (default: info)
    - LOOM_QUALITY_FILE_REFS: File references check (default: info)
    - LOOM_QUALITY_ACCEPTANCE: Acceptance criteria check (default: warn)
    - LOOM_QUALITY_VAGUE: Vague criteria check (default: warn)
    """

    test_plan: QualityGateLevel = field(
        default_factory=lambda: _parse_quality_gate_level(
            env_str("LOOM_QUALITY_TEST_PLAN", "info"), QualityGateLevel.INFO
        )
    )
    file_refs: QualityGateLevel = field(
        default_factory=lambda: _parse_quality_gate_level(
            env_str("LOOM_QUALITY_FILE_REFS", "info"), QualityGateLevel.INFO
        )
    )
    acceptance_criteria: QualityGateLevel = field(
        default_factory=lambda: _parse_quality_gate_level(
            env_str("LOOM_QUALITY_ACCEPTANCE", "warn"), QualityGateLevel.WARN
        )
    )
    vague_criteria: QualityGateLevel = field(
        default_factory=lambda: _parse_quality_gate_level(
            env_str("LOOM_QUALITY_VAGUE", "warn"), QualityGateLevel.WARN
        )
    )

    @classmethod
    def strict(cls) -> "QualityGates":
        """Create strict quality gates (acceptance criteria blocks).

        Used with --strict-quality CLI flag.
        """
        return cls(
            test_plan=QualityGateLevel.WARN,
            file_refs=QualityGateLevel.INFO,
            acceptance_criteria=QualityGateLevel.BLOCK,
            vague_criteria=QualityGateLevel.WARN,
        )


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
    # Note: Timeouts set high to avoid killing agents mid-work (see issue #2001)
    curator_timeout: int = field(
        default_factory=lambda: env_int("LOOM_CURATOR_TIMEOUT", 3600)
    )
    builder_timeout: int = field(
        default_factory=lambda: env_int("LOOM_BUILDER_TIMEOUT", 14400)
    )
    judge_timeout: int = field(
        default_factory=lambda: env_int("LOOM_JUDGE_TIMEOUT", 3600)
    )
    approval_timeout: int = field(
        default_factory=lambda: env_int("LOOM_APPROVAL_TIMEOUT", 1800)
    )
    doctor_timeout: int = field(
        default_factory=lambda: env_int("LOOM_DOCTOR_TIMEOUT", 3600)
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
    builder_completion_retries: int = field(
        default_factory=lambda: env_int("LOOM_BUILDER_COMPLETION_RETRIES", 2)
    )
    test_fix_max_retries: int = field(
        default_factory=lambda: env_int("LOOM_TEST_FIX_MAX_RETRIES", 2)
    )

    # Rate limiting
    rate_limit_threshold: int = field(
        default_factory=lambda: env_int("LOOM_RATE_LIMIT_THRESHOLD", 99)
    )

    # Quality gates configuration
    quality_gates: QualityGates = field(default_factory=QualityGates)

    # Worktree marker file name
    worktree_marker_file: str = ".loom-in-use"

    @property
    def is_force_mode(self) -> bool:
        """True if running in force mode (auto-approve, auto-merge)."""
        return self.mode == ExecutionMode.FORCE_MERGE

    @property
    def should_auto_approve(self) -> bool:
        """True if shepherd should auto-promote past the approval gate.

        Both DEFAULT and FORCE_MERGE auto-approve. Only NORMAL (legacy/deprecated) blocks.
        """
        return self.mode in (ExecutionMode.DEFAULT, ExecutionMode.FORCE_MERGE)

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
            Phase.APPROVAL: self.approval_timeout,
            Phase.BUILDER: self.builder_timeout,
            Phase.JUDGE: self.judge_timeout,
            Phase.DOCTOR: self.doctor_timeout,
        }
        return timeouts.get(phase, 300)
