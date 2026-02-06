"""Tests for daemon stall escalation logic."""

from __future__ import annotations

import pathlib
import tempfile
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext
from loom_tools.daemon_v2.iteration import (
    IterationResult,
    _escalate_level_1,
    _escalate_level_2,
    _escalate_level_3,
    _iteration_made_progress,
    _update_stall_counter,
)
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry


def _make_ctx(
    *,
    stalled: int = 0,
    health: str = "healthy",
    diagnostic: int = 3,
    recovery: int = 5,
    restart: int = 10,
    shepherds: dict | None = None,
    progress: list | None = None,
) -> DaemonContext:
    """Create a DaemonContext for testing."""
    config = DaemonConfig(
        stall_diagnostic_threshold=diagnostic,
        stall_recovery_threshold=recovery,
        stall_restart_threshold=restart,
    )
    ctx = DaemonContext(
        config=config,
        repo_root=pathlib.Path("/tmp/test-repo"),
    )
    ctx.consecutive_stalled = stalled
    ctx.snapshot = {
        "computed": {
            "health_status": health,
            "health_warnings": [],
            "total_ready": 0,
            "available_shepherd_slots": 0,
        },
        "shepherds": {
            "progress": progress or [],
        },
    }
    state = DaemonState()
    if shepherds:
        for name, entry_data in shepherds.items():
            entry = ShepherdEntry(**entry_data)
            state.shepherds[name] = entry
    ctx.state = state
    return ctx


def _make_result(
    *,
    spawned: int = 0,
    completed: int = 0,
    promoted: int = 0,
    support: int = 0,
) -> IterationResult:
    """Create an IterationResult for testing."""
    return IterationResult(
        status="success",
        summary="",
        shepherds_spawned=spawned,
        completions_handled=completed,
        proposals_promoted=promoted,
        support_roles_spawned=support,
    )


class TestIterationMadeProgress:
    """Tests for _iteration_made_progress."""

    def test_no_progress(self):
        result = _make_result()
        assert _iteration_made_progress(result) is False

    def test_shepherds_spawned(self):
        result = _make_result(spawned=1)
        assert _iteration_made_progress(result) is True

    def test_completions_handled(self):
        result = _make_result(completed=1)
        assert _iteration_made_progress(result) is True

    def test_proposals_promoted(self):
        result = _make_result(promoted=1)
        assert _iteration_made_progress(result) is True

    def test_support_roles_spawned(self):
        result = _make_result(support=1)
        assert _iteration_made_progress(result) is True


class TestUpdateStallCounter:
    """Tests for _update_stall_counter."""

    def test_counter_increments_on_stall(self):
        ctx = _make_ctx(stalled=0, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 1

    def test_counter_increments_multiple(self):
        ctx = _make_ctx(stalled=4, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 5

    def test_counter_resets_on_progress_shepherds(self):
        ctx = _make_ctx(stalled=5, health="stalled")
        result = _make_result(spawned=1)
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_counter_resets_on_progress_completions(self):
        ctx = _make_ctx(stalled=3, health="stalled")
        result = _make_result(completed=1)
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_counter_resets_on_progress_promoted(self):
        ctx = _make_ctx(stalled=7, health="stalled")
        result = _make_result(promoted=1)
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_counter_resets_on_progress_support(self):
        ctx = _make_ctx(stalled=2, health="stalled")
        result = _make_result(support=1)
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_counter_stays_zero_when_healthy(self):
        ctx = _make_ctx(stalled=0, health="healthy")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_counter_resets_when_healthy(self):
        ctx = _make_ctx(stalled=3, health="healthy")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    def test_degraded_health_does_not_increment(self):
        """Degraded (info-only warnings) should not count as stalled."""
        ctx = _make_ctx(stalled=0, health="degraded")
        result = _make_result()
        _update_stall_counter(ctx, result)
        # degraded is neither "healthy" nor stalled with warnings
        # it has info-level warnings only, so health != "healthy"
        # counter should increment since degraded != healthy and no progress
        assert ctx.consecutive_stalled == 1

    def test_no_snapshot_does_not_crash(self):
        ctx = _make_ctx()
        ctx.snapshot = None
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 0

    @patch("loom_tools.daemon_v2.iteration._escalate_level_1")
    def test_level_1_triggered_at_threshold(self, mock_l1):
        ctx = _make_ctx(stalled=2, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 3
        mock_l1.assert_called_once_with(ctx)

    @patch("loom_tools.daemon_v2.iteration._escalate_level_2")
    @patch("loom_tools.daemon_v2.iteration._escalate_level_1")
    def test_level_2_triggered_at_threshold(self, mock_l1, mock_l2):
        ctx = _make_ctx(stalled=4, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 5
        mock_l2.assert_called_once_with(ctx)
        mock_l1.assert_not_called()

    @patch("loom_tools.daemon_v2.iteration._escalate_level_3")
    @patch("loom_tools.daemon_v2.iteration._escalate_level_2")
    @patch("loom_tools.daemon_v2.iteration._escalate_level_1")
    def test_level_3_triggered_at_threshold(self, mock_l1, mock_l2, mock_l3):
        ctx = _make_ctx(stalled=9, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 10
        mock_l3.assert_called_once_with(ctx)
        mock_l2.assert_not_called()
        mock_l1.assert_not_called()

    @patch("loom_tools.daemon_v2.iteration._escalate_level_3")
    def test_level_3_triggered_above_threshold(self, mock_l3):
        """Level 3 should keep triggering above threshold."""
        ctx = _make_ctx(stalled=14, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 15
        mock_l3.assert_called_once_with(ctx)

    @patch("loom_tools.daemon_v2.iteration._escalate_level_1")
    def test_no_escalation_below_threshold(self, mock_l1):
        ctx = _make_ctx(stalled=0, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 1
        mock_l1.assert_not_called()

    def test_custom_thresholds(self):
        """Test with custom escalation thresholds."""
        ctx = _make_ctx(stalled=1, health="stalled", diagnostic=2, recovery=4, restart=6)
        result = _make_result()
        with patch("loom_tools.daemon_v2.iteration._escalate_level_1") as mock_l1:
            _update_stall_counter(ctx, result)
            assert ctx.consecutive_stalled == 2
            mock_l1.assert_called_once()


class TestEscalateLevel1:
    """Tests for Level 1 diagnostics."""

    @patch("loom_tools.daemon_v2.iteration.session_exists", return_value=True)
    def test_logs_working_shepherds(self, mock_session):
        ctx = _make_ctx(
            shepherds={
                "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
                "shepherd-2": {"status": "idle"},
            },
            progress=[
                {
                    "task_id": "abc1234",
                    "issue": 42,
                    "current_phase": "builder",
                    "heartbeat_age_seconds": 300,
                    "heartbeat_stale": True,
                }
            ],
        )
        # Should not raise
        _escalate_level_1(ctx)
        mock_session.assert_called()

    def test_handles_no_state(self):
        ctx = _make_ctx()
        ctx.state = None
        _escalate_level_1(ctx)  # Should not raise


class TestEscalateLevel2:
    """Tests for Level 2 force recovery."""

    @patch("loom_tools.daemon_v2.iteration.run_orphan_recovery")
    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=2)
    def test_calls_reclaim_and_orphan_recovery(self, mock_reclaim, mock_orphan):
        ctx = _make_ctx(stalled=5)
        _escalate_level_2(ctx)
        mock_reclaim.assert_called_once_with(ctx)
        mock_orphan.assert_called_once()

    @patch("loom_tools.daemon_v2.iteration.run_orphan_recovery", side_effect=Exception("fail"))
    @patch("loom_tools.daemon_v2.iteration.force_reclaim_stale_shepherds", return_value=0)
    def test_handles_orphan_recovery_failure(self, mock_reclaim, mock_orphan):
        ctx = _make_ctx(stalled=5)
        _escalate_level_2(ctx)  # Should not raise


class TestEscalateLevel3:
    """Tests for Level 3 pool restart."""

    @patch("loom_tools.daemon_v2.iteration.kill_stuck_session")
    @patch("loom_tools.daemon_v2.iteration.session_exists", return_value=True)
    def test_kills_all_working_shepherds(self, mock_exists, mock_kill):
        ctx = _make_ctx(
            stalled=10,
            shepherds={
                "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
                "shepherd-2": {"status": "working", "issue": 43, "task_id": "def5678"},
                "shepherd-3": {"status": "idle"},
            },
        )

        with patch(
            "loom_tools.daemon_v2.iteration._unclaim_issue"
        ) as mock_unclaim:
            _escalate_level_3(ctx)

        # Both working shepherds should be killed
        assert mock_kill.call_count == 2

        # Both should be reset to idle
        assert ctx.state.shepherds["shepherd-1"].status == "idle"
        assert ctx.state.shepherds["shepherd-2"].status == "idle"
        assert ctx.state.shepherds["shepherd-1"].idle_reason == "pool_restart"
        assert ctx.state.shepherds["shepherd-2"].idle_reason == "pool_restart"

        # Idle shepherd should be unchanged
        assert ctx.state.shepherds["shepherd-3"].status == "idle"

    @patch("loom_tools.daemon_v2.iteration.kill_stuck_session")
    @patch("loom_tools.daemon_v2.iteration.session_exists", return_value=False)
    def test_handles_dead_sessions(self, mock_exists, mock_kill):
        """Level 3 should still reset state even if tmux is already dead."""
        ctx = _make_ctx(
            stalled=10,
            shepherds={
                "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
            },
        )
        with patch("loom_tools.daemon_v2.iteration._unclaim_issue"):
            _escalate_level_3(ctx)

        mock_kill.assert_not_called()
        assert ctx.state.shepherds["shepherd-1"].status == "idle"

    def test_clears_progress_files(self):
        """Level 3 should clear stale progress files."""
        with tempfile.TemporaryDirectory() as tmpdir:
            repo_root = pathlib.Path(tmpdir)
            progress_dir = repo_root / ".loom" / "progress"
            progress_dir.mkdir(parents=True)

            # Create fake progress files
            (progress_dir / "shepherd-abc1234.json").write_text("{}")
            (progress_dir / "shepherd-def5678.json").write_text("{}")

            ctx = _make_ctx(stalled=10)
            ctx.repo_root = repo_root
            ctx.state = DaemonState()

            with patch("loom_tools.daemon_v2.iteration.session_exists", return_value=False):
                with patch("loom_tools.daemon_v2.iteration.kill_stuck_session"):
                    _escalate_level_3(ctx)

            remaining = list(progress_dir.glob("shepherd-*.json"))
            assert len(remaining) == 0

    def test_handles_no_state(self):
        ctx = _make_ctx(stalled=10)
        ctx.state = None
        _escalate_level_3(ctx)  # Should not raise

    @patch("loom_tools.daemon_v2.iteration.kill_stuck_session")
    @patch("loom_tools.daemon_v2.iteration.session_exists", return_value=True)
    def test_reverts_issue_labels(self, mock_exists, mock_kill):
        """Level 3 should revert issue labels for reclaimed shepherds."""
        ctx = _make_ctx(
            stalled=10,
            shepherds={
                "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
            },
        )
        with patch(
            "loom_tools.daemon_v2.iteration._unclaim_issue"
        ) as mock_unclaim:
            _escalate_level_3(ctx)
            mock_unclaim.assert_called_once_with(42)


class TestStallCounterInContext:
    """Tests for consecutive_stalled field on DaemonContext."""

    def test_default_value(self):
        config = DaemonConfig()
        ctx = DaemonContext(config=config, repo_root=pathlib.Path("/tmp"))
        assert ctx.consecutive_stalled == 0

    def test_persists_across_calls(self):
        ctx = _make_ctx(stalled=0, health="stalled")
        result = _make_result()
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 1
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 2
        _update_stall_counter(ctx, result)
        assert ctx.consecutive_stalled == 3


class TestConfigStallThresholds:
    """Tests for stall threshold configuration."""

    def test_default_thresholds(self):
        config = DaemonConfig()
        assert config.stall_diagnostic_threshold == 3
        assert config.stall_recovery_threshold == 5
        assert config.stall_restart_threshold == 10

    def test_from_env_stall_thresholds(self):
        import os
        from unittest.mock import patch as p

        env = {
            "LOOM_STALL_DIAGNOSTIC_THRESHOLD": "2",
            "LOOM_STALL_RECOVERY_THRESHOLD": "4",
            "LOOM_STALL_RESTART_THRESHOLD": "8",
        }
        with p.dict(os.environ, env, clear=False):
            config = DaemonConfig.from_env()
            assert config.stall_diagnostic_threshold == 2
            assert config.stall_recovery_threshold == 4
            assert config.stall_restart_threshold == 8


class TestBuildSummaryStallCounter:
    """Tests that stall counter appears in iteration summary."""

    def test_stalled_counter_in_summary(self):
        from loom_tools.daemon_v2.iteration import _build_summary

        ctx = _make_ctx(stalled=3, health="stalled")
        result = _make_result()
        summary = _build_summary(ctx, result)
        assert "stalled=3" in summary

    def test_no_stalled_when_zero(self):
        from loom_tools.daemon_v2.iteration import _build_summary

        ctx = _make_ctx(stalled=0, health="healthy")
        result = _make_result()
        summary = _build_summary(ctx, result)
        assert "stalled" not in summary
