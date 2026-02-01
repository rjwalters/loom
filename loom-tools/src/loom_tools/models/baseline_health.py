"""Models for ``.loom/baseline-health.json``."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from loom_tools.models.base import SerializableMixin


@dataclass
class FailingTest(SerializableMixin):
    """A single failing test entry."""

    name: str = ""
    ecosystem: str = ""
    failure_message: str = ""


@dataclass
class BaselineHealth(SerializableMixin):
    """Baseline health status for the main branch.

    Written by the Auditor role after validating main branch tests.
    Read by the Shepherd's preflight phase to avoid redundant baseline
    test runs when main is known to be broken.

    Attributes:
        status: One of "healthy", "failing", or "unknown".
        checked_at: ISO 8601 timestamp of the last check.
        main_commit: The HEAD commit of main at time of check.
        failing_tests: List of failing tests (when status is "failing").
        issue_tracking: Issue number tracking the failure (e.g., "#2042").
        cache_ttl_minutes: How long the cache is considered fresh.
    """

    status: str = "unknown"
    checked_at: str = ""
    main_commit: str = ""
    failing_tests: list[FailingTest] = field(default_factory=list)
    issue_tracking: str = ""
    cache_ttl_minutes: int = 15
