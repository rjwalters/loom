"""Tests for Warning generation in force_reclaim_stale_shepherds."""

from __future__ import annotations

import pathlib
import tempfile
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.daemon_v2.actions.shepherds import force_reclaim_stale_shepherds
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    tmp_path: pathlib.Path,
    *,
    force_mode: bool = False,
    shepherds: dict | None = None,
    progress: list | None = None,
) -> DaemonContext:
    """Create a DaemonContext for stall recovery testing."""
    config = DaemonConfig(force_mode=force_mode, startup_grace_period=0)
    ctx = DaemonContext(config=config, repo_root=tmp_path)
    ctx.state = DaemonState()
    ctx.snapshot = {
        "computed": {
            "health_warnings": [],
        },
        "shepherds": {
            "progress": progress or [],
        },
    }
    if shepherds:
        for name, entry_data in shepherds.items():
            ctx.state.shepherds[name] = ShepherdEntry(**entry_data)
    return ctx


@patch("loom_tools.daemon_v2.actions.shepherds.release_claim")
@patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue")
@patch("loom_tools.daemon_v2.actions.shepherds.record_failure")
@patch("loom_tools.daemon_v2.actions.shepherds.kill_stuck_session")
@patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
class TestStallRecoveryWarnings:
    """Tests for Warning generation when shepherds are reclaimed."""

    def test_warning_added_on_stall_recovery(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """A Warning is appended to ctx.state.warnings when a shepherd is reclaimed."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": "2026-01-25T10:00:00Z",
                    "last_phase": "builder",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        w = ctx.state.warnings[0]
        assert w.type == "shepherd_stall_recovery"
        assert w.severity == "warning"
        assert w.acknowledged is False
        assert "42" in w.message
        assert "/shepherd 42 -m" in w.message
        assert w.context["issue"] == 42
        assert w.context["shepherd"] == "shepherd-1"
        assert w.context["requires_role"] == "shepherd"

    def test_warning_includes_phase(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Warning context includes the phase the shepherd was in."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 55,
                    "task_id": "def5678",
                    "started": "2026-01-25T10:00:00Z",
                    "last_phase": "judge",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        assert ctx.state.warnings[0].context["phase"] == "judge"

    def test_no_warning_when_issue_is_none(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """No warning is generated when the shepherd has no assigned issue."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": None,
                    "task_id": "abc1234",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 0

    def test_multiple_shepherds_produce_multiple_warnings(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Each reclaimed shepherd produces its own Warning."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 10,
                    "task_id": "aaa1111",
                    "started": "2026-01-25T10:00:00Z",
                },
                "shepherd-2": {
                    "status": "working",
                    "issue": 20,
                    "task_id": "bbb2222",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 2
        issues = {w.context["issue"] for w in ctx.state.warnings}
        assert issues == {10, 20}

    def test_idle_shepherd_does_not_produce_warning(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Idle shepherds are not reclaimed and generate no warning."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "idle",
                    "issue": None,
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 0

    def test_infrastructure_error_class_from_failure_log(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """When failure log has an infrastructure error class, warning uses it."""
        from loom_tools.common.issue_failures import IssueFailureEntry

        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 99,
                    "task_id": "ccc3333",
                    "started": "2026-01-25T10:00:00Z",
                    "last_phase": "builder",
                },
            },
        )

        existing_entry = IssueFailureEntry(
            issue=99,
            error_class="mcp_infrastructure_failure",
            phase="builder",
        )
        with patch(
            "loom_tools.daemon_v2.actions.shepherds.get_failure_entry",
            return_value=existing_entry,
        ):
            force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        w = ctx.state.warnings[0]
        assert w.type == "shepherd_infrastructure_failure"
        assert "mcp_infrastructure_failure" in w.message
        assert w.context["error_class"] == "mcp_infrastructure_failure"

    def test_auth_infrastructure_error_class(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """auth_infrastructure_failure also produces type=shepherd_infrastructure_failure."""
        from loom_tools.common.issue_failures import IssueFailureEntry

        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 77,
                    "task_id": "ddd4444",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        existing_entry = IssueFailureEntry(
            issue=77,
            error_class="auth_infrastructure_failure",
            phase="curator",
        )
        with patch(
            "loom_tools.daemon_v2.actions.shepherds.get_failure_entry",
            return_value=existing_entry,
        ):
            force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        w = ctx.state.warnings[0]
        assert w.type == "shepherd_infrastructure_failure"
        assert "auth_infrastructure_failure" in w.message

    def test_generic_error_class_from_failure_log_not_overridden(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Failure log with generic error_class still uses the computed error class."""
        from loom_tools.common.issue_failures import IssueFailureEntry

        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 33,
                    "task_id": "eee5555",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        # A "budget_exhausted" entry in the failure log should not override
        existing_entry = IssueFailureEntry(
            issue=33,
            error_class="budget_exhausted",
            phase="builder",
        )
        with patch(
            "loom_tools.daemon_v2.actions.shepherds.get_failure_entry",
            return_value=existing_entry,
        ):
            force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        w = ctx.state.warnings[0]
        assert w.type == "shepherd_stall_recovery"  # Not infrastructure type

    def test_warning_message_contains_respawn_command(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Warning message includes the exact /shepherd <N> -m re-spawn command."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 123,
                    "task_id": "fff6666",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        assert "/shepherd 123 -m" in ctx.state.warnings[0].message

    def test_warning_has_correct_timestamp(
        self, _mock_session, _mock_kill, _mock_record, _mock_unclaim, _mock_release,
        tmp_path,
    ):
        """Warning timestamp is set (non-empty string)."""
        ctx = _make_ctx(
            tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 88,
                    "task_id": "ggg7777",
                    "started": "2026-01-25T10:00:00Z",
                },
            },
        )

        force_reclaim_stale_shepherds(ctx)

        assert len(ctx.state.warnings) == 1
        assert ctx.state.warnings[0].time != ""
