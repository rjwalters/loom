"""Tests for transient error recovery in daemon completions."""

from __future__ import annotations

import pathlib
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.daemon_v2.actions.completions import (
    MAX_TRANSIENT_RETRIES,
    TRANSIENT_BACKOFF_SECONDS,
    TRANSIENT_ERROR_PATTERNS,
    CompletionEntry,
    _check_transient_error,
    _handle_transient_error_retry,
    _requeue_issue,
    check_completions,
    handle_completion,
)
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.models.daemon_state import (
    DaemonState,
    ShepherdEntry,
    TransientRetryEntry,
)


@pytest.fixture
def ctx(tmp_path: pathlib.Path) -> DaemonContext:
    """Create a test daemon context."""
    config = DaemonConfig()
    repo_root = tmp_path
    (repo_root / ".loom").mkdir()
    c = DaemonContext(config=config, repo_root=repo_root)
    c.state = DaemonState()
    return c


class TestCheckTransientError:
    """Tests for _check_transient_error detection from milestones."""

    def test_no_milestones(self) -> None:
        assert _check_transient_error([]) is False

    def test_no_error_events(self) -> None:
        milestones = [
            {"event": "phase_entered", "data": {"phase": "builder"}},
            {"event": "heartbeat", "data": {"action": "running tests"}},
        ]
        assert _check_transient_error(milestones) is False

    def test_non_transient_error(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "Test suite failed with 3 failures"}},
        ]
        assert _check_transient_error(milestones) is False

    @pytest.mark.parametrize("pattern", TRANSIENT_ERROR_PATTERNS)
    def test_each_transient_pattern(self, pattern: str) -> None:
        milestones = [
            {"event": "error", "data": {"error": f"API call failed: {pattern}"}},
        ]
        assert _check_transient_error(milestones) is True

    def test_transient_error_event_type(self) -> None:
        milestones = [
            {"event": "transient_error", "data": {"error": "some error"}},
        ]
        assert _check_transient_error(milestones) is True

    def test_case_insensitive_matching(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "got a RATE LIMIT EXCEEDED error"}},
        ]
        assert _check_transient_error(milestones) is True

    def test_checks_most_recent_first(self) -> None:
        milestones = [
            {"event": "error", "data": {"error": "500 Internal Server Error"}},
            {"event": "heartbeat", "data": {"action": "recovered"}},
            {"event": "error", "data": {"error": "test failed normally"}},
        ]
        # The most recent error is not transient, but earlier one is
        # Since we check in reverse, the first non-transient error won't stop
        # us from finding the transient one
        assert _check_transient_error(milestones) is True


class TestCheckCompletionsTransientError:
    """Tests for transient error detection in check_completions."""

    def test_errored_with_transient_error_detected(self, ctx: DaemonContext) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            }
        )
        ctx.snapshot = {
            "shepherds": {
                "progress": [
                    {
                        "task_id": "abc1234",
                        "issue": 42,
                        "status": "errored",
                        "milestones": [
                            {"event": "error", "data": {"error": "500 Internal Server Error"}},
                        ],
                    }
                ]
            }
        }

        completions = check_completions(ctx)
        assert len(completions) == 1
        assert completions[0].is_transient_error is True
        assert completions[0].success is False
        assert completions[0].issue == 42

    def test_errored_without_transient_error(self, ctx: DaemonContext) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            }
        )
        ctx.snapshot = {
            "shepherds": {
                "progress": [
                    {
                        "task_id": "abc1234",
                        "issue": 42,
                        "status": "errored",
                        "milestones": [
                            {"event": "error", "data": {"error": "build failed: syntax error"}},
                        ],
                    }
                ]
            }
        }

        completions = check_completions(ctx)
        assert len(completions) == 1
        assert completions[0].is_transient_error is False

    def test_completed_shepherd_not_marked_transient(self, ctx: DaemonContext) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            }
        )
        ctx.snapshot = {
            "shepherds": {
                "progress": [
                    {
                        "task_id": "abc1234",
                        "issue": 42,
                        "status": "completed",
                    }
                ]
            }
        }

        completions = check_completions(ctx)
        assert len(completions) == 1
        assert completions[0].success is True
        assert completions[0].is_transient_error is False


class TestHandleTransientErrorRetry:
    """Tests for _handle_transient_error_retry logic."""

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    def test_first_retry_requeues(self, mock_requeue: MagicMock, ctx: DaemonContext) -> None:
        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=True,
        )

        _handle_transient_error_retry(ctx, completion, "2026-01-01T00:00:00Z")

        mock_requeue.assert_called_once_with(42, 1, "2026-01-01T00:00:00Z")
        assert "42" in ctx.state.transient_retries
        assert ctx.state.transient_retries["42"].retry_count == 1
        assert ctx.state.transient_retries["42"].backoff_until is not None

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    def test_increments_retry_count(self, mock_requeue: MagicMock, ctx: DaemonContext) -> None:
        ctx.state.transient_retries["42"] = TransientRetryEntry(retry_count=1)

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=True,
        )

        _handle_transient_error_retry(ctx, completion, "2026-01-01T00:00:00Z")

        assert ctx.state.transient_retries["42"].retry_count == 2
        mock_requeue.assert_called_once_with(42, 2, "2026-01-01T00:00:00Z")

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    def test_exhausted_retries_does_not_requeue(self, mock_requeue: MagicMock, ctx: DaemonContext) -> None:
        ctx.state.transient_retries["42"] = TransientRetryEntry(
            retry_count=MAX_TRANSIENT_RETRIES
        )

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=True,
        )

        _handle_transient_error_retry(ctx, completion, "2026-01-01T00:00:00Z")

        mock_requeue.assert_not_called()

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    def test_sets_backoff_time(self, mock_requeue: MagicMock, ctx: DaemonContext) -> None:
        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=True,
        )

        _handle_transient_error_retry(ctx, completion, "2026-01-01T00:00:00Z")

        entry = ctx.state.transient_retries["42"]
        assert entry.backoff_until is not None
        assert entry.last_retry_at == "2026-01-01T00:00:00Z"


class TestHandleCompletionIntegration:
    """Integration tests for handle_completion with transient errors."""

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    @patch("loom_tools.daemon_v2.actions.completions._trigger_shepherd_cleanup")
    def test_transient_error_triggers_retry(
        self, mock_cleanup: MagicMock, mock_requeue: MagicMock, ctx: DaemonContext
    ) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            }
        )

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=True,
        )

        handle_completion(ctx, completion)

        mock_requeue.assert_called_once()
        mock_cleanup.assert_not_called()
        # Shepherd should be set to idle
        assert ctx.state.shepherds["shepherd-1"].status == "idle"

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    @patch("loom_tools.daemon_v2.actions.completions._trigger_shepherd_cleanup")
    def test_success_clears_transient_retries(
        self, mock_cleanup: MagicMock, mock_requeue: MagicMock, ctx: DaemonContext
    ) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            },
            transient_retries={
                "42": TransientRetryEntry(retry_count=1),
            },
        )

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=True,
            pr_merged=True,
        )

        handle_completion(ctx, completion)

        # Transient retry tracking should be cleared on success
        assert "42" not in ctx.state.transient_retries
        mock_cleanup.assert_called_once()
        mock_requeue.assert_not_called()

    @patch("loom_tools.daemon_v2.actions.completions._requeue_issue")
    @patch("loom_tools.daemon_v2.actions.completions._trigger_shepherd_cleanup")
    def test_non_transient_error_no_retry(
        self, mock_cleanup: MagicMock, mock_requeue: MagicMock, ctx: DaemonContext
    ) -> None:
        ctx.state = DaemonState(
            shepherds={
                "shepherd-1": ShepherdEntry(
                    status="working", issue=42, task_id="abc1234"
                ),
            },
        )

        completion = CompletionEntry(
            type="shepherd",
            name="shepherd-1",
            issue=42,
            task_id="abc1234",
            success=False,
            is_transient_error=False,
        )

        handle_completion(ctx, completion)

        mock_requeue.assert_not_called()
        mock_cleanup.assert_not_called()


class TestRequeueIssue:
    """Tests for _requeue_issue label swap."""

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_swaps_labels(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = MagicMock(returncode=0)

        _requeue_issue(42, 1, "2026-01-01T00:00:00Z")

        # First call: label swap
        assert mock_gh.call_count == 2
        label_call = mock_gh.call_args_list[0]
        args = label_call[0][0]
        assert "issue" in args
        assert "edit" in args
        assert "42" in args
        assert "--remove-label" in args
        assert "loom:building" in args
        assert "--add-label" in args
        assert "loom:issue" in args

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_adds_comment(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = MagicMock(returncode=0)

        _requeue_issue(42, 2, "2026-01-01T00:00:00Z")

        # Second call: comment
        comment_call = mock_gh.call_args_list[1]
        args = comment_call[0][0]
        assert "issue" in args
        assert "comment" in args
        assert "42" in args

    @patch("loom_tools.daemon_v2.actions.completions.gh_run")
    def test_handles_label_swap_failure(self, mock_gh: MagicMock) -> None:
        mock_gh.return_value = MagicMock(returncode=1)

        # Should not raise
        _requeue_issue(42, 1, "2026-01-01T00:00:00Z")

        # Only one call (label swap fails, no comment added)
        assert mock_gh.call_count == 1


class TestTransientRetryEntry:
    """Tests for TransientRetryEntry model."""

    def test_default_values(self) -> None:
        entry = TransientRetryEntry()
        assert entry.retry_count == 0
        assert entry.last_retry_at is None
        assert entry.max_retries == 3
        assert entry.backoff_until is None

    def test_from_dict(self) -> None:
        data = {
            "retry_count": 2,
            "last_retry_at": "2026-01-01T00:00:00Z",
            "max_retries": 3,
            "backoff_until": "2026-01-01T00:05:00Z",
        }
        entry = TransientRetryEntry.from_dict(data)
        assert entry.retry_count == 2
        assert entry.last_retry_at == "2026-01-01T00:00:00Z"
        assert entry.backoff_until == "2026-01-01T00:05:00Z"

    def test_to_dict(self) -> None:
        entry = TransientRetryEntry(
            retry_count=1,
            last_retry_at="2026-01-01T00:00:00Z",
            backoff_until="2026-01-01T00:05:00Z",
        )
        d = entry.to_dict()
        assert d["retry_count"] == 1
        assert d["last_retry_at"] == "2026-01-01T00:00:00Z"
        assert d["backoff_until"] == "2026-01-01T00:05:00Z"
        assert d["max_retries"] == 3

    def test_roundtrip(self) -> None:
        entry = TransientRetryEntry(retry_count=2, last_retry_at="2026-01-01T00:00:00Z")
        d = entry.to_dict()
        restored = TransientRetryEntry.from_dict(d)
        assert restored.retry_count == entry.retry_count
        assert restored.last_retry_at == entry.last_retry_at


class TestDaemonStateTransientRetries:
    """Tests for transient_retries in DaemonState."""

    def test_default_empty(self) -> None:
        state = DaemonState()
        assert state.transient_retries == {}

    def test_from_dict_with_retries(self) -> None:
        data = {
            "running": True,
            "iteration": 1,
            "transient_retries": {
                "42": {"retry_count": 1, "max_retries": 3},
                "100": {"retry_count": 3, "max_retries": 3},
            },
        }
        state = DaemonState.from_dict(data)
        assert "42" in state.transient_retries
        assert state.transient_retries["42"].retry_count == 1
        assert state.transient_retries["100"].retry_count == 3

    def test_to_dict_includes_retries(self) -> None:
        state = DaemonState(
            transient_retries={
                "42": TransientRetryEntry(retry_count=2),
            }
        )
        d = state.to_dict()
        assert "transient_retries" in d
        assert "42" in d["transient_retries"]
        assert d["transient_retries"]["42"]["retry_count"] == 2

    def test_from_dict_without_retries(self) -> None:
        data = {"running": True, "iteration": 1}
        state = DaemonState.from_dict(data)
        assert state.transient_retries == {}
