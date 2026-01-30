"""Tests for shepherd error classes."""

from __future__ import annotations

import pytest

from loom_tools.shepherd.errors import (
    AgentStuckError,
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
    PhaseValidationError,
    PRNotFoundError,
    RateLimitError,
    ShepherdError,
    ShutdownSignal,
    WorktreeError,
)


class TestShepherdError:
    """Test base ShepherdError."""

    def test_is_exception(self) -> None:
        """ShepherdError should be an Exception."""
        err = ShepherdError("test error")
        assert isinstance(err, Exception)

    def test_message(self) -> None:
        """Error message should be preserved."""
        err = ShepherdError("test error")
        assert str(err) == "test error"


class TestShutdownSignal:
    """Test ShutdownSignal exception."""

    def test_inherits_from_shepherd_error(self) -> None:
        """ShutdownSignal should inherit from ShepherdError."""
        err = ShutdownSignal("shutdown requested")
        assert isinstance(err, ShepherdError)


class TestPhaseValidationError:
    """Test PhaseValidationError exception."""

    def test_stores_phase(self) -> None:
        """Error should store phase name."""
        err = PhaseValidationError("builder", "no PR found")
        assert err.phase == "builder"

    def test_formats_message(self) -> None:
        """Error message should include phase name."""
        err = PhaseValidationError("builder", "no PR found")
        assert str(err) == "builder phase: no PR found"


class TestAgentStuckError:
    """Test AgentStuckError exception."""

    def test_stores_phase_and_retries(self) -> None:
        """Error should store phase and retry count."""
        err = AgentStuckError("builder", 3)
        assert err.phase == "builder"
        assert err.retries == 3

    def test_formats_message(self) -> None:
        """Error message should include phase and retries."""
        err = AgentStuckError("builder", 3)
        assert str(err) == "builder agent stuck after 3 retry attempt(s)"


class TestRateLimitError:
    """Test RateLimitError exception."""

    def test_stores_usage_and_threshold(self) -> None:
        """Error should store usage and threshold values."""
        err = RateLimitError(95.5, 90.0)
        assert err.usage_percent == 95.5
        assert err.threshold == 90.0

    def test_formats_message(self) -> None:
        """Error message should include usage percentage."""
        err = RateLimitError(95.5, 90.0)
        assert "95.5%" in str(err)
        assert "90.0%" in str(err)


class TestIssueNotFoundError:
    """Test IssueNotFoundError exception."""

    def test_stores_issue(self) -> None:
        """Error should store issue number."""
        err = IssueNotFoundError(42)
        assert err.issue == 42

    def test_formats_message(self) -> None:
        """Error message should include issue number."""
        err = IssueNotFoundError(42)
        assert str(err) == "Issue #42 does not exist"


class TestIssueBlockedError:
    """Test IssueBlockedError exception."""

    def test_stores_issue(self) -> None:
        """Error should store issue number."""
        err = IssueBlockedError(42)
        assert err.issue == 42

    def test_formats_message(self) -> None:
        """Error message should include issue number."""
        err = IssueBlockedError(42)
        assert str(err) == "Issue #42 has loom:blocked label"


class TestIssueClosedError:
    """Test IssueClosedError exception."""

    def test_stores_issue_and_state(self) -> None:
        """Error should store issue number and state."""
        err = IssueClosedError(42, "CLOSED")
        assert err.issue == 42
        assert err.state == "CLOSED"

    def test_formats_message(self) -> None:
        """Error message should include issue number and state."""
        err = IssueClosedError(42, "CLOSED")
        assert str(err) == "Issue #42 is already CLOSED"


class TestPRNotFoundError:
    """Test PRNotFoundError exception."""

    def test_stores_issue(self) -> None:
        """Error should store issue number."""
        err = PRNotFoundError(42)
        assert err.issue == 42

    def test_formats_message(self) -> None:
        """Error message should include issue number."""
        err = PRNotFoundError(42)
        assert str(err) == "No PR found for issue #42"


class TestWorktreeError:
    """Test WorktreeError exception."""

    def test_stores_issue(self) -> None:
        """Error should store issue number."""
        err = WorktreeError(42, "creation failed")
        assert err.issue == 42

    def test_formats_message(self) -> None:
        """Error message should include issue number and message."""
        err = WorktreeError(42, "creation failed")
        assert str(err) == "Worktree for issue #42: creation failed"
