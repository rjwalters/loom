"""Tests for no-progress-file shepherd detection.

Covers the scenario where a shepherd is spawned but never creates a
progress file (e.g., stuck at a permission prompt). The daemon should
detect and reclaim such shepherds after the grace period.
"""

from __future__ import annotations

import json
import pathlib
from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.daemon_v2.actions.shepherds import (
    NO_PROGRESS_GRACE_PERIOD,
    _check_no_progress_file,
    force_reclaim_stale_shepherds,
)
from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import _reclaim_stale_shepherds
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    *,
    repo_root: pathlib.Path | None = None,
    shepherds: dict[str, dict] | None = None,
    progress: list[dict] | None = None,
) -> DaemonContext:
    """Create a DaemonContext for testing."""
    config = DaemonConfig()
    ctx = DaemonContext(
        config=config,
        repo_root=repo_root or pathlib.Path("/tmp/test-repo"),
    )
    ctx.snapshot = {
        "computed": {
            "health_status": "healthy",
            "health_warnings": [],
            "total_ready": 0,
            "active_shepherds": 0,
            "available_shepherd_slots": 3,
        },
        "shepherds": {
            "progress": progress or [],
        },
    }
    state = DaemonState()
    if shepherds:
        for name, entry_data in shepherds.items():
            state.shepherds[name] = ShepherdEntry(**entry_data)
    ctx.state = state
    return ctx


def _ts(seconds_ago: int) -> str:
    """Return an ISO timestamp N seconds in the past."""
    dt = datetime.now(timezone.utc) - timedelta(seconds=seconds_ago)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


class TestCheckNoProgressFile:
    """Tests for _check_no_progress_file helper."""

    def test_no_started_timestamp(self) -> None:
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
        })
        entry = ctx.state.shepherds["shepherd-1"]
        # started is None — should not flag
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_no_task_id(self) -> None:
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42, "started": _ts(600)},
        })
        entry = ctx.state.shepherds["shepherd-1"]
        # task_id is None — should not flag
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_within_grace_period(self) -> None:
        """Shepherd spawned less than grace period ago should NOT be flagged."""
        ctx = _make_ctx(shepherds={
            "shepherd-1": {
                "status": "working",
                "issue": 42,
                "task_id": "abc1234",
                "started": _ts(60),  # 60 seconds ago — within grace period
            },
        })
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_past_grace_period_no_progress_file(self, tmp_path: pathlib.Path) -> None:
        """Shepherd past grace period with no progress file should be flagged."""
        # Create .loom/progress dir but no progress file
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(600),  # 10 minutes ago — past grace period
                },
            },
        )
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is True

    def test_past_grace_period_with_progress_file(self, tmp_path: pathlib.Path) -> None:
        """Shepherd past grace period WITH a progress file should NOT be flagged."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        # Create matching progress file
        (progress_dir / "shepherd-abc1234.json").write_text(json.dumps({
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
        }))

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(600),
                },
            },
        )
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_past_grace_period_with_snapshot_progress(self, tmp_path: pathlib.Path) -> None:
        """Shepherd with matching issue in snapshot progress should NOT be flagged.

        The task_id in daemon-state may not match the progress file name, so
        we also check if any snapshot progress entry tracks this issue.
        """
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(600),
                },
            },
            progress=[
                {"task_id": "different", "issue": 42, "status": "working"},
            ],
        )
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_grace_period_constant(self) -> None:
        """Verify the grace period constant matches the expected value."""
        assert NO_PROGRESS_GRACE_PERIOD == 600


class TestForceReclaimNoProgress:
    """Tests for no-progress-file detection in force_reclaim_stale_shepherds."""

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_reclaims_shepherd_with_no_progress(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd past grace period without progress file gets reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(600),
                },
            },
        )

        with patch("loom_tools.daemon_v2.actions.shepherds.kill_stuck_session"):
            with patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue"):
                reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 1
        assert ctx.state.shepherds["shepherd-1"].status == "idle"
        assert ctx.state.shepherds["shepherd-1"].idle_reason == "stall_recovery"

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_does_not_reclaim_within_grace_period(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd within grace period should NOT be reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(60),  # Only 60 seconds ago
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)
        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-1"].status == "working"

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_does_not_reclaim_with_active_heartbeat(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd with an active progress file should NOT be reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        # Create matching progress file
        (progress_dir / "shepherd-abc1234.json").write_text(json.dumps({
            "task_id": "abc1234",
            "issue": 42,
            "status": "working",
        }))

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(600),
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)
        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-1"].status == "working"

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_idle_shepherd_unaffected(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Idle shepherds should never be reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {"status": "idle"},
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)
        assert reclaimed == 0


class TestProactiveReclaim:
    """Tests for _reclaim_stale_shepherds called every iteration."""

    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    def test_calls_reclaim_when_working_shepherds(self, mock_reclaim) -> None:
        """Reclaim should be called when there are working shepherds."""
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
        })
        _reclaim_stale_shepherds(ctx)
        mock_reclaim.assert_called_once_with(ctx)

    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds")
    def test_skips_when_no_working_shepherds(self, mock_reclaim) -> None:
        """Reclaim should NOT be called when all shepherds are idle."""
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "idle"},
            "shepherd-2": {"status": "idle"},
        })
        _reclaim_stale_shepherds(ctx)
        mock_reclaim.assert_not_called()

    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds")
    def test_skips_when_no_state(self, mock_reclaim) -> None:
        """Reclaim should not crash when state is None."""
        ctx = _make_ctx()
        ctx.state = None
        _reclaim_stale_shepherds(ctx)
        mock_reclaim.assert_not_called()

    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds")
    def test_skips_when_no_snapshot(self, mock_reclaim) -> None:
        """Reclaim should not crash when snapshot is None."""
        ctx = _make_ctx()
        ctx.snapshot = None
        _reclaim_stale_shepherds(ctx)
        mock_reclaim.assert_not_called()

    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=2)
    def test_recomputes_slots_after_reclaim(self, mock_reclaim) -> None:
        """After reclaiming, available slots should be recomputed."""
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42},
            "shepherd-2": {"status": "working", "issue": 43},
        })
        ctx.snapshot["computed"]["active_shepherds"] = 2
        ctx.snapshot["computed"]["available_shepherd_slots"] = 1

        # Simulate reclaim by changing shepherd status
        def reclaim_side_effect(c: DaemonContext) -> int:
            c.state.shepherds["shepherd-1"].status = "idle"
            c.state.shepherds["shepherd-2"].status = "idle"
            return 2

        mock_reclaim.side_effect = reclaim_side_effect
        _reclaim_stale_shepherds(ctx)

        assert ctx.snapshot["computed"]["active_shepherds"] == 0
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 3
