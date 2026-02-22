"""Tests for no-progress-file shepherd detection.

Covers two-tier startup detection:
  Tier 1 (~120s): Early warning — log + tmux capture, do NOT kill.
  Tier 2 (~300s): Hard reclaim — save diagnostic log, kill and reset.

Also covers the scenario where a shepherd is spawned but never creates a
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
    STARTUP_GRACE_PERIOD,
    _check_no_progress_file,
    _save_diagnostic_output,
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


# -----------------------------------------------------------------------
# Constants
# -----------------------------------------------------------------------


class TestConstants:
    """Verify grace period constants match expected values."""

    def test_startup_grace_period(self) -> None:
        assert STARTUP_GRACE_PERIOD == 120

    def test_no_progress_grace_period(self) -> None:
        assert NO_PROGRESS_GRACE_PERIOD == 300

    def test_config_defaults_match_constants(self) -> None:
        cfg = DaemonConfig()
        assert cfg.startup_grace_period == STARTUP_GRACE_PERIOD
        assert cfg.no_progress_grace_period == NO_PROGRESS_GRACE_PERIOD


# -----------------------------------------------------------------------
# _check_no_progress_file — basic cases
# -----------------------------------------------------------------------


class TestCheckNoProgressFile:
    """Tests for _check_no_progress_file helper."""

    def test_no_started_timestamp(self) -> None:
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42, "task_id": "abc1234"},
        })
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_no_task_id(self) -> None:
        ctx = _make_ctx(shepherds={
            "shepherd-1": {"status": "working", "issue": 42, "started": _ts(600)},
        })
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False

    def test_within_startup_grace_period(self) -> None:
        """Shepherd within startup grace should trigger neither warning nor reclaim."""
        ctx = _make_ctx(shepherds={
            "shepherd-1": {
                "status": "working",
                "issue": 42,
                "task_id": "abc1234",
                "started": _ts(60),  # 60s — within startup grace
            },
        })
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False
        assert entry.startup_warning_at is None

    def test_past_hard_reclaim_no_progress_file(self, tmp_path: pathlib.Path) -> None:
        """Shepherd past hard reclaim period with no progress file should be flagged."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(310),  # 310s — past 300s hard reclaim
                },
            },
        )
        with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value=""):
            entry = ctx.state.shepherds["shepherd-1"]
            assert _check_no_progress_file(ctx, "shepherd-1", entry) is True

    def test_past_grace_period_with_progress_file(self, tmp_path: pathlib.Path) -> None:
        """Shepherd past grace period WITH a progress file should NOT be flagged."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
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
        """Shepherd with matching issue in snapshot progress should NOT be flagged."""
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


# -----------------------------------------------------------------------
# _check_no_progress_file — Tier 1 early warning
# -----------------------------------------------------------------------


class TestTier1EarlyWarning:
    """Tests for Tier 1 early warning (between startup and hard reclaim grace)."""

    def test_tier1_warning_fires_sets_startup_warning_at(self, tmp_path: pathlib.Path) -> None:
        """Shepherd at 130s with no progress triggers warning but NOT reclaim."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(130),  # Past 120s, before 300s
                },
            },
        )
        entry = ctx.state.shepherds["shepherd-1"]

        with patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True):
            with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="some output"):
                result = _check_no_progress_file(ctx, "shepherd-1", entry)

        assert result is False  # NOT reclaimed
        assert entry.startup_warning_at is not None

    def test_tier1_warning_not_duplicated(self, tmp_path: pathlib.Path) -> None:
        """Second call at Tier 1 should not overwrite startup_warning_at."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        first_warning_ts = "2026-01-01T10:00:00Z"
        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(200),
                    "startup_warning_at": first_warning_ts,
                },
            },
        )
        entry = ctx.state.shepherds["shepherd-1"]

        with patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True):
            with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value=""):
                result = _check_no_progress_file(ctx, "shepherd-1", entry)

        assert result is False
        # Timestamp should be preserved from first warning
        assert entry.startup_warning_at == first_warning_ts

    def test_healthy_shepherd_unaffected(self, tmp_path: pathlib.Path) -> None:
        """Shepherd with progress file is never flagged regardless of age."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
        (progress_dir / "shepherd-abc1234.json").write_text(json.dumps({
            "task_id": "abc1234", "issue": 42, "status": "working",
        }))

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(1000),  # Very old
                },
            },
        )
        entry = ctx.state.shepherds["shepherd-1"]
        assert _check_no_progress_file(ctx, "shepherd-1", entry) is False
        assert entry.startup_warning_at is None


# -----------------------------------------------------------------------
# _save_diagnostic_output
# -----------------------------------------------------------------------


class TestSaveDiagnosticOutput:
    """Tests for diagnostic output capture and save."""

    def test_saves_diagnostic_file(self, tmp_path: pathlib.Path) -> None:
        """Verify diagnostic output is written to .loom/logs/."""
        ctx = _make_ctx(repo_root=tmp_path)

        with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="Error: permission denied\nStack trace..."):
            _save_diagnostic_output(ctx, "shepherd-1")

        log_dir = tmp_path / ".loom" / "logs"
        diag_files = list(log_dir.glob("stall-diagnostic-shepherd-1-*.log"))
        assert len(diag_files) == 1
        content = diag_files[0].read_text()
        assert "Error: permission denied" in content

    def test_no_output_skips_file_creation(self, tmp_path: pathlib.Path) -> None:
        """No diagnostic file should be created if tmux output is empty."""
        ctx = _make_ctx(repo_root=tmp_path)

        with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value=""):
            _save_diagnostic_output(ctx, "shepherd-1")

        log_dir = tmp_path / ".loom" / "logs"
        if log_dir.exists():
            diag_files = list(log_dir.glob("stall-diagnostic-*.log"))
            assert len(diag_files) == 0


# -----------------------------------------------------------------------
# force_reclaim_stale_shepherds — integration with two-tier detection
# -----------------------------------------------------------------------


class TestForceReclaimNoProgress:
    """Tests for no-progress-file detection in force_reclaim_stale_shepherds."""

    @patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="some output")
    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_reclaims_shepherd_past_hard_reclaim(
        self, mock_session, mock_pid, mock_claude, mock_capture, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd past hard reclaim (300s) without progress file gets reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(310),  # Past 300s
                },
            },
        )

        with patch("loom_tools.daemon_v2.actions.shepherds.kill_stuck_session"):
            with patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue"):
                reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 1
        assert ctx.state.shepherds["shepherd-1"].status == "idle"
        assert ctx.state.shepherds["shepherd-1"].idle_reason == "stall_recovery"
        # startup_warning_at should be cleared on reset
        assert ctx.state.shepherds["shepherd-1"].startup_warning_at is None

    @patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="")
    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_captures_diagnostic_before_kill(
        self, mock_session, mock_pid, mock_claude, mock_capture, tmp_path: pathlib.Path
    ) -> None:
        """Verify capture_tmux_output is called before kill_stuck_session."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(310),
                },
            },
        )

        call_order: list[str] = []

        def track_capture(name, lines=500):
            call_order.append("capture")
            return "diagnostic output"

        def track_kill(name):
            call_order.append("kill")

        mock_capture.side_effect = track_capture

        with patch("loom_tools.daemon_v2.actions.shepherds.kill_stuck_session", side_effect=track_kill):
            with patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue"):
                force_reclaim_stale_shepherds(ctx)

        assert "capture" in call_order
        assert "kill" in call_order
        assert call_order.index("capture") < call_order.index("kill")

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_does_not_reclaim_within_startup_grace(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd within startup grace period should NOT be reclaimed."""
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

    @patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="output")
    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_tier1_warning_does_not_reclaim(
        self, mock_session, mock_pid, mock_claude, mock_capture, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd between startup and hard reclaim grace triggers warning, not reclaim."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)

        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(200),  # Between 120s and 300s
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)
        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-1"].status == "working"
        assert ctx.state.shepherds["shepherd-1"].startup_warning_at is not None

    @patch("loom_tools.agent_spawn._is_claude_running", return_value=True)
    @patch("loom_tools.agent_spawn._get_pane_pid", return_value=12345)
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=True)
    def test_does_not_reclaim_with_active_heartbeat(
        self, mock_session, mock_pid, mock_claude, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd with an active progress file should NOT be reclaimed."""
        progress_dir = tmp_path / ".loom" / "progress"
        progress_dir.mkdir(parents=True)
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


# -----------------------------------------------------------------------
# Proactive reclaim (called every iteration)
# -----------------------------------------------------------------------


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

        def reclaim_side_effect(c: DaemonContext) -> int:
            c.state.shepherds["shepherd-1"].status = "idle"
            c.state.shepherds["shepherd-2"].status = "idle"
            return 2

        mock_reclaim.side_effect = reclaim_side_effect
        _reclaim_stale_shepherds(ctx)

        assert ctx.snapshot["computed"]["active_shepherds"] == 0
        assert ctx.snapshot["computed"]["available_shepherd_slots"] == 10


# -----------------------------------------------------------------------
# Startup grace period for tmux-session-missing check (issue #2969)
# -----------------------------------------------------------------------


class TestTmuxSessionGracePeriod:
    """Tests for the startup grace period in Check 1 (tmux-session-missing).

    Before this fix, STALL-L2 would immediately kill a shepherd whose tmux
    session had not yet been created.  Since the session is created
    asynchronously, a shepherd spawned only a few seconds before the next
    daemon poll had no tmux session and was incorrectly declared stale —
    producing a false shepherd_failure and potentially two PRs for the same
    issue.

    The fix: do NOT mark a shepherd stale for missing tmux session if it was
    spawned within the last startup_grace_period seconds.
    """

    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_no_tmux_session_within_grace_period_not_stale(
        self, mock_session, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd with no tmux session spawned within grace period is NOT stale."""
        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-3": {
                    "status": "working",
                    "issue": 2928,
                    "task_id": "abc1234",
                    "started": _ts(23),  # 23s after spawn — well within 120s grace
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-3"].status == "working"

    @patch("loom_tools.daemon_v2.actions.shepherds.kill_stuck_session")
    @patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue")
    @patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value="")
    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_no_tmux_session_past_grace_period_is_stale(
        self, mock_session, mock_capture, mock_unclaim, mock_kill, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd with no tmux session past grace period IS stale and gets reclaimed."""
        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(130),  # 130s — past the 120s grace period
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 1
        assert ctx.state.shepherds["shepherd-1"].status == "idle"
        assert ctx.state.shepherds["shepherd-1"].idle_reason == "stall_recovery"

    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_no_tmux_session_at_exactly_grace_boundary_not_stale(
        self, mock_session, tmp_path: pathlib.Path
    ) -> None:
        """Shepherd at exactly startup_grace_period - 1 seconds is NOT stale."""
        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    "started": _ts(119),  # 119s — one second before grace expires
                },
            },
        )

        reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-1"].status == "working"

    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_no_tmux_session_no_started_timestamp_is_stale(
        self, mock_session, tmp_path: pathlib.Path
    ) -> None:
        """Without a started timestamp we cannot compute spawn_age, so treat as stale."""
        ctx = _make_ctx(
            repo_root=tmp_path,
            shepherds={
                "shepherd-1": {
                    "status": "working",
                    "issue": 42,
                    "task_id": "abc1234",
                    # no "started" field — spawn_age_seconds will be None
                },
            },
        )

        with patch("loom_tools.daemon_v2.actions.shepherds._unclaim_issue"):
            with patch("loom_tools.daemon_v2.actions.shepherds.capture_tmux_output", return_value=""):
                reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 1
        assert ctx.state.shepherds["shepherd-1"].status == "idle"

    @patch("loom_tools.daemon_v2.actions.shepherds.session_exists", return_value=False)
    def test_custom_grace_period_respected(
        self, mock_session, tmp_path: pathlib.Path
    ) -> None:
        """Custom startup_grace_period configuration is respected."""
        from loom_tools.daemon_v2.config import DaemonConfig
        from loom_tools.daemon_v2.context import DaemonContext
        from loom_tools.models.daemon_state import DaemonState

        config = DaemonConfig(startup_grace_period=60)  # shorter grace: 60s
        ctx = DaemonContext(config=config, repo_root=tmp_path)
        ctx.snapshot = {
            "computed": {
                "health_status": "healthy",
                "health_warnings": [],
                "total_ready": 0,
                "active_shepherds": 1,
                "available_shepherd_slots": 2,
            },
            "shepherds": {"progress": []},
        }
        state = DaemonState()
        state.shepherds["shepherd-1"] = ShepherdEntry(
            status="working",
            issue=42,
            task_id="abc1234",
            started=_ts(50),  # 50s — within the custom 60s grace
        )
        ctx.state = state

        reclaimed = force_reclaim_stale_shepherds(ctx)

        assert reclaimed == 0
        assert ctx.state.shepherds["shepherd-1"].status == "working"
