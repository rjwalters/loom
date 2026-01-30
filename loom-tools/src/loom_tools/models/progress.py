"""Models for ``.loom/progress/shepherd-*.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class Milestone:
    event: str = ""
    timestamp: str = ""
    data: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Milestone:
        return cls(
            event=data.get("event", ""),
            timestamp=data.get("timestamp", ""),
            data=data.get("data", {}),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "event": self.event,
            "timestamp": self.timestamp,
            "data": self.data,
        }


@dataclass
class ShepherdProgress:
    task_id: str = ""
    issue: int = 0
    mode: str = "default"
    started_at: str = ""
    current_phase: str = ""
    last_heartbeat: str | None = None
    status: str = "working"
    milestones: list[Milestone] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ShepherdProgress:
        return cls(
            task_id=data.get("task_id", ""),
            issue=data.get("issue", 0),
            mode=data.get("mode", "default"),
            started_at=data.get("started_at", ""),
            current_phase=data.get("current_phase", ""),
            last_heartbeat=data.get("last_heartbeat"),
            status=data.get("status", "working"),
            milestones=[Milestone.from_dict(m) for m in data.get("milestones", [])],
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "task_id": self.task_id,
            "issue": self.issue,
            "mode": self.mode,
            "started_at": self.started_at,
            "current_phase": self.current_phase,
            "status": self.status,
            "milestones": [m.to_dict() for m in self.milestones],
        }
        if self.last_heartbeat is not None:
            d["last_heartbeat"] = self.last_heartbeat
        return d
