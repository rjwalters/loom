"""Tests for loom_tools.common.gitea — GiteaForge implementation.

All HTTP calls are mocked via ``requests.Session``. No real network
requests are made.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any
from unittest import mock

import pytest
import requests

from loom_tools.common.forge import (
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_BASE_URL = "https://gitea.example.com"
_TOKEN = "test-token-12345"

# Minimal Gitea config
_GITEA_CONFIG = {
    "gitea": {
        "url": _BASE_URL,
        "token": _TOKEN,
    },
}


def _mock_response(
    status_code: int = 200,
    json_data: Any = None,
    text: str = "",
) -> mock.MagicMock:
    """Create a mock requests.Response."""
    resp = mock.MagicMock(spec=requests.Response)
    resp.status_code = status_code
    resp.text = text or json.dumps(json_data) if json_data is not None else ""
    if json_data is not None:
        resp.json.return_value = json_data
    else:
        resp.json.side_effect = ValueError("No JSON")
    return resp


def _make_forge(
    nwo: str = "owner/repo",
    config: dict[str, Any] | None = None,
    token_env: str | None = None,
) -> Any:
    """Create a GiteaForge with mocked config and git remote.

    Pre-populates the NWO cache so tests don't need to mock subprocess.
    """
    from loom_tools.common.gitea import GiteaForge

    env_patch = {}
    if token_env is not None:
        env_patch["GITEA_TOKEN"] = token_env

    with (
        mock.patch("loom_tools.common.gitea.get_forge_config", return_value=config or _GITEA_CONFIG),
        mock.patch.dict(os.environ, env_patch, clear=False),
    ):
        forge = GiteaForge(cwd=Path("/tmp/test"))

    # Pre-populate NWO cache to avoid subprocess calls in tests
    forge._nwo_cache = nwo
    return forge


# ===========================================================================
# Protocol compliance
# ===========================================================================


class TestProtocolCompliance:
    """Verify GiteaForge satisfies the ForgeClient protocol."""

    def test_isinstance_check(self) -> None:
        forge = _make_forge()
        assert isinstance(forge, ForgeClient)

    def test_forge_type(self) -> None:
        forge = _make_forge()
        assert forge.forge_type == "gitea"


# ===========================================================================
# Constructor
# ===========================================================================


class TestConstructor:
    """Constructor validation tests."""

    def test_raises_without_base_url(self) -> None:
        with (
            mock.patch(
                "loom_tools.common.gitea.get_forge_config",
                return_value={"gitea": {"token": "tok"}},
            ),
            mock.patch.dict(os.environ, {}, clear=False),
            pytest.raises(ValueError, match="base URL is required"),
        ):
            from loom_tools.common.gitea import GiteaForge
            GiteaForge()

    def test_raises_without_token(self) -> None:
        with (
            mock.patch(
                "loom_tools.common.gitea.get_forge_config",
                return_value={"gitea": {"url": _BASE_URL}},
            ),
            mock.patch.dict(os.environ, {"GITEA_TOKEN": ""}, clear=False),
            pytest.raises(ValueError, match="API token is required"),
        ):
            from loom_tools.common.gitea import GiteaForge
            GiteaForge()

    def test_env_token_takes_priority(self) -> None:
        forge = _make_forge(token_env="env-token")
        assert forge._session.headers["Authorization"] == "token env-token"

    def test_config_token_fallback(self) -> None:
        forge = _make_forge()
        assert forge._session.headers["Authorization"] == f"token {_TOKEN}"

    def test_auth_header_format(self) -> None:
        forge = _make_forge()
        # Must be "token xxx", NOT "Bearer xxx"
        auth = forge._session.headers["Authorization"]
        assert auth.startswith("token ")
        assert not auth.startswith("Bearer ")


# ===========================================================================
# Issue operations
# ===========================================================================


class TestGetIssue:
    """Tests for get_issue()."""

    def test_returns_forge_issue(self) -> None:
        forge = _make_forge()
        issue_data = {
            "number": 42,
            "state": "open",
            "title": "Test issue",
            "html_url": "https://gitea.example.com/owner/repo/issues/42",
            "labels": [{"name": "bug", "id": 1}],
            "body": "Some body",
        }
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=issue_data)):
            result = forge.get_issue(42)

        assert result is not None
        assert isinstance(result, ForgeIssue)
        assert result.number == 42
        assert result.state == "OPEN"  # normalized to uppercase
        assert result.title == "Test issue"
        assert result.labels == ["bug"]

    def test_returns_none_for_404(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(404)):
            result = forge.get_issue(999)
        assert result is None

    def test_filters_out_pull_requests(self) -> None:
        forge = _make_forge()
        pr_as_issue = {
            "number": 10,
            "state": "open",
            "title": "A PR",
            "html_url": "https://gitea.example.com/owner/repo/issues/10",
            "labels": [],
            "pull_request": {"merged": False},
        }
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=pr_as_issue)):
            result = forge.get_issue(10)
        assert result is None


class TestListIssues:
    """Tests for list_issues()."""

    def test_returns_list(self) -> None:
        forge = _make_forge()
        issues = [
            {"number": 1, "state": "open", "title": "A", "html_url": "u1", "labels": []},
            {"number": 2, "state": "closed", "title": "B", "html_url": "u2", "labels": []},
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=issues)):
            result = forge.list_issues()
        assert len(result) == 2
        assert result[0].number == 1
        assert result[1].state == "CLOSED"

    def test_empty_on_error(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            result = forge.list_issues()
        assert result == []

    def test_passes_label_and_state_params(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=[])) as mock_req:
            forge.list_issues(labels=["bug", "urgent"], state="closed", limit=10)

        _, kwargs = mock_req.call_args
        params = kwargs["params"]
        assert params["labels"] == "bug,urgent"
        assert params["state"] == "closed"
        assert params["limit"] == 10
        assert params["type"] == "issues"


class TestCreateIssue:
    """Tests for create_issue()."""

    def test_creates_issue(self) -> None:
        forge = _make_forge()
        created = {
            "number": 99,
            "state": "open",
            "title": "New",
            "html_url": "u99",
            "labels": [],
        }
        # Mock label cache population + create
        responses = [
            _mock_response(json_data=[{"name": "bug", "id": 5}]),  # label cache
            _mock_response(json_data=created),  # create
        ]
        with mock.patch.object(forge._session, "request", side_effect=responses):
            result = forge.create_issue("New", "body", labels=["bug"])
        assert result is not None
        assert result.number == 99

    def test_returns_none_on_failure(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            result = forge.create_issue("Fail", "body")
        assert result is None


class TestCloseIssue:
    """Tests for close_issue()."""

    def test_success(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data={"state": "closed"})):
            assert forge.close_issue(42) is True

    def test_failure(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            assert forge.close_issue(42) is False


class TestCommentOnIssue:
    """Tests for comment_on_issue()."""

    def test_success(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data={"id": 1})):
            assert forge.comment_on_issue(42, "Hello") is True


# ===========================================================================
# Pull request operations
# ===========================================================================


class TestGetPullRequest:
    """Tests for get_pull_request()."""

    def test_returns_forge_pr(self) -> None:
        forge = _make_forge()
        pr_data = {
            "number": 10,
            "state": "open",
            "title": "Fix bug",
            "html_url": "https://gitea.example.com/owner/repo/pulls/10",
            "labels": [{"name": "enhancement", "id": 2}],
            "head": {"ref": "feature/fix", "label": "feature/fix"},
            "body": "Closes #42",
            "merged": False,
        }
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=pr_data)):
            result = forge.get_pull_request(10)

        assert result is not None
        assert isinstance(result, ForgePullRequest)
        assert result.number == 10
        assert result.state == "OPEN"
        assert result.head_branch == "feature/fix"
        assert result.labels == ["enhancement"]

    def test_merged_state(self) -> None:
        forge = _make_forge()
        pr_data = {
            "number": 11,
            "state": "closed",
            "title": "Done",
            "html_url": "u",
            "labels": [],
            "head": {"ref": "main"},
            "merged": True,
        }
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=pr_data)):
            result = forge.get_pull_request(11)
        assert result is not None
        assert result.state == "MERGED"

    def test_returns_none_for_404(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(404)):
            assert forge.get_pull_request(999) is None


class TestListPullRequests:
    """Tests for list_pull_requests()."""

    def test_returns_list(self) -> None:
        forge = _make_forge()
        prs = [
            {"number": 1, "state": "open", "title": "PR1", "html_url": "u", "labels": [],
             "head": {"ref": "branch-1"}, "merged": False},
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=prs)):
            result = forge.list_pull_requests()
        assert len(result) == 1

    def test_head_filter(self) -> None:
        forge = _make_forge()
        prs = [
            {"number": 1, "state": "open", "title": "PR1", "html_url": "u", "labels": [],
             "head": {"ref": "branch-a"}, "merged": False},
            {"number": 2, "state": "open", "title": "PR2", "html_url": "u", "labels": [],
             "head": {"ref": "branch-b"}, "merged": False},
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=prs)):
            result = forge.list_pull_requests(head="branch-b")
        assert len(result) == 1
        assert result[0].number == 2

    def test_search_param_warns(self) -> None:
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=[])),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            forge.list_pull_requests(search="some query")
        mock_logger.warning.assert_called_once()
        assert "search" in mock_logger.warning.call_args[0][0].lower()


class TestMergePullRequest:
    """Tests for merge_pull_request()."""

    def test_squash_merge(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data={})) as mock_req:
            assert forge.merge_pull_request(10, "squash") is True

        call_args = mock_req.call_args
        assert call_args[1]["json"]["Do"] == "squash"
        assert call_args[1]["json"]["delete_branch_after_merge"] is True

    def test_failure(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(409)):
            assert forge.merge_pull_request(10) is False


class TestClosePullRequest:
    """Tests for close_pull_request()."""

    def test_close_with_comment(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data={})) as mock_req:
            assert forge.close_pull_request(10, comment="Closing") is True
        # Should have made 2 calls: comment + close
        assert mock_req.call_count == 2

    def test_close_without_comment(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data={})) as mock_req:
            assert forge.close_pull_request(10) is True
        assert mock_req.call_count == 1


class TestGetPullRequestReviews:
    """Tests for get_pull_request_reviews()."""

    def test_returns_reviews_with_normalized_state(self) -> None:
        forge = _make_forge()
        reviews = [
            {"state": "approved", "user": {"login": "reviewer"}},
            {"state": "request_changes", "user": {"login": "other"}},
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=reviews)):
            result = forge.get_pull_request_reviews(10)
        assert len(result) == 2
        assert result[0]["state"] == "APPROVED"
        assert result[1]["state"] == "REQUEST_CHANGES"

    def test_empty_on_error(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            assert forge.get_pull_request_reviews(10) == []


# ===========================================================================
# Label operations
# ===========================================================================


class TestLabelOperations:
    """Tests for add_labels, remove_labels, transition_labels."""

    def test_add_labels(self) -> None:
        forge = _make_forge()
        responses = [
            _mock_response(json_data=[{"name": "bug", "id": 1}, {"name": "fix", "id": 2}]),  # cache
            _mock_response(json_data=[{"name": "bug", "id": 1}]),  # add
        ]
        with mock.patch.object(forge._session, "request", side_effect=responses):
            assert forge.add_labels("issue", 42, ["bug"]) is True

    def test_remove_labels(self) -> None:
        forge = _make_forge()
        responses = [
            _mock_response(json_data=[{"name": "bug", "id": 1}]),  # cache
            _mock_response(json_data={}),  # remove
        ]
        with mock.patch.object(forge._session, "request", side_effect=responses):
            assert forge.remove_labels("issue", 42, ["bug"]) is True

    def test_transition_labels(self) -> None:
        forge = _make_forge()
        responses = [
            _mock_response(json_data=[{"name": "old", "id": 1}, {"name": "new", "id": 2}]),  # cache for remove
            _mock_response(json_data={}),  # remove "old"
            _mock_response(json_data=[{"name": "old", "id": 1}, {"name": "new", "id": 2}]),  # cache for add (already populated but re-resolved)
            _mock_response(json_data=[{"name": "new", "id": 2}]),  # add "new"
        ]
        # Pre-populate label cache to avoid extra requests
        forge._label_cache = {"old": 1, "new": 2}
        responses_simple = [
            _mock_response(json_data={}),  # remove "old"
            _mock_response(json_data=[{"name": "new", "id": 2}]),  # add "new"
        ]
        with mock.patch.object(forge._session, "request", side_effect=responses_simple):
            assert forge.transition_labels("pr", 10, add=["new"], remove=["old"]) is True


class TestLabelCache:
    """Tests for label name→ID resolution cache."""

    def test_lazy_population(self) -> None:
        forge = _make_forge()
        assert forge._label_cache is None

        labels = [{"name": "bug", "id": 1}, {"name": "feat", "id": 2}]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=labels)):
            ids = forge._resolve_label_ids(["bug", "feat"])

        assert forge._label_cache is not None
        assert ids == [1, 2]

    def test_cache_invalidation_on_missing(self) -> None:
        forge = _make_forge()
        forge._label_cache = {"bug": 1}

        # "new-label" not in cache -> triggers re-fetch
        updated_labels = [{"name": "bug", "id": 1}, {"name": "new-label", "id": 3}]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=updated_labels)):
            ids = forge._resolve_label_ids(["bug", "new-label"])

        assert set(ids) == {1, 3}

    def test_unknown_label_warns(self) -> None:
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=[])),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            ids = forge._resolve_label_ids(["nonexistent"])
        assert ids == []
        mock_logger.warning.assert_called()


# ===========================================================================
# State normalization
# ===========================================================================


class TestStateNormalization:
    """Tests for state normalization (lowercase -> uppercase)."""

    def test_open(self) -> None:
        from loom_tools.common.gitea import GiteaForge
        assert GiteaForge._normalize_state("open") == "OPEN"

    def test_closed(self) -> None:
        from loom_tools.common.gitea import GiteaForge
        assert GiteaForge._normalize_state("closed") == "CLOSED"

    def test_merged(self) -> None:
        from loom_tools.common.gitea import GiteaForge
        assert GiteaForge._normalize_state("closed", merged=True) == "MERGED"


# ===========================================================================
# CI status
# ===========================================================================


class TestCIStatus:
    """Tests for get_default_branch_ci_status() and get_commit_ci_status()."""

    def test_passing(self) -> None:
        forge = _make_forge()
        # First call: get repo info (default branch), second: commit statuses,
        # third: actions runs (404 = not available)
        repo_resp = _mock_response(json_data={"default_branch": "main"})
        statuses_resp = _mock_response(json_data=[
            {"context": "ci/test", "status": "success"},
            {"context": "ci/lint", "status": "success"},
        ])
        actions_resp = _mock_response(404)  # Actions API not available
        with mock.patch.object(forge._session, "request", side_effect=[repo_resp, statuses_resp, actions_resp]):
            result = forge.get_default_branch_ci_status()

        assert isinstance(result, ForgeCIStatus)
        assert result.status == "passing"
        assert result.failed_runs == []
        assert result.total_runs == 2

    def test_failing(self) -> None:
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [
            {"context": "ci/test", "status": "failure"},
            {"context": "ci/lint", "status": "success"},
        ]
        actions_resp = _mock_response(404)
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            actions_resp,
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "failing"
        assert "ci/test" in result.failed_runs

    def test_error_status_treated_as_failure(self) -> None:
        """Gitea 'error' status maps to failure."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [{"context": "ci/deploy", "status": "error"}]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "failing"
        assert "ci/deploy" in result.failed_runs

    def test_warning_status_treated_as_pending(self) -> None:
        """Gitea 'warning' status maps to pending (not failure)."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [{"context": "ci/check", "status": "warning"}]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.failed_runs == []
        assert "pending" in result.message.lower()

    def test_pending_status(self) -> None:
        """Gitea 'pending' status is not treated as failure."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [
            {"context": "ci/test", "status": "pending"},
            {"context": "ci/lint", "status": "success"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.failed_runs == []

    def test_empty_statuses_returns_unknown(self) -> None:
        """Empty status list returns unknown."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=[]),
            _mock_response(404),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "unknown"

    def test_no_ci_configured(self) -> None:
        """API error on status fetch returns unknown."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=[]),  # empty statuses
            _mock_response(404),  # no actions
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "unknown"

    def test_latest_status_per_context(self) -> None:
        """Only the latest status per context is used."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        # First in list is latest; ci/test succeeded after previous failure
        statuses = [
            {"context": "ci/test", "status": "success"},
            {"context": "ci/test", "status": "failure"},  # older, ignored
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.total_runs == 1

    def test_cannot_determine_default_branch(self) -> None:
        """Returns unknown when default branch cannot be determined."""
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            result = forge.get_default_branch_ci_status()
        assert result.status == "unknown"


class TestCIStatusActionsRuns:
    """Tests for Gitea Actions runs integration."""

    def test_actions_runs_passing(self) -> None:
        """Actions runs that pass contribute to passing status."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = []  # No commit statuses
        actions = {"workflow_runs": [
            {"name": "CI", "status": "completed", "conclusion": "success"},
            {"name": "Lint", "status": "completed", "conclusion": "success"},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.total_runs == 2

    def test_actions_runs_failing(self) -> None:
        """Failed Actions runs are reported as failures."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = []
        actions = {"workflow_runs": [
            {"name": "CI", "status": "completed", "conclusion": "failure"},
            {"name": "Lint", "status": "completed", "conclusion": "success"},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "failing"
        assert "CI" in result.failed_runs

    def test_actions_cancelled_treated_as_failure(self) -> None:
        """Cancelled Actions runs are treated as failures."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        actions = {"workflow_runs": [
            {"name": "Deploy", "status": "completed", "conclusion": "cancelled"},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=[]),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "failing"
        assert "Deploy" in result.failed_runs

    def test_actions_in_progress_not_counted(self) -> None:
        """In-progress Actions runs are not counted as failures."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        actions = {"workflow_runs": [
            {"name": "CI", "status": "running", "conclusion": ""},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=[]),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"

    def test_actions_api_404_fallback(self) -> None:
        """When Actions API returns 404, falls back to commit statuses only."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [{"context": "ci/test", "status": "success"}]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),  # Actions API not available
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.total_runs == 1

    def test_merge_commit_statuses_and_actions(self) -> None:
        """Both commit statuses and Actions runs are merged."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [{"context": "external/check", "status": "success"}]
        actions = {"workflow_runs": [
            {"name": "CI", "status": "completed", "conclusion": "success"},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"
        assert result.total_runs == 2  # 1 commit status + 1 actions run

    def test_merge_with_failure_from_either_source(self) -> None:
        """Failure from either source results in overall failure."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        statuses = [{"context": "external/check", "status": "failure"}]
        actions = {"workflow_runs": [
            {"name": "CI", "status": "completed", "conclusion": "success"},
        ]}
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(json_data=actions),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "failing"
        assert "external/check" in result.failed_runs

    def test_actions_list_format(self) -> None:
        """Handle Gitea Actions API returning a plain list instead of dict."""
        forge = _make_forge()
        forge._default_branch_cache = "main"
        actions_list = [
            {"name": "CI", "status": "completed", "conclusion": "success"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=[]),
            _mock_response(json_data=actions_list),
        ]):
            result = forge.get_default_branch_ci_status()
        assert result.status == "passing"


class TestGetCommitCIStatus:
    """Tests for get_commit_ci_status() with specific SHA."""

    def test_commit_status_for_sha(self) -> None:
        """get_commit_ci_status queries the correct SHA."""
        forge = _make_forge()
        statuses = [{"context": "ci/test", "status": "success"}]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=statuses),
            _mock_response(404),
        ]) as mock_req:
            result = forge.get_commit_ci_status("abc123")

        assert result.status == "passing"
        # Verify the SHA was used in the URL
        first_call_url = mock_req.call_args_list[0][1].get("url", "") or mock_req.call_args_list[0][0][1]
        assert "abc123" in str(first_call_url) or "abc123" in str(mock_req.call_args_list[0])

    def test_commit_status_no_repo(self) -> None:
        """Returns unknown when repo cannot be determined."""
        forge = _make_forge()
        forge._nwo_cache = None
        with mock.patch.object(forge, "get_repo_nwo", return_value=None):
            result = forge.get_commit_ci_status("abc123")
        assert result.status == "unknown"


# ===========================================================================
# Repository metadata
# ===========================================================================


class TestRepoMetadata:
    """Tests for get_repo_nwo() and get_repo_default_branch()."""

    def test_nwo_from_https(self) -> None:
        forge = _make_forge(nwo="myorg/myrepo")
        with mock.patch("subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0,
                stdout="https://gitea.example.com/myorg/myrepo.git\n",
            )
            assert forge.get_repo_nwo() == "myorg/myrepo"

    def test_nwo_from_ssh(self) -> None:
        forge = _make_forge()
        with mock.patch("subprocess.run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(
                args=[], returncode=0,
                stdout="git@gitea.example.com:team/project.git\n",
            )
            # Clear cache
            forge._nwo_cache = None
            assert forge.get_repo_nwo() == "team/project"

    def test_default_branch(self) -> None:
        forge = _make_forge()
        repo_data = {"default_branch": "develop"}
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=repo_data)):
            assert forge.get_repo_default_branch() == "develop"


# ===========================================================================
# Batch operations
# ===========================================================================


class TestBatchOperations:
    """Tests for get_issues_batch() and find_pull_request_for_issue()."""

    def test_get_issues_batch(self) -> None:
        forge = _make_forge()
        issue1 = {"number": 1, "state": "open", "title": "A", "html_url": "u1", "labels": []}
        issue2 = {"number": 2, "state": "closed", "title": "B", "html_url": "u2", "labels": []}

        def side_effect(method, url, **kwargs):
            if "/issues/1" in url:
                return _mock_response(json_data=issue1)
            if "/issues/2" in url:
                return _mock_response(json_data=issue2)
            return _mock_response(404)

        with mock.patch.object(forge._session, "request", side_effect=side_effect):
            results = forge.get_issues_batch([1, 2, 999])

        assert results[1] is not None
        assert results[1].number == 1
        assert results[2] is not None
        assert results[999] is None

    def test_find_pr_by_branch(self) -> None:
        forge = _make_forge()
        prs = [
            {"number": 50, "state": "open", "title": "PR", "html_url": "u",
             "labels": [], "head": {"ref": "feature/issue-42"}, "merged": False},
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=prs)):
            result = forge.find_pull_request_for_issue(42)
        assert result == 50

    def test_find_pr_by_body_closes(self) -> None:
        forge = _make_forge()
        # First call returns no match for branch, second returns all PRs
        no_match_prs = [
            {"number": 1, "state": "open", "title": "Other", "html_url": "u",
             "labels": [], "head": {"ref": "other-branch"}, "merged": False},
        ]
        all_prs = [
            {"number": 60, "state": "open", "title": "Fix", "html_url": "u",
             "labels": [], "head": {"ref": "my-fix"}, "merged": False,
             "body": "This PR Closes #42 and does stuff"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=no_match_prs),  # head filter (client-side, no match)
            _mock_response(json_data=all_prs),  # body search
        ]):
            result = forge.find_pull_request_for_issue(42)
        assert result == 60

    def test_find_pr_by_body_fixes(self) -> None:
        """find_pull_request_for_issue matches 'Fixes #N' in PR body."""
        forge = _make_forge()
        no_match = []  # no branch match
        all_prs = [
            {"number": 70, "state": "open", "title": "Fix", "html_url": "u",
             "labels": [], "head": {"ref": "hotfix"}, "merged": False,
             "body": "Fixes #42"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=no_match),
            _mock_response(json_data=all_prs),
        ]):
            result = forge.find_pull_request_for_issue(42)
        assert result == 70

    def test_find_pr_by_body_resolves(self) -> None:
        """find_pull_request_for_issue matches 'Resolves #N' in PR body."""
        forge = _make_forge()
        no_match = []
        all_prs = [
            {"number": 80, "state": "open", "title": "Resolve", "html_url": "u",
             "labels": [], "head": {"ref": "fix-it"}, "merged": False,
             "body": "Resolves #42 with a proper fix"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=no_match),
            _mock_response(json_data=all_prs),
        ]):
            result = forge.find_pull_request_for_issue(42)
        assert result == 80

    def test_find_pr_by_body_case_insensitive(self) -> None:
        """find_pull_request_for_issue matches closing keywords case-insensitively."""
        forge = _make_forge()
        no_match = []
        all_prs = [
            {"number": 90, "state": "open", "title": "Fix", "html_url": "u",
             "labels": [], "head": {"ref": "fix-branch"}, "merged": False,
             "body": "fixes #42"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=no_match),
            _mock_response(json_data=all_prs),
        ]):
            result = forge.find_pull_request_for_issue(42)
        assert result == 90

    def test_find_pr_no_false_positive_on_partial_number(self) -> None:
        """find_pull_request_for_issue does not match 'Closes #421' for issue 42."""
        forge = _make_forge()
        no_match = []
        all_prs = [
            {"number": 100, "state": "open", "title": "Other", "html_url": "u",
             "labels": [], "head": {"ref": "other"}, "merged": False,
             "body": "Closes #421"},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=no_match),
            _mock_response(json_data=all_prs),
        ]):
            result = forge.find_pull_request_for_issue(42)
        assert result is None


# ===========================================================================
# Error handling
# ===========================================================================


class TestErrorHandling:
    """Tests for error paths."""

    def test_auth_failure_logs_error(self) -> None:
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", return_value=_mock_response(401)),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            result = forge.get_issue(42)
        assert result is None
        mock_logger.error.assert_called()
        assert "auth" in mock_logger.error.call_args[0][0].lower()

    def test_connection_error(self) -> None:
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", side_effect=requests.ConnectionError("down")),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            result = forge.get_issue(42)
        assert result is None
        mock_logger.error.assert_called()

    def test_timeout_error(self) -> None:
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", side_effect=requests.Timeout("timeout")),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            result = forge.get_issue(42)
        assert result is None
        mock_logger.error.assert_called()

    def test_server_error_returns_none(self) -> None:
        forge = _make_forge()
        with mock.patch.object(forge._session, "request", return_value=_mock_response(500)):
            assert forge.get_issue(42) is None
            assert forge.list_issues() == []
            assert forge.get_pull_request(10) is None

    def test_403_includes_scope_hint(self) -> None:
        """403 error message includes token scope guidance."""
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", return_value=_mock_response(403)),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            forge.get_issue(42)
        error_msg = mock_logger.error.call_args[0][0] % mock_logger.error.call_args[0][1:]
        assert "scope" in error_msg.lower() or "permission" in error_msg.lower()

    def test_401_includes_token_validity_hint(self) -> None:
        """401 error message includes token validity guidance."""
        forge = _make_forge()
        with (
            mock.patch.object(forge._session, "request", return_value=_mock_response(401)),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            forge.get_issue(42)
        error_msg = mock_logger.error.call_args[0][0] % mock_logger.error.call_args[0][1:]
        assert "valid" in error_msg.lower() or "expired" in error_msg.lower()


# ===========================================================================
# Rate limiting
# ===========================================================================


class TestRateLimiting:
    """Tests for HTTP 429 retry with backoff."""

    def test_retries_on_429_then_succeeds(self) -> None:
        """429 is retried and succeeds on the second attempt."""
        forge = _make_forge()
        rate_limit_resp = _mock_response(429)
        rate_limit_resp.headers = {}
        success_resp = _mock_response(json_data={"number": 1, "state": "open", "title": "A", "html_url": "u", "labels": []})

        with (
            mock.patch.object(forge._session, "request", side_effect=[rate_limit_resp, success_resp]),
            mock.patch("loom_tools.common.gitea.time.sleep") as mock_sleep,
            mock.patch("loom_tools.common.gitea.logger"),
        ):
            result = forge.get_issue(1)

        assert result is not None
        assert result.number == 1
        mock_sleep.assert_called_once()

    def test_respects_retry_after_header(self) -> None:
        """Uses Retry-After header when present."""
        forge = _make_forge()
        rate_limit_resp = _mock_response(429)
        rate_limit_resp.headers = {"Retry-After": "5"}
        success_resp = _mock_response(json_data={"number": 1, "state": "open", "title": "A", "html_url": "u", "labels": []})

        with (
            mock.patch.object(forge._session, "request", side_effect=[rate_limit_resp, success_resp]),
            mock.patch("loom_tools.common.gitea.time.sleep") as mock_sleep,
            mock.patch("loom_tools.common.gitea.logger"),
        ):
            forge.get_issue(1)

        mock_sleep.assert_called_once_with(5.0)

    def test_exhausts_retries_on_persistent_429(self) -> None:
        """Returns None after exhausting all retries."""
        forge = _make_forge()
        rate_limit_resp = _mock_response(429)
        rate_limit_resp.headers = {}

        with (
            mock.patch.object(forge._session, "request", return_value=rate_limit_resp),
            mock.patch("loom_tools.common.gitea.time.sleep"),
            mock.patch("loom_tools.common.gitea.logger") as mock_logger,
        ):
            result = forge.get_issue(1)

        assert result is None
        mock_logger.error.assert_called()
        assert "exhausted" in mock_logger.error.call_args[0][0].lower()

    def test_exponential_backoff(self) -> None:
        """Backoff doubles on each retry."""
        forge = _make_forge()
        rate_limit_resp = _mock_response(429)
        rate_limit_resp.headers = {}
        success_resp = _mock_response(json_data={"number": 1, "state": "open", "title": "A", "html_url": "u", "labels": []})

        # 3 rate limits then success (4 total attempts = max retries + 1)
        with (
            mock.patch.object(forge._session, "request", side_effect=[
                rate_limit_resp, rate_limit_resp, rate_limit_resp, success_resp,
            ]),
            mock.patch("loom_tools.common.gitea.time.sleep") as mock_sleep,
            mock.patch("loom_tools.common.gitea.logger"),
        ):
            result = forge.get_issue(1)

        assert result is not None
        # Backoff: 1.0, 2.0, 4.0
        sleep_calls = [call[0][0] for call in mock_sleep.call_args_list]
        assert sleep_calls == [1.0, 2.0, 4.0]


# ===========================================================================
# Pagination
# ===========================================================================


class TestPagination:
    """Tests for paginated list methods."""

    def test_list_issues_paginates(self) -> None:
        """list_issues fetches multiple pages until a short page."""
        forge = _make_forge()
        # Page 1: full page of 50, page 2: partial page of 10
        page1 = [
            {"number": i, "state": "open", "title": f"Issue {i}", "html_url": f"u{i}", "labels": []}
            for i in range(1, 51)
        ]
        page2 = [
            {"number": i, "state": "open", "title": f"Issue {i}", "html_url": f"u{i}", "labels": []}
            for i in range(51, 61)
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=page1),
            _mock_response(json_data=page2),
        ]):
            result = forge.list_issues()
        assert len(result) == 60

    def test_list_issues_respects_limit(self) -> None:
        """list_issues stops after limit items."""
        forge = _make_forge()
        page1 = [
            {"number": i, "state": "open", "title": f"Issue {i}", "html_url": f"u{i}", "labels": []}
            for i in range(1, 11)
        ]
        with mock.patch.object(forge._session, "request", return_value=_mock_response(json_data=page1)):
            result = forge.list_issues(limit=5)
        assert len(result) == 5

    def test_list_prs_paginates(self) -> None:
        """list_pull_requests fetches multiple pages."""
        forge = _make_forge()
        page1 = [
            {"number": i, "state": "open", "title": f"PR {i}", "html_url": f"u{i}",
             "labels": [], "head": {"ref": f"branch-{i}"}, "merged": False}
            for i in range(1, 51)
        ]
        page2 = [
            {"number": 51, "state": "open", "title": "PR 51", "html_url": "u51",
             "labels": [], "head": {"ref": "branch-51"}, "merged": False},
        ]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=page1),
            _mock_response(json_data=page2),
        ]):
            result = forge.list_pull_requests()
        assert len(result) == 51

    def test_label_cache_paginates(self) -> None:
        """Label cache fetches all labels across pages."""
        forge = _make_forge()
        page1 = [{"name": f"label-{i}", "id": i} for i in range(1, 51)]
        page2 = [{"name": f"label-{i}", "id": i} for i in range(51, 61)]
        with mock.patch.object(forge._session, "request", side_effect=[
            _mock_response(json_data=page1),
            _mock_response(json_data=page2),
        ]):
            forge._populate_label_cache()
        assert forge._label_cache is not None
        assert len(forge._label_cache) == 60
        assert "label-55" in forge._label_cache


# ===========================================================================
# Factory function
# ===========================================================================


class TestGetForgeFactory:
    """Tests for get_forge() in forge.py."""

    def test_returns_gitea_forge(self) -> None:
        from loom_tools.common.forge import get_forge

        with (
            mock.patch("loom_tools.common.forge.detect_forge") as mock_detect,
            mock.patch(
                "loom_tools.common.gitea.get_forge_config",
                return_value=_GITEA_CONFIG,
            ),
            mock.patch.dict(os.environ, {"GITEA_TOKEN": "tok"}, clear=False),
        ):
            from loom_tools.common.forge import ForgeType
            mock_detect.return_value = ForgeType.GITEA
            forge = get_forge()

        assert forge.forge_type == "gitea"

    def test_returns_github_forge_by_default(self) -> None:
        from loom_tools.common.forge import ForgeType, get_forge

        with mock.patch("loom_tools.common.forge.detect_forge") as mock_detect:
            mock_detect.return_value = ForgeType.GITHUB
            forge = get_forge()

        assert forge.forge_type == "github"
