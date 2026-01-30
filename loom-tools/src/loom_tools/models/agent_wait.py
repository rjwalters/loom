"""Models for agent monitoring wait results and completion states."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any


class WaitStatus(Enum):
    """Exit status for agent monitoring."""

    COMPLETED = "completed"
    TIMEOUT = "timeout"
    SESSION_NOT_FOUND = "session_not_found"
    SIGNAL = "signal"
    STUCK = "stuck"
    ERRORED = "errored"


class SignalType(Enum):
    """Types of shutdown signals detected."""

    SHUTDOWN = "shutdown"
    ABORT = "abort"


class CompletionReason(Enum):
    """Reasons an agent completed its work."""

    EXPLICIT_EXIT = "explicit_exit"
    PHASE_CONTRACT_SATISFIED = "phase_contract_satisfied"
    BUILDER_PR_CREATED = "builder_pr_created"
    JUDGE_REVIEW_COMPLETE = "judge_review_complete"
    DOCTOR_FIXES_COMPLETE = "doctor_fixes_complete"
    CURATOR_CURATION_COMPLETE = "curator_curation_complete"


class StuckAction(Enum):
    """Actions to take when agent is detected as stuck."""

    WARN = "warn"
    PAUSE = "pause"
    RESTART = "restart"
    RETRY = "retry"


@dataclass
class StuckConfig:
    """Configuration for stuck detection thresholds."""

    warning_threshold: int = 300  # 5 minutes
    critical_threshold: int = 600  # 10 minutes
    # Prompt stuck detection: check every 10s, fire after 30s stuck
    prompt_stuck_check_interval: int = 10  # how often to check
    prompt_stuck_age_threshold: int = 30  # how long stuck before detection
    prompt_stuck_recovery_cooldown: int = 60  # seconds before re-trying recovery
    action: StuckAction = StuckAction.WARN

    @classmethod
    def from_env(cls) -> StuckConfig:
        """Load stuck detection config from environment variables."""
        import os

        action_str = os.environ.get("LOOM_STUCK_ACTION", "warn").lower()
        try:
            action = StuckAction(action_str)
        except ValueError:
            action = StuckAction.WARN

        return cls(
            warning_threshold=int(os.environ.get("LOOM_STUCK_WARNING", "300")),
            critical_threshold=int(os.environ.get("LOOM_STUCK_CRITICAL", "600")),
            prompt_stuck_check_interval=int(
                os.environ.get("LOOM_PROMPT_STUCK_CHECK_INTERVAL", "10")
            ),
            prompt_stuck_age_threshold=int(
                os.environ.get("LOOM_PROMPT_STUCK_AGE_THRESHOLD", "30")
            ),
            prompt_stuck_recovery_cooldown=int(
                os.environ.get("LOOM_PROMPT_STUCK_RECOVERY_COOLDOWN", "60")
            ),
            action=action,
        )


@dataclass
class WaitResult:
    """Result from agent monitoring wait operation."""

    status: WaitStatus
    name: str
    elapsed: int = 0
    reason: CompletionReason | None = None
    signal_type: SignalType | None = None
    stuck_status: str | None = None
    stuck_action: str | None = None
    idle_time: int | None = None
    error_message: str | None = None

    def to_dict(self) -> dict[str, Any]:
        """Convert to JSON-serializable dict."""
        d: dict[str, Any] = {
            "status": self.status.value,
            "name": self.name,
            "elapsed": self.elapsed,
        }
        if self.reason is not None:
            d["reason"] = self.reason.value
        if self.signal_type is not None:
            d["signal_type"] = self.signal_type.value
        if self.stuck_status is not None:
            d["stuck_status"] = self.stuck_status
        if self.stuck_action is not None:
            d["action"] = self.stuck_action
        if self.idle_time is not None:
            d["idle_time"] = self.idle_time
        if self.error_message is not None:
            d["error"] = self.error_message
        return d


@dataclass
class ContractCheckResult:
    """Result from phase contract validation."""

    satisfied: bool
    status: str = "not_satisfied"
    message: str = ""
    recovery_action: str = "none"

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> ContractCheckResult:
        """Parse from validate-phase.sh JSON output."""
        status = data.get("status", "unknown")
        return cls(
            satisfied=status in ("satisfied", "recovered"),
            status=status,
            message=data.get("message", ""),
            recovery_action=data.get("recovery_action", "none"),
        )


@dataclass
class MonitorConfig:
    """Configuration for agent monitoring."""

    name: str
    timeout: int = 3600
    poll_interval: int = 5
    issue: int | None = None
    task_id: str | None = None
    phase: str | None = None
    worktree: str | None = None
    pr_number: int | None = None
    idle_timeout: int = 60
    contract_interval: int = 90
    min_idle_elapsed: int = 10
    heartbeat_interval: int = 60
    stuck_config: StuckConfig = field(default_factory=StuckConfig)

    @classmethod
    def from_args(
        cls,
        name: str,
        timeout: int = 3600,
        poll_interval: int = 5,
        issue: int | None = None,
        task_id: str | None = None,
        phase: str | None = None,
        worktree: str | None = None,
        pr_number: int | None = None,
        idle_timeout: int = 60,
        contract_interval: int = 90,
        min_idle_elapsed: int = 10,
    ) -> MonitorConfig:
        """Create config from CLI arguments."""
        return cls(
            name=name,
            timeout=timeout,
            poll_interval=poll_interval,
            issue=issue,
            task_id=task_id,
            phase=phase,
            worktree=worktree,
            pr_number=pr_number,
            idle_timeout=idle_timeout,
            contract_interval=contract_interval,
            min_idle_elapsed=min_idle_elapsed,
            stuck_config=StuckConfig.from_env(),
        )
