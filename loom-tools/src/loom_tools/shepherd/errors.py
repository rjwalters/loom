"""Custom exceptions for shepherd orchestration."""

from __future__ import annotations


class ShepherdError(Exception):
    """Base exception for shepherd errors."""


class ShutdownSignal(ShepherdError):
    """Graceful shutdown requested via stop file or abort label."""


class PhaseValidationError(ShepherdError):
    """Phase contract validation failed."""

    def __init__(self, phase: str, message: str) -> None:
        self.phase = phase
        super().__init__(f"{phase} phase: {message}")


class AgentStuckError(ShepherdError):
    """Agent was stuck after max retries."""

    def __init__(self, phase: str, retries: int) -> None:
        self.phase = phase
        self.retries = retries
        super().__init__(f"{phase} agent stuck after {retries} retry attempt(s)")


class RateLimitError(ShepherdError):
    """API rate limit exceeded."""

    def __init__(self, usage_percent: float, threshold: float) -> None:
        self.usage_percent = usage_percent
        self.threshold = threshold
        super().__init__(
            f"API rate limit exceeded: {usage_percent:.1f}% (threshold: {threshold:.1f}%)"
        )


class IssueNotFoundError(ShepherdError):
    """Issue does not exist."""

    def __init__(self, issue: int) -> None:
        self.issue = issue
        super().__init__(f"Issue #{issue} does not exist")


class IssueBlockedError(ShepherdError):
    """Issue has loom:blocked label."""

    def __init__(self, issue: int) -> None:
        self.issue = issue
        super().__init__(f"Issue #{issue} has loom:blocked label")


class IssueClosedError(ShepherdError):
    """Issue is already closed."""

    def __init__(self, issue: int, state: str) -> None:
        self.issue = issue
        self.state = state
        super().__init__(f"Issue #{issue} is already {state}")


class PRNotFoundError(ShepherdError):
    """Pull request not found for issue."""

    def __init__(self, issue: int) -> None:
        self.issue = issue
        super().__init__(f"No PR found for issue #{issue}")


class WorktreeError(ShepherdError):
    """Worktree operation failed."""

    def __init__(self, issue: int, message: str) -> None:
        self.issue = issue
        super().__init__(f"Worktree for issue #{issue}: {message}")
