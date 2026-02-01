"""Tests for loom_tools.common.worktree_safety.

Tests the safety checks that prevent worktree removal when:
- An in-use marker file exists
- Active processes have the worktree as their CWD
- The worktree is within its creation grace period

Related issue: #1833 - Worktree removal destroys active shell sessions
"""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import time
from unittest.mock import MagicMock, patch

import pytest

from loom_tools.common.worktree_safety import (
    DEFAULT_GRACE_PERIOD_SECONDS,
    WorktreeSafetyResult,
    check_cwd_inside_worktree,
    check_grace_period,
    check_in_use_marker,
    find_processes_using_directory,
    is_worktree_safe_to_remove,
    should_reuse_worktree,
)


class TestCheckInUseMarker:
    """Tests for check_in_use_marker function."""

    def test_no_marker(self, tmp_path: pathlib.Path) -> None:
        """Worktree without marker file returns (False, None)."""
        exists, data = check_in_use_marker(tmp_path)
        assert exists is False
        assert data is None

    def test_marker_exists(self, tmp_path: pathlib.Path) -> None:
        """Worktree with marker file returns (True, parsed_data)."""
        marker_data = {
            "shepherd_task_id": "abc123",
            "issue": 42,
            "created_at": "2026-01-30T10:00:00Z",
            "pid": 12345,
        }
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text(json.dumps(marker_data))

        exists, data = check_in_use_marker(tmp_path)
        assert exists is True
        assert data is not None
        assert data["shepherd_task_id"] == "abc123"
        assert data["issue"] == 42

    def test_marker_empty_file(self, tmp_path: pathlib.Path) -> None:
        """Empty marker file returns (True, {}) - treated as valid but empty marker."""
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text("")

        exists, data = check_in_use_marker(tmp_path)
        assert exists is True
        # Empty file is treated as valid empty dict by read_json_file
        assert data == {}

    def test_marker_invalid_json(self, tmp_path: pathlib.Path) -> None:
        """Invalid JSON in marker file returns (True, {}) - marker present but unparseable."""
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text("not valid json {")

        exists, data = check_in_use_marker(tmp_path)
        assert exists is True
        # Invalid JSON is treated as empty dict by read_json_file
        assert data == {}

    def test_custom_marker_name(self, tmp_path: pathlib.Path) -> None:
        """Custom marker name is respected."""
        marker_file = tmp_path / ".custom-marker"
        marker_file.write_text('{"custom": true}')

        exists, data = check_in_use_marker(tmp_path, marker_name=".custom-marker")
        assert exists is True
        assert data is not None
        assert data["custom"] is True


class TestCheckGracePeriod:
    """Tests for check_grace_period function."""

    def test_nonexistent_directory(self, tmp_path: pathlib.Path) -> None:
        """Nonexistent directory returns (False, None)."""
        nonexistent = tmp_path / "does-not-exist"
        within, age = check_grace_period(nonexistent)
        assert within is False
        assert age is None

    def test_new_worktree_within_grace(self, tmp_path: pathlib.Path) -> None:
        """Newly created worktree is within grace period."""
        # Create a .git file to simulate worktree
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        within, age = check_grace_period(tmp_path, grace_seconds=300)
        assert within is True
        assert age is not None
        assert age < 300  # Should be very fresh

    def test_old_worktree_past_grace(self, tmp_path: pathlib.Path) -> None:
        """Old worktree is past grace period."""
        # Create a .git file
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        # Use a very short grace period
        within, age = check_grace_period(tmp_path, grace_seconds=0)
        assert within is False
        assert age is not None
        assert age >= 0

    def test_custom_grace_period(self, tmp_path: pathlib.Path) -> None:
        """Custom grace period is respected."""
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        # With 1 hour grace period, newly created should be within grace
        within, age = check_grace_period(tmp_path, grace_seconds=3600)
        assert within is True


class TestFindProcessesUsingDirectory:
    """Tests for find_processes_using_directory function."""

    def test_empty_directory_no_processes(self, tmp_path: pathlib.Path) -> None:
        """Empty directory has no processes using it."""
        pids = find_processes_using_directory(tmp_path)
        # Should be empty (current process is filtered out)
        assert isinstance(pids, list)

    def test_excludes_current_process(self, tmp_path: pathlib.Path) -> None:
        """Current process PID is excluded from results."""
        # Change to tmp_path so we're "using" it
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_path)
            pids = find_processes_using_directory(tmp_path)
            # Current PID should be excluded
            assert os.getpid() not in pids
        finally:
            os.chdir(original_cwd)

    @pytest.mark.skipif(not os.path.exists("/proc"), reason="Linux-only test")
    def test_linux_proc_detection(self, tmp_path: pathlib.Path) -> None:
        """On Linux, /proc is used to detect processes."""
        # This test just verifies the function runs on Linux
        pids = find_processes_using_directory(tmp_path)
        assert isinstance(pids, list)


class TestIsWorktreeSafeToRemove:
    """Tests for is_worktree_safe_to_remove function."""

    def test_nonexistent_worktree(self, tmp_path: pathlib.Path) -> None:
        """Nonexistent worktree is safe to remove."""
        nonexistent = tmp_path / "does-not-exist"
        result = is_worktree_safe_to_remove(nonexistent)

        assert result.safe_to_remove is True
        assert "does not exist" in (result.reason or "")

    def test_blocked_by_marker(self, tmp_path: pathlib.Path) -> None:
        """Worktree with in-use marker is NOT safe to remove."""
        marker_data = {
            "shepherd_task_id": "abc123",
            "issue": 42,
        }
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text(json.dumps(marker_data))

        result = is_worktree_safe_to_remove(tmp_path)

        assert result.safe_to_remove is False
        assert result.marker_present is True
        assert result.marker_data is not None
        assert "shepherd" in (result.reason or "").lower()
        assert "abc123" in (result.reason or "")

    def test_blocked_by_grace_period(self, tmp_path: pathlib.Path) -> None:
        """Worktree within grace period is NOT safe to remove."""
        # Create .git file to have a timestamp
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        result = is_worktree_safe_to_remove(
            tmp_path,
            check_marker=False,  # Skip marker check
            check_processes=False,  # Skip process check
            check_grace=True,
            grace_seconds=3600,  # 1 hour grace
        )

        assert result.safe_to_remove is False
        assert result.within_grace_period is True
        assert "grace period" in (result.reason or "").lower()

    def test_safe_when_all_checks_pass(self, tmp_path: pathlib.Path) -> None:
        """Worktree is safe when all checks pass."""
        # No marker, use short grace period
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        result = is_worktree_safe_to_remove(
            tmp_path,
            check_marker=True,
            check_processes=True,
            check_grace=True,
            grace_seconds=0,  # No grace period
        )

        assert result.safe_to_remove is True
        assert result.marker_present is False

    def test_individual_checks_can_be_disabled(self, tmp_path: pathlib.Path) -> None:
        """Individual safety checks can be disabled."""
        # Create marker
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text('{"task": "test"}')

        # With marker check enabled - blocked
        result = is_worktree_safe_to_remove(tmp_path, check_marker=True)
        assert result.safe_to_remove is False

        # With marker check disabled - safe (ignoring other checks)
        result = is_worktree_safe_to_remove(
            tmp_path,
            check_marker=False,
            check_processes=False,
            check_grace=False,
        )
        assert result.safe_to_remove is True

    def test_result_includes_all_data(self, tmp_path: pathlib.Path) -> None:
        """WorktreeSafetyResult includes all relevant data."""
        marker_data = {"shepherd_task_id": "xyz789"}
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text(json.dumps(marker_data))

        result = is_worktree_safe_to_remove(tmp_path)

        assert isinstance(result, WorktreeSafetyResult)
        assert result.marker_present is True
        assert result.marker_data == marker_data


class TestShouldReuseWorktree:
    """Tests for should_reuse_worktree function."""

    def test_reuse_when_marker_present(self, tmp_path: pathlib.Path) -> None:
        """Worktree with marker should be reused."""
        marker_file = tmp_path / ".loom-in-use"
        marker_file.write_text('{"task": "test"}')

        assert should_reuse_worktree(tmp_path) is True

    def test_no_reuse_when_safe_to_remove(self, tmp_path: pathlib.Path) -> None:
        """Worktree safe to remove should not be reused."""
        # Create old worktree (past grace period)
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        # Use 0 grace period to simulate old worktree
        assert should_reuse_worktree(tmp_path, grace_seconds=0) is False

    def test_reuse_when_within_grace(self, tmp_path: pathlib.Path) -> None:
        """Worktree within grace period should be reused."""
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: /some/path")

        # Use long grace period
        assert should_reuse_worktree(tmp_path, grace_seconds=3600) is True


class TestIntegration:
    """Integration tests for worktree safety in realistic scenarios."""

    def test_scenario_fresh_worktree_with_active_session(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Scenario: Freshly created worktree with active builder session.

        This is the core scenario from issue #1833:
        - A shepherd creates a worktree
        - The builder's shell CWD is set to the worktree
        - The builder fails before making commits
        - Stale detection should NOT remove the worktree because:
          a) There's an active process using it
          b) It's within the grace period
        """
        # Create worktree structure
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: ../../.git/worktrees/issue-42")

        # Add in-use marker (simulating shepherd creating it)
        marker = tmp_path / ".loom-in-use"
        marker.write_text(json.dumps({
            "shepherd_task_id": "test-task",
            "issue": 42,
            "created_at": "2026-01-30T10:00:00Z",
        }))

        # Safety check should block removal
        result = is_worktree_safe_to_remove(tmp_path)

        assert result.safe_to_remove is False
        assert result.marker_present is True
        assert "shepherd" in (result.reason or "").lower()

    def test_scenario_abandoned_worktree_safe_to_clean(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Scenario: Abandoned worktree with no active session.

        - Worktree was created long ago (past grace period)
        - No marker file (shepherd completed or crashed)
        - No active processes using it
        - Safe to clean up
        """
        # Create old worktree (just the directory, no marker)
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: ../../.git/worktrees/issue-99")

        result = is_worktree_safe_to_remove(
            tmp_path,
            grace_seconds=0,  # Simulate old worktree
        )

        assert result.safe_to_remove is True
        assert result.marker_present is False

    def test_scenario_worktree_reuse_vs_recreate(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Scenario: Builder pre-flight deciding to reuse vs recreate worktree.

        When a worktree exists with 0 commits ahead of main:
        - If within grace period: REUSE (another session may be active)
        - If marker exists: REUSE (shepherd is actively using it)
        - If old and no marker: CAN REMOVE and recreate
        """
        git_file = tmp_path / ".git"
        git_file.write_text("gitdir: ../../.git/worktrees/issue-123")

        # Scenario A: Fresh worktree - reuse
        assert should_reuse_worktree(tmp_path, grace_seconds=3600) is True

        # Scenario B: Old worktree no marker - can remove
        assert should_reuse_worktree(tmp_path, grace_seconds=0) is False

        # Scenario C: Old worktree WITH marker - reuse
        marker = tmp_path / ".loom-in-use"
        marker.write_text('{"task": "active"}')
        assert should_reuse_worktree(tmp_path, grace_seconds=0) is True
