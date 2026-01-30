"""Models for stuck detection output and ``.loom/stuck-history.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class StuckMetrics:
    idle_seconds: int = 0
    working_seconds: int = 0
    loop_count: int = 0
    error_count: int = 0
    heartbeat_age: int | None = None
    current_phase: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckMetrics:
        return cls(
            idle_seconds=data.get("idle_seconds", 0),
            working_seconds=data.get("working_seconds", 0),
            loop_count=data.get("loop_count", 0),
            error_count=data.get("error_count", 0),
            heartbeat_age=data.get("heartbeat_age"),
            current_phase=data.get("current_phase"),
        )

    def to_dict(self) -> dict[str, Any]:
        # Always include all fields to match bash script output format
        return {
            "idle_seconds": self.idle_seconds,
            "working_seconds": self.working_seconds,
            "loop_count": self.loop_count,
            "error_count": self.error_count,
            "heartbeat_age": self.heartbeat_age if self.heartbeat_age is not None else -1,
            "current_phase": self.current_phase if self.current_phase is not None else "unknown",
        }


@dataclass
class StuckThresholds:
    idle: int = 600
    working: int = 1800
    loop: int = 3
    error_spike: int = 5
    heartbeat_stale: int = 120

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckThresholds:
        return cls(
            idle=data.get("idle", 600),
            working=data.get("working", 1800),
            loop=data.get("loop", 3),
            error_spike=data.get("error_spike", 5),
            heartbeat_stale=data.get("heartbeat_stale", 120),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "idle": self.idle,
            "working": self.working,
            "loop": self.loop,
            "error_spike": self.error_spike,
            "heartbeat_stale": self.heartbeat_stale,
        }


@dataclass
class StuckDetection:
    agent_id: str = ""
    issue: int | None = None
    status: str = ""
    stuck: bool = False
    severity: str = "none"
    suggested_intervention: str = "none"
    indicators: list[str] = field(default_factory=list)
    metrics: StuckMetrics = field(default_factory=StuckMetrics)
    thresholds: StuckThresholds = field(default_factory=StuckThresholds)
    checked_at: str = ""

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckDetection:
        return cls(
            agent_id=data.get("agent_id", ""),
            issue=data.get("issue"),
            status=data.get("status", ""),
            stuck=data.get("stuck", False),
            severity=data.get("severity", "none"),
            suggested_intervention=data.get("suggested_intervention", "none"),
            indicators=data.get("indicators", []),
            metrics=StuckMetrics.from_dict(data.get("metrics", {})),
            thresholds=StuckThresholds.from_dict(data.get("thresholds", {})),
            checked_at=data.get("checked_at", ""),
        )

    def to_dict(self) -> dict[str, Any]:
        # Always include all fields to match bash script output format
        # Note: issue comes before status in bash output for working agents
        return {
            "agent_id": self.agent_id,
            "issue": self.issue,  # Always include, may be None/null
            "status": self.status,
            "stuck": self.stuck,
            "severity": self.severity,
            "suggested_intervention": self.suggested_intervention,
            "indicators": self.indicators,
            "metrics": self.metrics.to_dict(),
            "thresholds": self.thresholds.to_dict(),
            "checked_at": self.checked_at,
        }


@dataclass
class StuckHistoryEntry:
    detected_at: str = ""
    detection: StuckDetection = field(default_factory=StuckDetection)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckHistoryEntry:
        return cls(
            detected_at=data.get("detected_at", ""),
            detection=StuckDetection.from_dict(data.get("detection", {})),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "detected_at": self.detected_at,
            "detection": self.detection.to_dict(),
        }


@dataclass
class StuckHistory:
    created_at: str = ""
    entries: list[StuckHistoryEntry] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> StuckHistory:
        return cls(
            created_at=data.get("created_at", ""),
            entries=[StuckHistoryEntry.from_dict(e) for e in data.get("entries", [])],
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "created_at": self.created_at,
            "entries": [e.to_dict() for e in self.entries],
        }
