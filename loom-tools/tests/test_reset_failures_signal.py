"""Tests for reset_failures signal handling in daemon loop."""

from __future__ import annotations

import json
import pathlib
from unittest import mock

import pytest

from loom_tools.common.issue_failures import record_failure
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.loop import _handle_reset_failures
from loom_tools.models.daemon_state import (
    BlockedIssueRetry,
    DaemonState,
    RecentFailure,
    SystematicFailure,
)


@pytest.fixture
def repo(tmp_path: pathlib.Path) -> pathlib.Path:
    """Create a minimal repo with .loom directory."""
    (tmp_path / ".git").mkdir()
    loom_dir = tmp_path / ".loom"
    loom_dir.mkdir()
    return tmp_path


def _make_ctx(
    repo: pathlib.Path,
    blocked_retries: dict[str, BlockedIssueRetry] | None = None,
    recent_failures: list[RecentFailure] | None = None,
) -> DaemonContext:
    config = DaemonConfig()
    ctx = DaemonContext(config=config, repo_root=repo)
    ctx.state = DaemonState(
        blocked_issue_retries=blocked_retries or {},
        recent_failures=recent_failures or [],
        systematic_failure=SystematicFailure(
            active=True, pattern="builder_stuck", count=3
        ),
        needs_human_input=[
            {"type": "exhausted_retry", "issue": 42, "error_class": "builder_stuck"},
            {"type": "other", "reason": "manual"},
        ],
    )
    return ctx


class TestResetFailuresSignalSingleIssue:
    def test_clears_in_memory_state(self, repo: pathlib.Path) -> None:
        ctx = _make_ctx(
            repo,
            blocked_retries={
                "42": BlockedIssueRetry(retry_count=3, retry_exhausted=True),
                "99": BlockedIssueRetry(retry_count=1),
            },
            recent_failures=[
                RecentFailure(issue=42, error_class="builder_stuck"),
                RecentFailure(issue=99, error_class="judge_stuck"),
                RecentFailure(issue=42, error_class="builder_stuck"),
            ],
        )

        cmd = {"action": "reset_failures", "issue": 42}
        _handle_reset_failures(ctx, cmd)

        # In-memory state should be updated
        assert "42" not in ctx.state.blocked_issue_retries
        assert "99" in ctx.state.blocked_issue_retries
        assert len(ctx.state.recent_failures) == 1
        assert ctx.state.recent_failures[0].issue == 99
        # needs_human_input should have removed issue 42 escalation
        assert len(ctx.state.needs_human_input) == 1
        assert ctx.state.needs_human_input[0]["type"] == "other"

    def test_clears_persistent_log(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")

        # Need daemon-state.json for the handler
        (repo / ".loom" / "daemon-state.json").write_text(json.dumps({
            "blocked_issue_retries": {},
            "recent_failures": [],
        }))

        ctx = _make_ctx(repo)
        cmd = {"action": "reset_failures", "issue": 42}
        _handle_reset_failures(ctx, cmd)

        from loom_tools.common.issue_failures import load_failure_log
        log = load_failure_log(repo)
        assert "42" not in log.entries
        assert "99" in log.entries


class TestResetFailuresSignalAll:
    def test_clears_all_in_memory_state(self, repo: pathlib.Path) -> None:
        ctx = _make_ctx(
            repo,
            blocked_retries={
                "42": BlockedIssueRetry(retry_count=3),
                "99": BlockedIssueRetry(retry_count=1),
            },
            recent_failures=[
                RecentFailure(issue=42, error_class="builder_stuck"),
                RecentFailure(issue=99, error_class="judge_stuck"),
            ],
        )

        cmd = {"action": "reset_failures", "all": True}
        _handle_reset_failures(ctx, cmd)

        assert ctx.state.blocked_issue_retries == {}
        assert ctx.state.recent_failures == []
        assert ctx.state.systematic_failure.active is False
        assert ctx.state.systematic_failure.pattern == ""
        assert ctx.state.systematic_failure.count == 0
        # Non-exhausted_retry human input should survive
        assert len(ctx.state.needs_human_input) == 1
        assert ctx.state.needs_human_input[0]["type"] == "other"

    def test_clears_all_persistent_entries(self, repo: pathlib.Path) -> None:
        record_failure(repo, 42, error_class="builder_stuck")
        record_failure(repo, 99, error_class="judge_stuck")

        (repo / ".loom" / "daemon-state.json").write_text(json.dumps({
            "blocked_issue_retries": {},
            "recent_failures": [],
        }))

        ctx = _make_ctx(repo)
        cmd = {"action": "reset_failures", "all": True}
        _handle_reset_failures(ctx, cmd)

        from loom_tools.common.issue_failures import load_failure_log
        log = load_failure_log(repo)
        assert log.entries == {}


class TestResetFailuresSignalInvalid:
    def test_warns_on_missing_params(self, repo: pathlib.Path) -> None:
        ctx = _make_ctx(repo)
        cmd = {"action": "reset_failures"}  # Missing both issue and all
        # Should not raise, just log a warning
        _handle_reset_failures(ctx, cmd)
        # State should be unchanged
        assert len(ctx.state.needs_human_input) == 2

    def test_issue_as_string_is_converted(self, repo: pathlib.Path) -> None:
        """Issue number passed as string should still work."""
        record_failure(repo, 42, error_class="builder_stuck")
        (repo / ".loom" / "daemon-state.json").write_text(json.dumps({
            "blocked_issue_retries": {"42": {"retry_count": 1}},
            "recent_failures": [{"issue": 42, "error_class": "builder_stuck"}],
        }))

        ctx = _make_ctx(
            repo,
            blocked_retries={"42": BlockedIssueRetry(retry_count=1)},
            recent_failures=[RecentFailure(issue=42, error_class="builder_stuck")],
        )
        cmd = {"action": "reset_failures", "issue": "42"}  # String, not int
        _handle_reset_failures(ctx, cmd)

        assert "42" not in ctx.state.blocked_issue_retries
