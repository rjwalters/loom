"""Tests for loom_tools.common.github module."""

from __future__ import annotations

import json
import subprocess
from unittest import mock

from loom_tools.common.github import (
    gh_get_default_branch_ci_status,
    gh_issue_list,
    gh_list,
    gh_pr_list,
)


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


class TestGhList:
    """Tests for gh_list function."""

    def test_issue_list_basic(self) -> None:
        """Basic issue list returns parsed JSON."""
        mock_issues = [
            {"number": 1, "title": "Issue 1", "labels": [], "state": "OPEN"},
            {"number": 2, "title": "Issue 2", "labels": [], "state": "OPEN"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_issues),
            )
            result = gh_list("issue")

        assert len(result) == 2
        assert result[0]["number"] == 1
        mock_gh.assert_called_once()
        call_args = mock_gh.call_args[0][0]
        assert call_args[0] == "issue"
        assert "list" in call_args

    def test_pr_list_basic(self) -> None:
        """Basic PR list returns parsed JSON."""
        mock_prs = [
            {"number": 10, "title": "PR 1", "labels": [], "state": "OPEN"},
        ]
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps(mock_prs),
            )
            result = gh_list("pr")

        assert len(result) == 1
        assert result[0]["number"] == 10
        call_args = mock_gh.call_args[0][0]
        assert call_args[0] == "pr"

    def test_with_labels_filter(self) -> None:
        """Labels are passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("issue", labels=["bug", "urgent"])

        call_args = mock_gh.call_args[0][0]
        assert "--label" in call_args
        label_idx = call_args.index("--label")
        assert call_args[label_idx + 1] == "bug,urgent"

    def test_with_state_filter(self) -> None:
        """State is passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("pr", state="closed")

        call_args = mock_gh.call_args[0][0]
        assert "--state" in call_args
        state_idx = call_args.index("--state")
        assert call_args[state_idx + 1] == "closed"

    def test_with_custom_fields(self) -> None:
        """Custom fields are passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("issue", fields=["number", "url", "createdAt"])

        call_args = mock_gh.call_args[0][0]
        assert "--json" in call_args
        json_idx = call_args.index("--json")
        assert call_args[json_idx + 1] == "number,url,createdAt"

    def test_with_search_parameter(self) -> None:
        """Search query is passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("pr", search="Closes #123")

        call_args = mock_gh.call_args[0][0]
        assert "--search" in call_args
        search_idx = call_args.index("--search")
        assert call_args[search_idx + 1] == "Closes #123"

    def test_with_head_parameter(self) -> None:
        """Head branch filter is passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("pr", head="feature/issue-42")

        call_args = mock_gh.call_args[0][0]
        assert "--head" in call_args
        head_idx = call_args.index("--head")
        assert call_args[head_idx + 1] == "feature/issue-42"

    def test_with_limit_parameter(self) -> None:
        """Limit is passed to gh command."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("issue", limit=5)

        call_args = mock_gh.call_args[0][0]
        assert "--limit" in call_args
        limit_idx = call_args.index("--limit")
        assert call_args[limit_idx + 1] == "5"

    def test_empty_result(self) -> None:
        """Empty stdout returns empty list."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="")
            result = gh_list("issue")

        assert result == []

    def test_whitespace_only_result(self) -> None:
        """Whitespace-only stdout returns empty list."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="   \n  ")
            result = gh_list("pr")

        assert result == []

    def test_nonzero_return_code(self) -> None:
        """Non-zero return code returns empty list."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=1, stdout="[]")
            result = gh_list("issue")

        assert result == []

    def test_invalid_json(self) -> None:
        """Invalid JSON returns empty list."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="not valid json")
            result = gh_list("pr")

        assert result == []

    def test_default_fields_used(self) -> None:
        """Default fields are used when not specified."""
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="[]")
            gh_list("issue")

        call_args = mock_gh.call_args[0][0]
        json_idx = call_args.index("--json")
        fields = call_args[json_idx + 1]
        assert "number" in fields
        assert "title" in fields
        assert "labels" in fields
        assert "state" in fields


class TestGhIssueListWrapper:
    """Tests for gh_issue_list thin wrapper."""

    def test_calls_gh_list_with_issue_type(self) -> None:
        """gh_issue_list calls gh_list with entity_type='issue'."""
        with mock.patch("loom_tools.common.github.gh_list") as mock_list:
            mock_list.return_value = []
            gh_issue_list(labels=["bug"], state="closed", fields=["number"])

        mock_list.assert_called_once_with(
            "issue", labels=["bug"], state="closed", fields=["number"]
        )


class TestGhPrListWrapper:
    """Tests for gh_pr_list thin wrapper."""

    def test_calls_gh_list_with_pr_type(self) -> None:
        """gh_pr_list calls gh_list with entity_type='pr'."""
        with mock.patch("loom_tools.common.github.gh_list") as mock_list:
            mock_list.return_value = []
            gh_pr_list(labels=["ready"], state="open", fields=["url"])

        mock_list.assert_called_once_with(
            "pr", labels=["ready"], state="open", fields=["url"]
        )
