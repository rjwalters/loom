"""Tests for loom_tools.common.paths module."""

from __future__ import annotations

from pathlib import Path

import pytest

from loom_tools.common.paths import LoomPaths, NamingConventions


class TestLoomPaths:
    """Tests for LoomPaths class."""

    def test_loom_dir(self, tmp_path: Path) -> None:
        """Test loom_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.loom_dir == tmp_path / ".loom"

    def test_scripts_dir(self, tmp_path: Path) -> None:
        """Test scripts_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.scripts_dir == tmp_path / ".loom" / "scripts"

    def test_progress_dir(self, tmp_path: Path) -> None:
        """Test progress_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.progress_dir == tmp_path / ".loom" / "progress"

    def test_worktrees_dir(self, tmp_path: Path) -> None:
        """Test worktrees_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.worktrees_dir == tmp_path / ".loom" / "worktrees"

    def test_logs_dir(self, tmp_path: Path) -> None:
        """Test logs_dir property."""
        paths = LoomPaths(tmp_path)
        assert paths.logs_dir == tmp_path / ".loom" / "logs"

    def test_daemon_state_file(self, tmp_path: Path) -> None:
        """Test daemon_state_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.daemon_state_file == tmp_path / ".loom" / "daemon-state.json"

    def test_health_metrics_file(self, tmp_path: Path) -> None:
        """Test health_metrics_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.health_metrics_file == tmp_path / ".loom" / "health-metrics.json"

    def test_alerts_file(self, tmp_path: Path) -> None:
        """Test alerts_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.alerts_file == tmp_path / ".loom" / "alerts.json"

    def test_stuck_history_file(self, tmp_path: Path) -> None:
        """Test stuck_history_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stuck_history_file == tmp_path / ".loom" / "stuck-history.json"

    def test_config_file(self, tmp_path: Path) -> None:
        """Test config_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.config_file == tmp_path / ".loom" / "config.json"

    def test_stop_daemon_file(self, tmp_path: Path) -> None:
        """Test stop_daemon_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stop_daemon_file == tmp_path / ".loom" / "stop-daemon"

    def test_stop_shepherds_file(self, tmp_path: Path) -> None:
        """Test stop_shepherds_file property."""
        paths = LoomPaths(tmp_path)
        assert paths.stop_shepherds_file == tmp_path / ".loom" / "stop-shepherds"

    def test_worktree_path(self, tmp_path: Path) -> None:
        """Test worktree_path method."""
        paths = LoomPaths(tmp_path)
        assert paths.worktree_path(42) == tmp_path / ".loom" / "worktrees" / "issue-42"
        assert paths.worktree_path(123) == tmp_path / ".loom" / "worktrees" / "issue-123"

    def test_progress_file(self, tmp_path: Path) -> None:
        """Test progress_file method."""
        paths = LoomPaths(tmp_path)
        assert paths.progress_file("abc1234") == tmp_path / ".loom" / "progress" / "shepherd-abc1234.json"

    def test_builder_log_file(self, tmp_path: Path) -> None:
        """Test builder_log_file method."""
        paths = LoomPaths(tmp_path)
        assert paths.builder_log_file(42) == tmp_path / ".loom" / "logs" / "loom-builder-issue-42.log"


class TestNamingConventions:
    """Tests for NamingConventions class."""

    def test_branch_name(self) -> None:
        """Test branch_name static method."""
        assert NamingConventions.branch_name(42) == "feature/issue-42"
        assert NamingConventions.branch_name(123) == "feature/issue-123"
        assert NamingConventions.branch_name(1) == "feature/issue-1"

    def test_worktree_name(self) -> None:
        """Test worktree_name static method."""
        assert NamingConventions.worktree_name(42) == "issue-42"
        assert NamingConventions.worktree_name(123) == "issue-123"
        assert NamingConventions.worktree_name(1) == "issue-1"

    def test_issue_from_branch_valid(self) -> None:
        """Test issue_from_branch with valid branch names."""
        assert NamingConventions.issue_from_branch("feature/issue-42") == 42
        assert NamingConventions.issue_from_branch("feature/issue-123") == 123
        assert NamingConventions.issue_from_branch("feature/issue-1") == 1

    def test_issue_from_branch_invalid(self) -> None:
        """Test issue_from_branch with invalid branch names."""
        assert NamingConventions.issue_from_branch("main") is None
        assert NamingConventions.issue_from_branch("feature/other") is None
        assert NamingConventions.issue_from_branch("feature/issue-") is None
        assert NamingConventions.issue_from_branch("feature/issue-abc") is None
        assert NamingConventions.issue_from_branch("") is None

    def test_issue_from_worktree_valid(self) -> None:
        """Test issue_from_worktree with valid worktree names."""
        assert NamingConventions.issue_from_worktree("issue-42") == 42
        assert NamingConventions.issue_from_worktree("issue-123") == 123
        assert NamingConventions.issue_from_worktree("issue-1") == 1

    def test_issue_from_worktree_invalid(self) -> None:
        """Test issue_from_worktree with invalid worktree names."""
        assert NamingConventions.issue_from_worktree("main") is None
        assert NamingConventions.issue_from_worktree("issue-") is None
        assert NamingConventions.issue_from_worktree("issue-abc") is None
        assert NamingConventions.issue_from_worktree("") is None
        assert NamingConventions.issue_from_worktree("terminal-1") is None

    def test_roundtrip_branch(self) -> None:
        """Test that branch_name and issue_from_branch are inverses."""
        for issue in [1, 42, 123, 9999]:
            branch = NamingConventions.branch_name(issue)
            assert NamingConventions.issue_from_branch(branch) == issue

    def test_roundtrip_worktree(self) -> None:
        """Test that worktree_name and issue_from_worktree are inverses."""
        for issue in [1, 42, 123, 9999]:
            worktree = NamingConventions.worktree_name(issue)
            assert NamingConventions.issue_from_worktree(worktree) == issue

    def test_constants(self) -> None:
        """Test that class constants are correctly defined."""
        assert NamingConventions.BRANCH_PREFIX == "feature/issue-"
        assert NamingConventions.WORKTREE_PREFIX == "issue-"
