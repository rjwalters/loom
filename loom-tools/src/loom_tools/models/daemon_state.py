"""Deprecated daemon state models — Phase 3.2 stub.

The Python daemon brain (``daemon_v2/``) and the ``.loom/daemon-state.json``
state file were deleted in Phase 3.2 (#3399).  This module is kept as a
minimal stub so that the Phase 3.1.x CLI ports (status.py, completions.py)
that still reference DaemonState as a legacy fallback continue to import
without error.  The stub classes always produce empty / zero / False values.

Phase 3.4 (#3401) will delete this stub along with all remaining
daemon-state read paths in the CLI ports.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class ShepherdEntry:
    """Stub shepherd entry — always empty."""

    status: str = "idle"
    issue: int | None = None
    pr_number: int | None = None
    task_id: str | None = None
    started: str | None = None
    last_phase: str | None = None
    output_file: str | None = None
    idle_since: str | None = None
    idle_reason: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {}


@dataclass
class SupportRoleEntry:
    """Stub support role entry — always empty."""

    status: str = "idle"
    task_id: str | None = None
    started: str | None = None
    last_completed: str | None = None
    last_result: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {}


@dataclass
class PipelineState:
    """Stub pipeline state — always empty."""

    blocked: list[dict[str, Any]] = field(default_factory=list)
    last_updated: str | None = None


@dataclass
class Warning:
    """Stub warning entry."""

    message: str = ""
    severity: str = "info"
    time: str = ""
    acknowledged: bool = False


@dataclass
class SystematicFailure:
    """Stub systematic failure state."""

    active: bool = False
    pattern: str = ""
    count: int = 0
    detected_at: str | None = None
    cooldown_until: str | None = None
    probe_count: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "active": self.active,
            "pattern": self.pattern,
            "count": self.count,
            "probe_count": self.probe_count,
        }


@dataclass
class BlockedIssueRetry:
    """Stub blocked issue retry info."""

    retry_count: int = 0
    error_class: str = "unknown"
    retry_exhausted: bool = False
    escalated_to_human: bool = False
    last_retry_at: str | None = None


@dataclass
class DaemonState:
    """Stub daemon state — Phase 3.2.

    Always returns empty/zero values. The daemon-state.json producer is deleted.
    All callers that read this class will see an empty state.
    """

    running: bool = False
    started_at: str | None = None
    last_poll: str | None = None
    iteration: int = 0
    daemon_pid: int | None = None
    completed_issues: list[int] = field(default_factory=list)
    total_prs_merged: int = 0
    shepherds: dict[str, ShepherdEntry] = field(default_factory=dict)
    support_roles: dict[str, SupportRoleEntry] = field(default_factory=dict)
    pipeline_state: PipelineState = field(default_factory=PipelineState)
    warnings: list[Warning] = field(default_factory=list)
    systematic_failure: SystematicFailure = field(default_factory=SystematicFailure)
    blocked_issue_retries: dict[str, BlockedIssueRetry] = field(default_factory=dict)
    # recent_failures kept as stub for shepherd/cli.py compatibility (Phase 3.3 removes shepherd)
    recent_failures: list[Any] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "DaemonState":
        """Always returns an empty DaemonState (daemon-state.json is deleted)."""
        return cls()

    def to_dict(self) -> dict[str, Any]:
        return {"running": self.running}
