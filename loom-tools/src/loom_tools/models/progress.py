"""Models for ``.loom/progress/shepherd-*.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from loom_tools.models.base import SerializableMixin


@dataclass
class Milestone(SerializableMixin):
    event: str = ""
    timestamp: str = ""
    data: dict[str, Any] = field(default_factory=dict)


@dataclass
class ShepherdProgress(SerializableMixin):
    task_id: str = ""
    issue: int = 0
    mode: str = "default"
    started_at: str = ""
    current_phase: str = ""
    last_heartbeat: str | None = None
    status: str = "working"
    milestones: list[Milestone] = field(default_factory=list)
