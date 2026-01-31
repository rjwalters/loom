"""Models for ``.loom/health-metrics.json`` and ``.loom/alerts.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from loom_tools.models.base import SerializableMixin


@dataclass
class ThroughputMetric(SerializableMixin):
    issues_per_hour: float = 0.0
    prs_per_hour: float = 0.0


@dataclass
class LatencyMetric(SerializableMixin):
    avg_iteration_seconds: float = 0.0


@dataclass
class QueueDepths(SerializableMixin):
    ready: int = 0
    building: int = 0
    review_requested: int = 0
    changes_requested: int = 0
    ready_to_merge: int = 0


@dataclass
class ErrorRates(SerializableMixin):
    consecutive_failures: int = 0
    success_rate: float = 100.0
    stuck_agents: int = 0


@dataclass
class ResourceUsage(SerializableMixin):
    active_shepherds: int = 0
    session_percent: float = 0.0


@dataclass
class PipelineHealthMetric(SerializableMixin):
    status: str = "healthy"
    blocked_count: int = 0
    retryable_count: int = 0
    permanent_blocked_count: int = 0
    systematic_failure_active: bool = False


@dataclass
class MetricEntry(SerializableMixin):
    timestamp: str = ""
    throughput: ThroughputMetric = field(default_factory=ThroughputMetric)
    latency: LatencyMetric = field(default_factory=LatencyMetric)
    queue_depths: QueueDepths = field(default_factory=QueueDepths)
    error_rates: ErrorRates = field(default_factory=ErrorRates)
    resource_usage: ResourceUsage = field(default_factory=ResourceUsage)
    pipeline_health: PipelineHealthMetric = field(default_factory=PipelineHealthMetric)


@dataclass
class HealthMetrics(SerializableMixin):
    initialized_at: str = ""
    retention_hours: int = 24
    metrics: list[MetricEntry] = field(default_factory=list)
    health_score: int = 100
    health_status: str = "excellent"
    last_updated: str = ""


@dataclass
class Alert:
    """Alert model with custom to_dict for sparse serialization."""

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
        # Custom to_dict: only include acknowledged_at if not None
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
class AlertsFile(SerializableMixin):
    initialized_at: str = ""
    alerts: list[Alert] = field(default_factory=list)
    acknowledged: list[Alert] = field(default_factory=list)
