"""Tests for loom_tools.common.github module."""

from __future__ import annotations

import json
import os
import subprocess
from unittest import mock

import pytest

from loom_tools.common.forge import (
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
)
from loom_tools.common.github import (
    ApiMode,
    GitHubForge,
    _gh_cmd,
    _is_rate_limited,
    _normalize_rest_entity,
    _normalize_rest_labels,
    _parse_nwo,
    _reset_forge,
    _reset_nwo_cache,
    get_api_mode,
    get_forge,
    get_repo_nwo,
    gh_entity_edit,
    gh_get_default_branch_ci_status,
    gh_issue_comment,
    gh_issue_list,
    gh_issue_view,
    gh_list,
    gh_pr_list,
    gh_pr_view,
)


# ---------------------------------------------------------------------------
# _parse_nwo
# ---------------------------------------------------------------------------


class TestParseNwo:
    """Tests for _parse_nwo URL parsing."""

    def test_ssh_url(self) -> None:
        assert _parse_nwo("git@github.com:owner/repo.git") == "owner/repo"

    def test_ssh_url_without_git_suffix(self) -> None:
        assert _parse_nwo("git@github.com:owner/repo") == "owner/repo"

    def test_https_url(self) -> None:
        assert _parse_nwo("https://github.com/owner/repo.git") == "owner/repo"

    def test_https_url_without_git_suffix(self) -> None:
        assert _parse_nwo("https://github.com/owner/repo") == "owner/repo"

    def test_http_url(self) -> None:
        assert _parse_nwo("http://github.com/owner/repo.git") == "owner/repo"

    def test_custom_ssh_host(self) -> None:
        assert _parse_nwo("git@gh.enterprise.com:org/project.git") == "org/project"

    def test_invalid_url(self) -> None:
        assert _parse_nwo("not-a-url") is None

    def test_empty_string(self) -> None:
        assert _parse_nwo("") is None

    def test_ssh_with_nested_path(self) -> None:
        # e.g., GitLab subgroups
        assert _parse_nwo("git@gitlab.com:group/subgroup/repo.git") == "group/subgroup/repo"


# ---------------------------------------------------------------------------
# get_repo_nwo
# ---------------------------------------------------------------------------


class TestGetRepoNwo:
    """Tests for get_repo_nwo with subprocess mocking."""

    def setup_method(self) -> None:
        _reset_nwo_cache()

    def teardown_method(self) -> None:
        _reset_nwo_cache()

    def test_returns_nwo_from_git_remote(self) -> None:
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0,
                stdout="git@github.com:owner/repo.git\n",
            )
            result = get_repo_nwo()
        assert result == "owner/repo"

    def test_caches_result(self) -> None:
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0,
                stdout="git@github.com:owner/repo.git\n",
            )
            first = get_repo_nwo()
            second = get_repo_nwo()
        assert first == second == "owner/repo"
        mock_run.assert_called_once()

    def test_returns_none_on_failure(self) -> None:
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(returncode=1, stdout="")
            result = get_repo_nwo()
        assert result is None

    def test_returns_none_on_os_error(self) -> None:
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.side_effect = OSError("git not found")
            result = get_repo_nwo()
        assert result is None


# ---------------------------------------------------------------------------
# get_api_mode
# ---------------------------------------------------------------------------


class TestGetApiMode:
    """Tests for LOOM_GH_API_MODE environment variable handling."""

    def test_default_is_auto(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("LOOM_GH_API_MODE", None)
            assert get_api_mode() == ApiMode.AUTO

    def test_graphql_mode(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "graphql"}):
            assert get_api_mode() == ApiMode.GRAPHQL

    def test_rest_mode(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "rest"}):
            assert get_api_mode() == ApiMode.REST

    def test_auto_mode_explicit(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "auto"}):
            assert get_api_mode() == ApiMode.AUTO

    def test_case_insensitive(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "REST"}):
            assert get_api_mode() == ApiMode.REST

    def test_invalid_value_defaults_to_auto(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "invalid"}):
            assert get_api_mode() == ApiMode.AUTO

    def test_whitespace_trimmed(self) -> None:
        with mock.patch.dict(os.environ, {"LOOM_GH_API_MODE": "  rest  "}):
            assert get_api_mode() == ApiMode.REST


# ---------------------------------------------------------------------------
# _is_rate_limited
# ---------------------------------------------------------------------------


class TestIsRateLimited:
    """Tests for rate limit detection."""

    def test_successful_command_not_rate_limited(self) -> None:
        result = mock.Mock(returncode=0, stderr="", stdout="")
        assert _is_rate_limited(result) is False

    def test_detects_api_rate_limit(self) -> None:
        result = mock.Mock(
            returncode=1,
            stderr="API rate limit exceeded for user",
            stdout="",
        )
        assert _is_rate_limited(result) is True

    def test_detects_secondary_rate_limit(self) -> None:
        result = mock.Mock(
            returncode=1,
            stderr="You have exceeded a secondary rate limit",
            stdout="",
        )
        assert _is_rate_limited(result) is True

    def test_detects_http_403(self) -> None:
        result = mock.Mock(
            returncode=1,
            stderr="HTTP 403: rate limit exceeded",
            stdout="",
        )
        assert _is_rate_limited(result) is True

    def test_non_rate_limit_error(self) -> None:
        result = mock.Mock(
            returncode=1,
            stderr="Not Found (HTTP 404)",
            stdout="",
        )
        assert _is_rate_limited(result) is False

    def test_rate_limit_in_stdout(self) -> None:
        result = mock.Mock(
            returncode=1,
            stderr="",
            stdout="rate limit exceeded",
        )
        assert _is_rate_limited(result) is True

    def test_none_stderr(self) -> None:
        result = mock.Mock(returncode=1, stderr=None, stdout=None)
        assert _is_rate_limited(result) is False


# ---------------------------------------------------------------------------
# _normalize_rest_labels / _normalize_rest_entity
# ---------------------------------------------------------------------------


class TestNormalization:
    """Tests for REST response normalization."""

    def test_normalize_labels(self) -> None:
        rest_labels = [
            {"id": 1, "name": "bug", "color": "d73a4a", "description": "Something is broken"},
            {"id": 2, "name": "enhancement", "color": "a2eeef", "description": ""},
        ]
        result = _normalize_rest_labels(rest_labels)
        assert result == [{"name": "bug"}, {"name": "enhancement"}]

    def test_normalize_empty_labels(self) -> None:
        assert _normalize_rest_labels([]) == []

    def test_normalize_entity_state_uppercased(self) -> None:
        data = {"state": "open", "title": "Test"}
        result = _normalize_rest_entity(data)
        assert result["state"] == "OPEN"

    def test_normalize_entity_html_url_mapped(self) -> None:
        data = {"html_url": "https://github.com/owner/repo/issues/1", "title": "Test"}
        result = _normalize_rest_entity(data)
        assert result["url"] == "https://github.com/owner/repo/issues/1"

    def test_normalize_entity_labels_simplified(self) -> None:
        data = {
            "labels": [
                {"id": 1, "name": "loom:issue", "color": "abc"},
            ],
            "title": "Test",
        }
        result = _normalize_rest_entity(data)
        assert result["labels"] == [{"name": "loom:issue"}]

    def test_normalize_entity_field_filtering(self) -> None:
        data = {"state": "open", "title": "Test", "body": "Long body", "number": 42}
        result = _normalize_rest_entity(data, fields=["state", "title"])
        assert "state" in result
        assert "title" in result
        assert "body" not in result
        assert "number" not in result

    def test_normalize_entity_no_field_filtering(self) -> None:
        data = {"state": "open", "title": "Test", "number": 42}
        result = _normalize_rest_entity(data)
        assert "state" in result
        assert "title" in result
        assert "number" in result


# ---------------------------------------------------------------------------
# gh_issue_view
# ---------------------------------------------------------------------------


class TestGhIssueView:
    """Tests for gh_issue_view with dual-mode support."""

    def test_graphql_success(self) -> None:
        """GraphQL mode returns parsed issue."""
        issue_data = {"state": "OPEN", "title": "Test", "url": "https://github.com/o/r/issues/1"}
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(issue_data), stderr=""
                )
                result = gh_issue_view(1, fields=["state", "title", "url"])
        assert result == issue_data

    def test_graphql_not_found_returns_none(self) -> None:
        """GraphQL mode returns None for missing issue."""
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stdout="", stderr="not found")
                result = gh_issue_view(999)
        assert result is None

    def test_auto_mode_falls_back_on_rate_limit(self) -> None:
        """Auto mode falls back to REST when rate limited."""
        rest_data = {
            "state": "open",
            "title": "Test",
            "html_url": "https://github.com/o/r/issues/1",
            "labels": [],
            "number": 1,
        }
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.AUTO):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                # First call (GraphQL) fails with rate limit
                graphql_fail = mock.Mock(
                    returncode=1, stdout="", stderr="API rate limit exceeded"
                )
                # Second call (REST) succeeds
                rest_success = mock.Mock(
                    returncode=0, stdout=json.dumps(rest_data), stderr=""
                )
                mock_gh.side_effect = [graphql_fail, rest_success]

                with mock.patch("loom_tools.common.github.get_repo_nwo", return_value="o/r"):
                    result = gh_issue_view(1)

        assert result is not None
        assert result["state"] == "OPEN"  # Normalized to uppercase

    def test_rest_mode_skips_graphql(self) -> None:
        """REST mode goes directly to REST API."""
        rest_data = {
            "state": "open",
            "title": "Test",
            "html_url": "https://github.com/o/r/issues/1",
            "labels": [],
            "number": 1,
        }
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.REST):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(rest_data), stderr=""
                )
                with mock.patch("loom_tools.common.github.get_repo_nwo", return_value="o/r"):
                    result = gh_issue_view(1)

        assert result is not None
        # Should have called gh api, not gh issue view
        call_args = mock_gh.call_args[0][0]
        assert "api" in call_args

    def test_filters_out_pull_requests(self) -> None:
        """REST issue view filters out PRs (which share the issues endpoint)."""
        pr_data = {
            "state": "open",
            "title": "Test PR",
            "html_url": "https://github.com/o/r/pull/1",
            "labels": [],
            "number": 1,
            "pull_request": {"url": "https://api.github.com/repos/o/r/pulls/1"},
        }
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.REST):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(pr_data), stderr=""
                )
                with mock.patch("loom_tools.common.github.get_repo_nwo", return_value="o/r"):
                    result = gh_issue_view(1)
        assert result is None

    def test_rest_fallback_no_nwo_returns_none(self) -> None:
        """REST fallback returns None when NWO cannot be determined."""
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.REST):
            with mock.patch("loom_tools.common.github.get_repo_nwo", return_value=None):
                result = gh_issue_view(1)
        assert result is None


# ---------------------------------------------------------------------------
# gh_pr_view
# ---------------------------------------------------------------------------


class TestGhPrView:
    """Tests for gh_pr_view with dual-mode support."""

    def test_graphql_success(self) -> None:
        pr_data = {"state": "OPEN", "title": "Test PR", "number": 10}
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(pr_data), stderr=""
                )
                result = gh_pr_view(10, fields=["state", "title", "number"])
        assert result == pr_data

    def test_returns_none_on_not_found(self) -> None:
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stdout="", stderr="not found")
                result = gh_pr_view(999)
        assert result is None


# ---------------------------------------------------------------------------
# gh_entity_edit
# ---------------------------------------------------------------------------


class TestGhEntityEdit:
    """Tests for gh_entity_edit."""

    def test_edit_labels_graphql(self) -> None:
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = gh_entity_edit(
                    "issue", 42,
                    add_labels=["loom:building"],
                    remove_labels=["loom:issue"],
                )
        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "--add-label" in call_args
        assert "loom:building" in call_args
        assert "--remove-label" in call_args
        assert "loom:issue" in call_args

    def test_edit_returns_false_on_failure(self) -> None:
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stderr="error")
                result = gh_entity_edit("issue", 42, add_labels=["x"])
        assert result is False


# ---------------------------------------------------------------------------
# gh_issue_comment
# ---------------------------------------------------------------------------


class TestGhIssueComment:
    """Tests for gh_issue_comment."""

    def test_comment_graphql_success(self) -> None:
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = gh_issue_comment(42, "test comment")
        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "comment" in call_args
        assert "test comment" in call_args

    def test_comment_returns_false_on_failure(self) -> None:
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stderr="error")
                result = gh_issue_comment(42, "test")
        assert result is False


# ---------------------------------------------------------------------------
# Existing tests (preserved)
# ---------------------------------------------------------------------------


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


# ---------------------------------------------------------------------------
# _gh_cmd (runtime verification of gh-cached)
# ---------------------------------------------------------------------------


class TestGhCmd:
    """Tests for _gh_cmd runtime verification of gh-cached."""

    def test_returns_gh_cached_when_available_and_functional(self) -> None:
        """gh-cached is used when found on PATH and --version succeeds."""
        with mock.patch("loom_tools.common.github.shutil.which", return_value="/usr/local/bin/gh-cached"):
            with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
                mock_run.return_value = mock.Mock(returncode=0)
                assert _gh_cmd() == "gh-cached"

                # Verify --version probe was called
                mock_run.assert_called_once_with(
                    ["gh-cached", "--version"],
                    capture_output=True,
                    timeout=5,
                    check=True,
                )

    def test_falls_back_to_gh_when_version_fails(self) -> None:
        """Falls back to gh when gh-cached --version returns non-zero."""
        with mock.patch("loom_tools.common.github.shutil.which", return_value="/usr/local/bin/gh-cached"):
            with mock.patch(
                "loom_tools.common.github.subprocess.run",
                side_effect=subprocess.CalledProcessError(1, "gh-cached"),
            ):
                assert _gh_cmd() == "gh"

    def test_falls_back_to_gh_when_version_times_out(self) -> None:
        """Falls back to gh when gh-cached --version times out."""
        with mock.patch("loom_tools.common.github.shutil.which", return_value="/usr/local/bin/gh-cached"):
            with mock.patch(
                "loom_tools.common.github.subprocess.run",
                side_effect=subprocess.TimeoutExpired("gh-cached", 5),
            ):
                assert _gh_cmd() == "gh"

    def test_falls_back_to_gh_when_os_error(self) -> None:
        """Falls back to gh when gh-cached triggers an OSError (broken interpreter)."""
        with mock.patch("loom_tools.common.github.shutil.which", return_value="/usr/local/bin/gh-cached"):
            with mock.patch(
                "loom_tools.common.github.subprocess.run",
                side_effect=OSError("No such file or directory"),
            ):
                assert _gh_cmd() == "gh"

    def test_falls_back_to_gh_when_not_on_path(self) -> None:
        """Falls back to gh when gh-cached is not found on PATH."""
        with mock.patch("loom_tools.common.github.shutil.which", return_value=None):
            assert _gh_cmd() == "gh"


# ---------------------------------------------------------------------------
# GitHubForge — ForgeClient protocol conformance
# ---------------------------------------------------------------------------


class TestGitHubForgeProtocol:
    """Tests that GitHubForge satisfies the ForgeClient protocol."""

    def test_is_forge_client_instance(self) -> None:
        """GitHubForge instances satisfy the ForgeClient protocol."""
        forge = GitHubForge()
        assert isinstance(forge, ForgeClient)

    def test_forge_type(self) -> None:
        """forge_type property returns 'github'."""
        forge = GitHubForge()
        assert forge.forge_type == "github"


# ---------------------------------------------------------------------------
# GitHubForge — get_issue / list_issues
# ---------------------------------------------------------------------------


class TestGitHubForgeIssues:
    """Tests for GitHubForge issue operations."""

    def test_get_issue_returns_forge_issue(self) -> None:
        """get_issue returns a ForgeIssue dataclass."""
        issue_data = {
            "number": 42,
            "state": "OPEN",
            "title": "Test issue",
            "url": "https://github.com/o/r/issues/42",
            "labels": [{"name": "bug"}],
        }
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(issue_data), stderr=""
                )
                result = forge.get_issue(42)

        assert isinstance(result, ForgeIssue)
        assert result.number == 42
        assert result.state == "OPEN"
        assert result.title == "Test issue"
        assert result.labels == ["bug"]

    def test_get_issue_returns_none_when_not_found(self) -> None:
        """get_issue returns None for missing issue."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stdout="", stderr="not found")
                result = forge.get_issue(999)

        assert result is None

    def test_list_issues_returns_forge_issues(self) -> None:
        """list_issues returns a list of ForgeIssue objects."""
        issues_data = [
            {"number": 1, "title": "Issue 1", "labels": [{"name": "bug"}], "state": "OPEN"},
            {"number": 2, "title": "Issue 2", "labels": [], "state": "OPEN"},
        ]
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0, stdout=json.dumps(issues_data)
            )
            result = forge.list_issues(labels=["bug"])

        assert len(result) == 2
        assert all(isinstance(i, ForgeIssue) for i in result)
        assert result[0].number == 1
        assert result[0].labels == ["bug"]

    def test_comment_on_issue(self) -> None:
        """comment_on_issue delegates to gh_issue_comment."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = forge.comment_on_issue(42, "test comment")

        assert result is True


# ---------------------------------------------------------------------------
# GitHubForge — get_pull_request / list_pull_requests
# ---------------------------------------------------------------------------


class TestGitHubForgePullRequests:
    """Tests for GitHubForge pull request operations."""

    def test_get_pull_request_returns_forge_pr(self) -> None:
        """get_pull_request returns a ForgePullRequest dataclass."""
        pr_data = {
            "number": 10,
            "state": "OPEN",
            "title": "Test PR",
            "url": "https://github.com/o/r/pull/10",
            "labels": [{"name": "loom:review-requested"}],
            "headRefName": "feature/issue-42",
            "body": "Closes #42",
        }
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(
                    returncode=0, stdout=json.dumps(pr_data), stderr=""
                )
                result = forge.get_pull_request(10)

        assert isinstance(result, ForgePullRequest)
        assert result.number == 10
        assert result.state == "OPEN"
        assert result.head_branch == "feature/issue-42"
        assert result.labels == ["loom:review-requested"]

    def test_get_pull_request_returns_none_when_not_found(self) -> None:
        """get_pull_request returns None for missing PR."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=1, stdout="", stderr="not found")
                result = forge.get_pull_request(999)

        assert result is None

    def test_list_pull_requests_returns_forge_prs(self) -> None:
        """list_pull_requests returns a list of ForgePullRequest objects."""
        prs_data = [
            {"number": 10, "title": "PR 1", "labels": [], "state": "OPEN"},
        ]
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0, stdout=json.dumps(prs_data)
            )
            result = forge.list_pull_requests()

        assert len(result) == 1
        assert isinstance(result[0], ForgePullRequest)
        assert result[0].number == 10

    def test_comment_on_pull_request(self) -> None:
        """comment_on_pull_request runs gh pr comment."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stderr="")
            result = forge.comment_on_pull_request(10, "LGTM!")

        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "pr" in call_args
        assert "comment" in call_args

    def test_merge_pull_request(self) -> None:
        """merge_pull_request runs gh pr merge."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stderr="")
            result = forge.merge_pull_request(10, method="squash")

        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "pr" in call_args
        assert "merge" in call_args
        assert "--squash" in call_args

    def test_auto_merge_pull_request_success(self) -> None:
        """auto_merge_pull_request delegates to gh pr merge --auto."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="", stderr="")
            result = forge.auto_merge_pull_request(10, method="squash")

        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "pr" in call_args
        assert "merge" in call_args
        assert "--auto" in call_args
        assert "--squash" in call_args
        assert "--delete-branch" in call_args

    def test_auto_merge_pull_request_failure(self) -> None:
        """auto_merge_pull_request returns False on gh failure."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=1, stdout="", stderr="auto-merge not available",
            )
            result = forge.auto_merge_pull_request(10)

        assert result is False

    def test_get_pull_request_reviews(self) -> None:
        """get_pull_request_reviews returns review list."""
        review_data = {
            "reviews": [
                {"state": "APPROVED", "author": {"login": "user1"}},
            ],
        }
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0, stdout=json.dumps(review_data), stderr=""
            )
            result = forge.get_pull_request_reviews(10)

        assert len(result) == 1
        assert result[0]["state"] == "APPROVED"


# ---------------------------------------------------------------------------
# GitHubForge — label operations
# ---------------------------------------------------------------------------


class TestGitHubForgeLabels:
    """Tests for GitHubForge label operations."""

    def test_add_labels(self) -> None:
        """add_labels delegates to gh_entity_edit."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = forge.add_labels("issue", 42, ["bug", "urgent"])

        assert result is True

    def test_remove_labels(self) -> None:
        """remove_labels delegates to gh_entity_edit."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = forge.remove_labels("issue", 42, ["loom:issue"])

        assert result is True

    def test_transition_labels(self) -> None:
        """transition_labels adds and removes in one call."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.get_api_mode", return_value=ApiMode.GRAPHQL):
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.return_value = mock.Mock(returncode=0, stderr="")
                result = forge.transition_labels(
                    "issue", 42,
                    add=["loom:building"],
                    remove=["loom:issue"],
                )

        assert result is True
        call_args = mock_gh.call_args[0][0]
        assert "--add-label" in call_args
        assert "--remove-label" in call_args


# ---------------------------------------------------------------------------
# GitHubForge — CI status
# ---------------------------------------------------------------------------


class TestGitHubForgeCIStatus:
    """Tests for GitHubForge CI status."""

    def test_get_default_branch_ci_status_returns_dataclass(self) -> None:
        """get_default_branch_ci_status returns a ForgeCIStatus."""
        mock_runs = [
            {"name": "CI", "conclusion": "success", "status": "completed", "headBranch": "main"},
        ]
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0, stdout=json.dumps(mock_runs)
            )
            result = forge.get_default_branch_ci_status()

        assert isinstance(result, ForgeCIStatus)
        assert result.status == "passing"
        assert result.total_runs == 1

    def test_get_commit_ci_status_passing(self) -> None:
        """get_commit_ci_status returns passing when all checks pass."""
        check_runs_data = json.dumps({
            "check_runs": [
                {"name": "CI", "conclusion": "success"},
                {"name": "Lint", "conclusion": "success"},
            ],
        })
        status_data = json.dumps({
            "statuses": [
                {"context": "coverage", "state": "success"},
            ],
        })
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0, stdout="git@github.com:owner/repo.git\n",
            )
            _reset_nwo_cache()
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout=check_runs_data, stderr=""),
                    mock.Mock(returncode=0, stdout=status_data, stderr=""),
                ]
                result = forge.get_commit_ci_status("abc123def")
        _reset_nwo_cache()

        assert isinstance(result, ForgeCIStatus)
        assert result.status == "passing"
        assert result.total_runs == 3
        assert result.failed_runs == []

    def test_get_commit_ci_status_failing(self) -> None:
        """get_commit_ci_status returns failing when a check fails."""
        check_runs_data = json.dumps({
            "check_runs": [
                {"name": "CI", "conclusion": "failure"},
                {"name": "Lint", "conclusion": "success"},
            ],
        })
        status_data = json.dumps({"statuses": []})
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0, stdout="git@github.com:owner/repo.git\n",
            )
            _reset_nwo_cache()
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout=check_runs_data, stderr=""),
                    mock.Mock(returncode=0, stdout=status_data, stderr=""),
                ]
                result = forge.get_commit_ci_status("abc123def")
        _reset_nwo_cache()

        assert result.status == "failing"
        assert "CI" in result.failed_runs

    def test_get_commit_ci_status_no_checks(self) -> None:
        """get_commit_ci_status returns unknown when no checks found."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0, stdout="git@github.com:owner/repo.git\n",
            )
            _reset_nwo_cache()
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout=json.dumps({"check_runs": []}), stderr=""),
                    mock.Mock(returncode=0, stdout=json.dumps({"statuses": []}), stderr=""),
                ]
                result = forge.get_commit_ci_status("abc123def")
        _reset_nwo_cache()

        assert result.status == "unknown"

    def test_get_commit_ci_status_commit_status_error(self) -> None:
        """Commit status 'error' state is treated as failure."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0, stdout="git@github.com:owner/repo.git\n",
            )
            _reset_nwo_cache()
            with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
                mock_gh.side_effect = [
                    mock.Mock(returncode=0, stdout=json.dumps({"check_runs": []}), stderr=""),
                    mock.Mock(returncode=0, stdout=json.dumps({
                        "statuses": [{"context": "deploy", "state": "error"}],
                    }), stderr=""),
                ]
                result = forge.get_commit_ci_status("abc123def")
        _reset_nwo_cache()

        assert result.status == "failing"
        assert "deploy" in result.failed_runs

    def test_get_commit_ci_status_no_nwo(self) -> None:
        """Returns unknown when repo NWO cannot be determined."""
        forge = GitHubForge()
        _reset_nwo_cache()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(returncode=1, stdout="")
            result = forge.get_commit_ci_status("abc123")
        _reset_nwo_cache()

        assert result.status == "unknown"


# ---------------------------------------------------------------------------
# GitHubForge — repo metadata
# ---------------------------------------------------------------------------


class TestGitHubForgeRepoMetadata:
    """Tests for GitHubForge repository metadata."""

    def test_get_repo_nwo(self) -> None:
        """get_repo_nwo delegates to module-level get_repo_nwo."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.subprocess.run") as mock_run:
            mock_run.return_value = mock.Mock(
                returncode=0, stdout="git@github.com:owner/repo.git\n"
            )
            _reset_nwo_cache()
            result = forge.get_repo_nwo()

        assert result == "owner/repo"
        _reset_nwo_cache()

    def test_get_repo_default_branch(self) -> None:
        """get_repo_default_branch parses defaultBranchRef."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(
                returncode=0,
                stdout=json.dumps({"defaultBranchRef": {"name": "main"}}),
                stderr="",
            )
            result = forge.get_repo_default_branch()

        assert result == "main"


# ---------------------------------------------------------------------------
# GitHubForge — GitHub-specific run() escape hatch
# ---------------------------------------------------------------------------


class TestGitHubForgeRun:
    """Tests for GitHubForge.run() (GitHub-specific, not on ForgeClient)."""

    def test_run_delegates_to_gh_run(self) -> None:
        """run() delegates to module-level gh_run."""
        forge = GitHubForge()
        with mock.patch("loom_tools.common.github.gh_run") as mock_gh:
            mock_gh.return_value = mock.Mock(returncode=0, stdout="ok", stderr="")
            result = forge.run(["issue", "list"], check=False)

        assert result.returncode == 0
        mock_gh.assert_called_once()


# ---------------------------------------------------------------------------
# get_forge() singleton
# ---------------------------------------------------------------------------


class TestGetForge:
    """Tests for the get_forge() factory."""

    def setup_method(self) -> None:
        _reset_forge()

    def teardown_method(self) -> None:
        _reset_forge()

    def test_returns_github_forge(self) -> None:
        """get_forge() returns a GitHubForge instance."""
        forge = get_forge()
        assert isinstance(forge, GitHubForge)

    def test_returns_same_instance(self) -> None:
        """get_forge() returns the same singleton instance."""
        forge1 = get_forge()
        forge2 = get_forge()
        assert forge1 is forge2

    def test_satisfies_forge_client_protocol(self) -> None:
        """get_forge() returns a ForgeClient-compatible instance."""
        forge = get_forge()
        assert isinstance(forge, ForgeClient)

    def test_reset_clears_singleton(self) -> None:
        """_reset_forge() allows creating a new instance."""
        forge1 = get_forge()
        _reset_forge()
        forge2 = get_forge()
        assert forge1 is not forge2
