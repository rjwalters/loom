"""Tests for loom_tools.common.github module."""

from __future__ import annotations

import json
import subprocess
from unittest import mock

from loom_tools.common.github import gh_get_default_branch_ci_status


class TestGhGetDefaultBranchCiStatus:
    """Tests for gh_get_default_branch_ci_status function."""

    def test_all_passing(self) -> None:
        """When all workflows pass, returns passing status."""
        mock_runs = [
            {"name": "CI", "conclusion": "success", "status": "completed", "headBranch": "main"},
            {"name": "Lint", "conclusion": "success", "status": "completed", "headBranch": "main"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_runs),
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "passing"
        assert result["failed_runs"] == []
        assert result["total_runs"] == 2

    def test_one_failing(self) -> None:
        """When one workflow fails, returns failing status with the failed run."""
        mock_runs = [
            {"name": "CI", "conclusion": "failure", "status": "completed", "headBranch": "main"},
            {"name": "Lint", "conclusion": "success", "status": "completed", "headBranch": "main"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_runs),
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "failing"
        assert result["failed_runs"] == ["CI"]
        assert result["total_runs"] == 2
        assert "1 workflow(s) failed" in result["message"]

    def test_multiple_failing(self) -> None:
        """When multiple workflows fail, returns all failed names."""
        mock_runs = [
            {"name": "CI", "conclusion": "failure", "status": "completed", "headBranch": "main"},
            {"name": "Lint", "conclusion": "failure", "status": "completed", "headBranch": "main"},
            {"name": "Test", "conclusion": "success", "status": "completed", "headBranch": "main"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_runs),
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "failing"
        assert "CI" in result["failed_runs"]
        assert "Lint" in result["failed_runs"]
        assert result["total_runs"] == 3

    def test_in_progress_not_counted_as_failure(self) -> None:
        """In-progress workflows are not counted as failures."""
        mock_runs = [
            {"name": "CI", "conclusion": None, "status": "in_progress", "headBranch": "main"},
            {"name": "Lint", "conclusion": "success", "status": "completed", "headBranch": "main"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_runs),
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "passing"
        assert result["failed_runs"] == []

    def test_empty_runs(self) -> None:
        """When no workflow runs found, returns unknown status."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout="[]",
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "unknown"
        assert result["total_runs"] == 0

    def test_gh_command_fails(self) -> None:
        """When gh command fails, returns unknown status."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=1,
                stdout="",
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "unknown"

    def test_json_decode_error(self) -> None:
        """When JSON is invalid, returns unknown status."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout="not valid json",
            )
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "unknown"

    def test_subprocess_error(self) -> None:
        """When subprocess raises an error, returns unknown status."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.side_effect = subprocess.SubprocessError("Connection failed")
            result = gh_get_default_branch_ci_status()

        assert result["status"] == "unknown"

    def test_only_latest_run_per_workflow(self) -> None:
        """When multiple runs of same workflow, only counts the latest (first in list)."""
        mock_runs = [
            {"name": "CI", "conclusion": "success", "status": "completed", "headBranch": "main"},
            {"name": "CI", "conclusion": "failure", "status": "completed", "headBranch": "main"},
            {"name": "CI", "conclusion": "failure", "status": "completed", "headBranch": "main"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_runs),
            )
            result = gh_get_default_branch_ci_status()

        # Should only see one CI run (the first/latest), which passed
        assert result["status"] == "passing"
        assert result["total_runs"] == 1
