"""Models for ``.loom/health-metrics.json`` and ``.loom/alerts.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class ThroughputMetric:
    issues_per_hour: float = 0.0
    prs_per_hour: float = 0.0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ThroughputMetric:
        return cls(
            issues_per_hour=data.get("issues_per_hour", 0.0),
            prs_per_hour=data.get("prs_per_hour", 0.0),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "issues_per_hour": self.issues_per_hour,
            "prs_per_hour": self.prs_per_hour,
        }


@dataclass
class LatencyMetric:
    avg_iteration_seconds: float = 0.0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> LatencyMetric:
        return cls(avg_iteration_seconds=data.get("avg_iteration_seconds", 0.0))

    def to_dict(self) -> dict[str, Any]:
        return {"avg_iteration_seconds": self.avg_iteration_seconds}


@dataclass
class QueueDepths:
    ready: int = 0
    building: int = 0
    review_requested: int = 0
    changes_requested: int = 0
    ready_to_merge: int = 0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> QueueDepths:
        return cls(
            ready=data.get("ready", 0),
            building=data.get("building", 0),
            review_requested=data.get("review_requested", 0),
            changes_requested=data.get("changes_requested", 0),
            ready_to_merge=data.get("ready_to_merge", 0),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "ready": self.ready,
            "building": self.building,
            "review_requested": self.review_requested,
            "changes_requested": self.changes_requested,
            "ready_to_merge": self.ready_to_merge,
        }


@dataclass
class ErrorRates:
    consecutive_failures: int = 0
    success_rate: float = 100.0
    stuck_agents: int = 0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ErrorRates:
        return cls(
            consecutive_failures=data.get("consecutive_failures", 0),
            success_rate=data.get("success_rate", 100.0),
            stuck_agents=data.get("stuck_agents", 0),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "consecutive_failures": self.consecutive_failures,
            "success_rate": self.success_rate,
            "stuck_agents": self.stuck_agents,
        }


@dataclass
class ResourceUsage:
    active_shepherds: int = 0
    session_percent: float = 0.0

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> ResourceUsage:
        return cls(
            active_shepherds=data.get("active_shepherds", 0),
            session_percent=data.get("session_percent", 0.0),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "active_shepherds": self.active_shepherds,
            "session_percent": self.session_percent,
        }


@dataclass
class MetricEntry:
    timestamp: str = ""
    throughput: ThroughputMetric = field(default_factory=ThroughputMetric)
    latency: LatencyMetric = field(default_factory=LatencyMetric)
    queue_depths: QueueDepths = field(default_factory=QueueDepths)
    error_rates: ErrorRates = field(default_factory=ErrorRates)
    resource_usage: ResourceUsage = field(default_factory=ResourceUsage)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> MetricEntry:
        return cls(
            timestamp=data.get("timestamp", ""),
            throughput=ThroughputMetric.from_dict(data.get("throughput", {})),
            latency=LatencyMetric.from_dict(data.get("latency", {})),
            queue_depths=QueueDepths.from_dict(data.get("queue_depths", {})),
            error_rates=ErrorRates.from_dict(data.get("error_rates", {})),
            resource_usage=ResourceUsage.from_dict(data.get("resource_usage", {})),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "timestamp": self.timestamp,
            "throughput": self.throughput.to_dict(),
            "latency": self.latency.to_dict(),
            "queue_depths": self.queue_depths.to_dict(),
            "error_rates": self.error_rates.to_dict(),
            "resource_usage": self.resource_usage.to_dict(),
        }


@dataclass
class HealthMetrics:
    initialized_at: str = ""
    retention_hours: int = 24
    metrics: list[MetricEntry] = field(default_factory=list)
    health_score: int = 100
    health_status: str = "excellent"
    last_updated: str = ""

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> HealthMetrics:
        return cls(
            initialized_at=data.get("initialized_at", ""),
            retention_hours=data.get("retention_hours", 24),
            metrics=[MetricEntry.from_dict(m) for m in data.get("metrics", [])],
            health_score=data.get("health_score", 100),
            health_status=data.get("health_status", "excellent"),
            last_updated=data.get("last_updated", ""),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "initialized_at": self.initialized_at,
            "retention_hours": self.retention_hours,
            "metrics": [m.to_dict() for m in self.metrics],
            "health_score": self.health_score,
            "health_status": self.health_status,
            "last_updated": self.last_updated,
        }


@dataclass
class Alert:
    id: str = ""
    type: str = ""
    severity: str = "info"
    message: str = ""
    timestamp: str = ""
    acknowledged: bool = False
    acknowledged_at: str | None = None
    context: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Alert:
        return cls(
            id=data.get("id", ""),
            type=data.get("type", ""),
            severity=data.get("severity", "info"),
            message=data.get("message", ""),
            timestamp=data.get("timestamp", ""),
            acknowledged=data.get("acknowledged", False),
            acknowledged_at=data.get("acknowledged_at"),
            context=data.get("context", {}),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id,
            "type": self.type,
            "severity": self.severity,
            "message": self.message,
            "timestamp": self.timestamp,
            "acknowledged": self.acknowledged,
            "context": self.context,
        }
        if self.acknowledged_at is not None:
            d["acknowledged_at"] = self.acknowledged_at
        return d


@dataclass
class AlertsFile:
    initialized_at: str = ""
    alerts: list[Alert] = field(default_factory=list)
    acknowledged: list[Alert] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> AlertsFile:
        return cls(
            initialized_at=data.get("initialized_at", ""),
            alerts=[Alert.from_dict(a) for a in data.get("alerts", [])],
            acknowledged=[Alert.from_dict(a) for a in data.get("acknowledged", [])],
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "initialized_at": self.initialized_at,
            "alerts": [a.to_dict() for a in self.alerts],
            "acknowledged": [a.to_dict() for a in self.acknowledged],
        }
