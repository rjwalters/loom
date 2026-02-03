"""Tests for daemon context."""

import pathlib
import tempfile

import pytest

from loom_tools.daemon_v2.config import DaemonConfig
from loom_tools.daemon_v2.context import DaemonContext


class TestDaemonContext:
    """Tests for DaemonContext."""

    def test_creation(self):
        """Test context creation."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")

        ctx = DaemonContext(config=config, repo_root=repo_root)

        assert ctx.config == config
        assert ctx.repo_root == repo_root
        assert ctx.iteration == 0
        assert ctx.running is True
        assert ctx.snapshot is None
        assert ctx.state is None

    def test_session_id_generated(self):
        """Test that session ID is auto-generated."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")

        ctx = DaemonContext(config=config, repo_root=repo_root)

        assert ctx.session_id is not None
        assert len(ctx.session_id) > 0
        assert "-" in ctx.session_id  # Format: {timestamp}-{pid}

    def test_file_paths(self):
        """Test file path properties."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")

        ctx = DaemonContext(config=config, repo_root=repo_root)

        assert ctx.log_file == repo_root / ".loom" / "daemon.log"
        assert ctx.state_file == repo_root / ".loom" / "daemon-state.json"
        assert ctx.metrics_file == repo_root / ".loom" / "daemon-metrics.json"
        assert ctx.stop_signal == repo_root / ".loom" / "stop-daemon"
        assert ctx.pid_file == repo_root / ".loom" / "daemon-loop.pid"

    def test_get_recommended_actions_no_snapshot(self):
        """Test getting recommended actions with no snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        actions = ctx.get_recommended_actions()
        assert actions == []

    def test_get_recommended_actions_with_snapshot(self):
        """Test getting recommended actions from snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        ctx.snapshot = {
            "computed": {
                "recommended_actions": ["spawn_shepherds", "trigger_guide"],
            }
        }

        actions = ctx.get_recommended_actions()
        assert actions == ["spawn_shepherds", "trigger_guide"]

    def test_get_available_shepherd_slots_no_snapshot(self):
        """Test getting available slots with no snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        slots = ctx.get_available_shepherd_slots()
        assert slots == 0

    def test_get_available_shepherd_slots_with_snapshot(self):
        """Test getting available slots from snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        ctx.snapshot = {
            "computed": {
                "available_shepherd_slots": 2,
            }
        }

        slots = ctx.get_available_shepherd_slots()
        assert slots == 2

    def test_get_ready_issues_no_snapshot(self):
        """Test getting ready issues with no snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        issues = ctx.get_ready_issues()
        assert issues == []

    def test_get_ready_issues_with_snapshot(self):
        """Test getting ready issues from snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        ctx.snapshot = {
            "pipeline": {
                "ready_issues": [
                    {"number": 1, "title": "Issue 1"},
                    {"number": 2, "title": "Issue 2"},
                ],
            }
        }

        issues = ctx.get_ready_issues()
        assert len(issues) == 2
        assert issues[0]["number"] == 1

    def test_get_promotable_proposals_no_snapshot(self):
        """Test getting promotable proposals with no snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        proposals = ctx.get_promotable_proposals()
        assert proposals == []

    def test_get_promotable_proposals_with_snapshot(self):
        """Test getting promotable proposals from snapshot."""
        config = DaemonConfig()
        repo_root = pathlib.Path("/tmp/test-repo")
        ctx = DaemonContext(config=config, repo_root=repo_root)

        ctx.snapshot = {
            "computed": {
                "promotable_proposals": [10, 11, 12],
            }
        }

        proposals = ctx.get_promotable_proposals()
        assert proposals == [10, 11, 12]
