"""Tests for daemon_v2 retry blocked issues action."""

from __future__ import annotations

import pathlib
from unittest import mock

import pytest

from loom_tools.daemon_v2.actions.retry_blocked import (
    escalate_blocked_issues,
    retry_blocked_issues,
    _retry_single_issue,
    _update_retry_state,
)
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.models.daemon_state import BlockedIssueRetry, DaemonState


def _make_ctx(
    tmp_path: pathlib.Path,
    blocked_retries: dict[str, BlockedIssueRetry] | None = None,
    max_retry_count: int = 3,
) -> DaemonContext:
    """Create a minimal DaemonContext for testing."""
    config = DaemonConfig()
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState(
        blocked_issue_retries=blocked_retries or {},
    )
    ctx.snapshot = {
        "config": {"max_retry_count": max_retry_count},
        "pipeline_health": {"retryable_issues": []},
        "computed": {
            "active_shepherds": 0,
            "available_shepherd_slots": 3,
            "total_ready": 0,
            "total_building": 0,
            "total_blocked": 0,
            "health_status": "healthy",
            "health_warnings": [],
            "recommended_actions": [],
        },
    }
    return ctx


class TestRetryBlockedIssues:
    """Tests for retry_blocked_issues action handler."""

    def test_empty_list_returns_zero(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path)
        assert retry_blocked_issues([], ctx) == 0

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_retries_single_issue(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path)
        mock_gh.return_value = mock.MagicMock(returncode=0)

        retryable = [{"number": 42, "retry_count": 0}]
        result = retry_blocked_issues(retryable, ctx)

        assert result == 1

        # Verify label swap: remove loom:blocked, add loom:issue
        label_call = mock_gh.call_args_list[0]
        args = label_call[0][0]
        assert "issue" in args
        assert "edit" in args
        assert "42" in args
        assert "--remove-label" in args
        assert "loom:blocked" in args
        assert "--add-label" in args
        assert "loom:issue" in args

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_retries_multiple_issues(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path)
        mock_gh.return_value = mock.MagicMock(returncode=0)

        retryable = [
            {"number": 42, "retry_count": 0},
            {"number": 99, "retry_count": 1},
        ]
        result = retry_blocked_issues(retryable, ctx)
        assert result == 2

    def test_missing_number_skipped(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path)
        retryable = [{"retry_count": 0}]
        result = retry_blocked_issues(retryable, ctx)
        assert result == 0

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_updates_retry_count_in_state(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path, blocked_retries={
            "42": BlockedIssueRetry(retry_count=0, error_class="builder_stuck"),
        })
        mock_gh.return_value = mock.MagicMock(returncode=0)

        retryable = [{"number": 42, "retry_count": 0}]
        retry_blocked_issues(retryable, ctx)

        # Verify state was updated
        entry = ctx.state.blocked_issue_retries["42"]
        assert entry.retry_count == 1
        assert entry.last_retry_at is not None

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_creates_retry_entry_if_missing(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """If no BlockedIssueRetry entry exists, one is created."""
        ctx = _make_ctx(tmp_path)
        mock_gh.return_value = mock.MagicMock(returncode=0)

        retryable = [{"number": 42, "retry_count": 0}]
        retry_blocked_issues(retryable, ctx)

        assert "42" in ctx.state.blocked_issue_retries
        entry = ctx.state.blocked_issue_retries["42"]
        assert entry.retry_count == 1

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_adds_comment_with_retry_count(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path)
        mock_gh.return_value = mock.MagicMock(returncode=0)

        retryable = [{"number": 42, "retry_count": 1}]
        retry_blocked_issues(retryable, ctx)

        # Find the comment call (second gh_run call after label edit)
        comment_call = mock_gh.call_args_list[1]
        args = comment_call[0][0]
        assert args[:3] == ["issue", "comment", "42"]
        body = args[args.index("--body") + 1]
        assert "attempt 2" in body
        assert "2/3" in body


class TestRetryIssueAlreadyUnblocked:
    """Edge case: issue no longer has loom:blocked label."""

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=False)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_skips_issue_without_blocked_label(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path)

        retryable = [{"number": 42, "retry_count": 0}]
        result = retry_blocked_issues(retryable, ctx)

        # Issue skipped, no label swap attempted
        assert result == 0
        mock_gh.assert_not_called()


class TestGhFailureHandling:
    """Verify graceful handling when gh CLI calls fail."""

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_label_swap_failure_returns_false(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        mock_gh.side_effect = Exception("API error")
        ctx = _make_ctx(tmp_path)

        retryable = [{"number": 42, "retry_count": 0}]
        result = retry_blocked_issues(retryable, ctx)
        assert result == 0

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked._issue_has_label", return_value=True)
    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_comment_failure_still_succeeds(
        self, mock_gh: mock.MagicMock, mock_has_label: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """Label swap succeeds but comment fails - issue should still count as retried."""
        def side_effect(args, **kwargs):
            if "comment" in args:
                raise Exception("Comment API error")
            return mock.MagicMock(returncode=0)

        mock_gh.side_effect = side_effect
        ctx = _make_ctx(tmp_path)

        retryable = [{"number": 42, "retry_count": 0}]
        result = retry_blocked_issues(retryable, ctx)
        assert result == 1


class TestUpdateRetryState:
    """Tests for _update_retry_state helper."""

    def test_updates_existing_entry(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path, blocked_retries={
            "42": BlockedIssueRetry(
                retry_count=1,
                error_class="builder_stuck",
                last_blocked_phase="builder",
            ),
        })

        _update_retry_state(ctx, 42, 2, "2026-01-30T10:00:00Z")

        entry = ctx.state.blocked_issue_retries["42"]
        assert entry.retry_count == 2
        assert entry.last_retry_at == "2026-01-30T10:00:00Z"
        # Original fields preserved
        assert entry.error_class == "builder_stuck"
        assert entry.last_blocked_phase == "builder"

    def test_creates_new_entry(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path)

        _update_retry_state(ctx, 42, 1, "2026-01-30T10:00:00Z")

        assert "42" in ctx.state.blocked_issue_retries
        entry = ctx.state.blocked_issue_retries["42"]
        assert entry.retry_count == 1
        assert entry.last_retry_at == "2026-01-30T10:00:00Z"

    def test_noop_when_no_state(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path)
        ctx.state = None
        # Should not raise
        _update_retry_state(ctx, 42, 1, "2026-01-30T10:00:00Z")


class TestIterationDispatch:
    """Verify iteration.py correctly dispatches retry_blocked_issues action."""

    @mock.patch("loom_tools.daemon_v2.iteration.retry_blocked_issues")
    @mock.patch("loom_tools.daemon_v2.iteration.build_snapshot")
    @mock.patch("loom_tools.daemon_v2.iteration.read_daemon_state")
    @mock.patch("loom_tools.daemon_v2.iteration.write_json_file")
    @mock.patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    def test_dispatches_retry_blocked_action(
        self,
        mock_reclaim: mock.MagicMock,
        mock_write: mock.MagicMock,
        mock_read_state: mock.MagicMock,
        mock_snapshot: mock.MagicMock,
        mock_retry: mock.MagicMock,
        tmp_path: pathlib.Path,
    ) -> None:
        from loom_tools.daemon_v2.iteration import run_iteration

        retryable = [{"number": 42, "retry_count": 0}]

        mock_snapshot.return_value = {
            "timestamp": "2026-01-30T18:00:00Z",
            "pipeline": {"ready_issues": []},
            "proposals": {},
            "prs": {},
            "shepherds": {"progress": [], "stale_heartbeat_count": 0},
            "validation": {"orphaned": [], "invalid_task_ids": []},
            "support_roles": {},
            "pipeline_health": {
                "status": "stalled",
                "retryable_count": 1,
                "retryable_issues": retryable,
            },
            "systematic_failure": {},
            "preflight": {},
            "usage": {"session_percent": 50},
            "ci_status": None,
            "tmux_pool": {},
            "config": {"max_retry_count": 3},
            "computed": {
                "active_shepherds": 0,
                "available_shepherd_slots": 3,
                "total_ready": 0,
                "total_building": 0,
                "total_blocked": 1,
                "total_proposals": 0,
                "needs_work_generation": False,
                "recommended_actions": ["retry_blocked_issues"],
                "promotable_proposals": [],
                "health_status": "stalled",
                "health_warnings": [],
            },
        }
        mock_read_state.return_value = DaemonState()
        mock_retry.return_value = 1

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        (loom_dir / "daemon-state.json").write_text("{}")
        (loom_dir / "progress").mkdir()

        config = DaemonConfig()
        ctx = DaemonContext(config=config, repo_root=tmp_path, iteration=1)

        run_iteration(ctx)

        # Verify retry_blocked_issues was called with the retryable data
        mock_retry.assert_called_once_with(retryable, ctx)

    @mock.patch("loom_tools.daemon_v2.iteration.retry_blocked_issues")
    @mock.patch("loom_tools.daemon_v2.iteration.build_snapshot")
    @mock.patch("loom_tools.daemon_v2.iteration.read_daemon_state")
    @mock.patch("loom_tools.daemon_v2.iteration.write_json_file")
    @mock.patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    def test_does_not_dispatch_when_action_not_recommended(
        self,
        mock_reclaim: mock.MagicMock,
        mock_write: mock.MagicMock,
        mock_read_state: mock.MagicMock,
        mock_snapshot: mock.MagicMock,
        mock_retry: mock.MagicMock,
        tmp_path: pathlib.Path,
    ) -> None:
        from loom_tools.daemon_v2.iteration import run_iteration

        mock_snapshot.return_value = {
            "timestamp": "2026-01-30T18:00:00Z",
            "pipeline": {"ready_issues": []},
            "proposals": {},
            "prs": {},
            "shepherds": {"progress": [], "stale_heartbeat_count": 0},
            "validation": {"orphaned": [], "invalid_task_ids": []},
            "support_roles": {},
            "pipeline_health": {"retryable_issues": []},
            "systematic_failure": {},
            "preflight": {},
            "usage": {"session_percent": 50},
            "ci_status": None,
            "tmux_pool": {},
            "config": {},
            "computed": {
                "active_shepherds": 0,
                "available_shepherd_slots": 3,
                "total_ready": 2,
                "total_building": 0,
                "total_blocked": 0,
                "total_proposals": 0,
                "needs_work_generation": False,
                "recommended_actions": ["spawn_shepherds"],
                "promotable_proposals": [],
                "health_status": "healthy",
                "health_warnings": [],
            },
        }
        mock_read_state.return_value = DaemonState()

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        (loom_dir / "daemon-state.json").write_text("{}")
        (loom_dir / "progress").mkdir()

        config = DaemonConfig()
        ctx = DaemonContext(config=config, repo_root=tmp_path, iteration=1)

        with mock.patch("loom_tools.daemon_v2.iteration.spawn_shepherds", return_value=0):
            run_iteration(ctx)

        # retry_blocked_issues should NOT have been called
        mock_retry.assert_not_called()


class TestEscalateBlockedIssues:
    """Tests for escalate_blocked_issues action handler."""

    def test_empty_list_returns_zero(self, tmp_path: pathlib.Path) -> None:
        ctx = _make_ctx(tmp_path)
        assert escalate_blocked_issues([], ctx) == 0

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_escalates_single_issue(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path, blocked_retries={
            "42": BlockedIssueRetry(
                retry_count=2,
                error_class="builder_test_failure",
                escalated_to_human=False,
            ),
        })
        mock_gh.return_value = mock.MagicMock(returncode=0)

        escalation = [{"number": 42, "error_class": "builder_test_failure", "retry_count": 2, "reason": "Exceeded 2 retries"}]
        result = escalate_blocked_issues(escalation, ctx)

        assert result == 1
        # Verify comment was posted
        call_args = mock_gh.call_args[0][0]
        assert "comment" in call_args
        assert "42" in call_args

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_marks_escalated_in_state(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path, blocked_retries={
            "42": BlockedIssueRetry(
                retry_count=2,
                error_class="builder_test_failure",
                escalated_to_human=False,
            ),
        })
        mock_gh.return_value = mock.MagicMock(returncode=0)

        escalation = [{"number": 42, "error_class": "builder_test_failure", "retry_count": 2, "reason": "test"}]
        escalate_blocked_issues(escalation, ctx)

        entry = ctx.state.blocked_issue_retries["42"]
        assert entry.escalated_to_human is True

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_adds_to_needs_human_input(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        ctx = _make_ctx(tmp_path)
        mock_gh.return_value = mock.MagicMock(returncode=0)

        escalation = [{"number": 55, "error_class": "doctor_exhausted", "retry_count": 0, "reason": "immediate"}]
        escalate_blocked_issues(escalation, ctx)

        human_items = ctx.state.needs_human_input
        assert len(human_items) == 1
        assert human_items[0]["type"] == "exhausted_retry"
        assert human_items[0]["issue"] == 55
        assert human_items[0]["error_class"] == "doctor_exhausted"

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_no_duplicate_human_input_entry(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """Calling escalate twice for the same issue only adds one entry."""
        ctx = _make_ctx(tmp_path)
        ctx.state.needs_human_input = [
            {"type": "exhausted_retry", "issue": 42, "error_class": "builder_test_failure", "retry_count": 2}
        ]
        mock_gh.return_value = mock.MagicMock(returncode=0)

        escalation = [{"number": 42, "error_class": "builder_test_failure", "retry_count": 2, "reason": "test"}]
        escalate_blocked_issues(escalation, ctx)

        # Still only one entry
        assert len(ctx.state.needs_human_input) == 1

    @mock.patch("loom_tools.daemon_v2.actions.retry_blocked.gh_run")
    def test_comment_failure_still_escalates(
        self, mock_gh: mock.MagicMock, tmp_path: pathlib.Path
    ) -> None:
        """Comment failure does not prevent escalation in daemon state."""
        mock_gh.side_effect = Exception("API error")
        ctx = _make_ctx(tmp_path)

        escalation = [{"number": 42, "error_class": "builder_test_failure", "retry_count": 2, "reason": "test"}]
        result = escalate_blocked_issues(escalation, ctx)

        # Still counted as escalated (state was updated)
        assert result == 1
        assert ctx.state.blocked_issue_retries["42"].escalated_to_human is True


class TestRecordBlockedReasonPreservesRetryCount:
    """Verify record_blocked_reason preserves retry_count set by retry handler."""

    def test_preserves_retry_count_from_retry_handler(self, tmp_path: pathlib.Path) -> None:
        """When retry handler sets retry_count=2, record_blocked_reason preserves it."""
        import json
        from loom_tools.common.systematic_failure import record_blocked_reason

        loom_dir = tmp_path / ".loom"
        loom_dir.mkdir(parents=True)
        (tmp_path / ".git").mkdir()

        # Simulate state after retry handler incremented retry_count to 2
        state = {
            "running": True,
            "blocked_issue_retries": {
                "42": {
                    "retry_count": 2,
                    "last_retry_at": "2026-01-30T10:00:00Z",
                    "retry_exhausted": False,
                    "error_class": "builder_stuck",
                    "last_blocked_at": "2026-01-29T10:00:00Z",
                    "last_blocked_phase": "builder",
                    "last_blocked_details": "timed out",
                },
            },
        }
        (loom_dir / "daemon-state.json").write_text(json.dumps(state))

        # Issue fails again - record_blocked_reason should preserve retry_count
        record_blocked_reason(
            tmp_path, 42, error_class="judge_stuck", phase="judge", details="review failed"
        )

        result = json.loads((loom_dir / "daemon-state.json").read_text())
        entry = result["blocked_issue_retries"]["42"]

        # retry_count should be preserved (not reset to 0)
        assert entry["retry_count"] == 2
        # But error metadata should be updated
        assert entry["error_class"] == "judge_stuck"
        assert entry["last_blocked_phase"] == "judge"
        assert entry["last_blocked_details"] == "review failed"
        # last_retry_at should be preserved
        assert entry["last_retry_at"] == "2026-01-30T10:00:00Z"
