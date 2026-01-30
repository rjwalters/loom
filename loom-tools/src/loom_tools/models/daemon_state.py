"""Models for ``.loom/daemon-state.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class ShepherdEntry:
    status: str = "idle"
    issue: int | None = None
    task_id: str | None = None
    output_file: str | None = None
    started: str | None = None
    last_phase: str | None = None
    pr_number: int | None = None
    idle_since: str | None = None
    idle_reason: str | None = None
    last_issue: int | None = None
    last_completed: str | None = None
    execution_mode: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ShepherdEntry:
        return cls(
            status=data.get("status", "idle"),
            issue=data.get("issue"),
            task_id=data.get("task_id"),
            output_file=data.get("output_file"),
            started=data.get("started"),
            last_phase=data.get("last_phase"),
            pr_number=data.get("pr_number"),
            idle_since=data.get("idle_since"),
            idle_reason=data.get("idle_reason"),
            last_issue=data.get("last_issue"),
            last_completed=data.get("last_completed"),
            execution_mode=data.get("execution_mode"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"status": self.status}
        for k in (
            "issue", "task_id", "output_file", "started", "last_phase",
            "pr_number", "idle_since", "idle_reason", "last_issue",
            "last_completed", "execution_mode",
        ):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


@dataclass
class SupportRoleEntry:
    status: str = "idle"
    task_id: str | None = None
    started: str | None = None
    last_completed: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> SupportRoleEntry:
        return cls(
            status=data.get("status", "idle"),
            task_id=data.get("task_id"),
            started=data.get("started"),
            last_completed=data.get("last_completed"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"status": self.status}
        for k in ("task_id", "started", "last_completed"):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d


@dataclass
class Warning:
    time: str = ""
    type: str = ""
    severity: str = "warning"
    message: str = ""
    context: dict[str, Any] = field(default_factory=dict)
    acknowledged: bool = False

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Warning:
        return cls(
            time=data.get("time", ""),
            type=data.get("type", ""),
            severity=data.get("severity", "warning"),
            message=data.get("message", ""),
            context=data.get("context", {}),
            acknowledged=data.get("acknowledged", False),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "time": self.time,
            "type": self.type,
            "severity": self.severity,
            "message": self.message,
            "context": self.context,
            "acknowledged": self.acknowledged,
        }


@dataclass
class PipelineState:
    ready: list[str] = field(default_factory=list)
    building: list[str] = field(default_factory=list)
    review_requested: list[str] = field(default_factory=list)
    changes_requested: list[str] = field(default_factory=list)
    ready_to_merge: list[str] = field(default_factory=list)
    blocked: list[dict[str, Any]] = field(default_factory=list)
    last_updated: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> PipelineState:
        return cls(
            ready=data.get("ready", []),
            building=data.get("building", []),
            review_requested=data.get("review_requested", []),
            changes_requested=data.get("changes_requested", []),
            ready_to_merge=data.get("ready_to_merge", []),
            blocked=data.get("blocked", []),
            last_updated=data.get("last_updated"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "ready": self.ready,
            "building": self.building,
            "review_requested": self.review_requested,
            "changes_requested": self.changes_requested,
            "ready_to_merge": self.ready_to_merge,
            "blocked": self.blocked,
        }
        if self.last_updated is not None:
            d["last_updated"] = self.last_updated
        return d


@dataclass
class DaemonState:
    started_at: str | None = None
    last_poll: str | None = None
    running: bool = False
    iteration: int = 0
    force_mode: bool = False
    execution_mode: str = "direct"
    daemon_session_id: str | None = None
    shepherds: dict[str, ShepherdEntry] = field(default_factory=dict)
    support_roles: dict[str, SupportRoleEntry] = field(default_factory=dict)
    pipeline_state: PipelineState = field(default_factory=PipelineState)
    warnings: list[Warning] = field(default_factory=list)
    completed_issues: list[int] = field(default_factory=list)
    total_prs_merged: int = 0
    last_architect_trigger: str | None = None
    last_hermit_trigger: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> DaemonState:
        shepherds = {
            k: ShepherdEntry.from_dict(v)
            for k, v in data.get("shepherds", {}).items()
        }
        support_roles = {
            k: SupportRoleEntry.from_dict(v)
            for k, v in data.get("support_roles", {}).items()
        }
        pipeline_raw = data.get("pipeline_state", {})
        pipeline = PipelineState.from_dict(pipeline_raw) if pipeline_raw else PipelineState()
        warnings = [Warning.from_dict(w) for w in data.get("warnings", [])]

        return cls(
            started_at=data.get("started_at"),
            last_poll=data.get("last_poll"),
            running=data.get("running", False),
            iteration=data.get("iteration", 0),
            force_mode=data.get("force_mode", False),
            execution_mode=data.get("execution_mode", "direct"),
            daemon_session_id=data.get("daemon_session_id"),
            shepherds=shepherds,
            support_roles=support_roles,
            pipeline_state=pipeline,
            warnings=warnings,
            completed_issues=data.get("completed_issues", []),
            total_prs_merged=data.get("total_prs_merged", 0),
            last_architect_trigger=data.get("last_architect_trigger"),
            last_hermit_trigger=data.get("last_hermit_trigger"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "running": self.running,
            "iteration": self.iteration,
            "force_mode": self.force_mode,
            "execution_mode": self.execution_mode,
            "shepherds": {k: v.to_dict() for k, v in self.shepherds.items()},
            "support_roles": {k: v.to_dict() for k, v in self.support_roles.items()},
            "pipeline_state": self.pipeline_state.to_dict(),
            "warnings": [w.to_dict() for w in self.warnings],
            "completed_issues": self.completed_issues,
            "total_prs_merged": self.total_prs_merged,
        }
        for k in (
            "started_at", "last_poll", "daemon_session_id",
            "last_architect_trigger", "last_hermit_trigger",
        ):
            v = getattr(self, k)
            if v is not None:
                d[k] = v
        return d
