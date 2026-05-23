"""End-to-end integration tests for GiteaForge against a real Gitea instance.

Requires:
    - Docker running with Gitea container (see tests/integration/docker-compose.yml)
    - Bootstrap completed (see tests/integration/setup-gitea.sh)
    - GITEA_URL and GITEA_TOKEN environment variables set

Priority levels follow the curation in issue #3156:
    P0 - Must pass (blocks Gitea support claim)
    P1 - Important (validates edge cases)
    P2 - Nice to have (batch ops, error handling)
"""

from __future__ import annotations

import os
import time
from typing import Any

import pytest
import requests

from loom_tools.common.forge import (
    ForgeIssue,
    ForgePullRequest,
    ForgeType,
    detect_forge,
)

pytestmark = [
    pytest.mark.integration,
]


# ============================================================================
# P0: Forge Detection
# ============================================================================


class TestForgeDetection:
    """P0: Verify detect_forge() identifies Gitea correctly."""

    def test_detect_forge_via_env_var(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """LOOM_FORGE_TYPE=gitea should return ForgeType.GITEA."""
        monkeypatch.setenv("LOOM_FORGE_TYPE", "gitea")
        assert detect_forge() == ForgeType.GITEA

    def test_forge_type_property(self, gitea_forge: Any) -> None:
        """GiteaForge.forge_type should be 'gitea'."""
        assert gitea_forge.forge_type == "gitea"


# ============================================================================
# P0: Issue CRUD
# ============================================================================


class TestIssueCRUD:
    """P0: Create, read, list, close, and comment on issues."""

    def test_create_issue(self, gitea_forge: Any) -> None:
        """Create an issue and verify returned fields."""
        issue = gitea_forge.create_issue(
            title="Test issue from integration tests",
            body="This is a test issue created by the integration test suite.",
        )
        assert issue is not None
        assert isinstance(issue, ForgeIssue)
        assert issue.number > 0
        assert issue.title == "Test issue from integration tests"
        assert issue.state == "OPEN"

    def test_create_issue_with_labels(self, gitea_forge: Any) -> None:
        """Create an issue with labels attached."""
        issue = gitea_forge.create_issue(
            title="Labeled issue",
            body="Issue with labels.",
            labels=["loom:issue"],
        )
        assert issue is not None
        assert "loom:issue" in issue.labels

    def test_get_issue(self, gitea_forge: Any) -> None:
        """Create then fetch an issue by number."""
        created = gitea_forge.create_issue(
            title="Get-test issue",
            body="For get_issue() testing.",
        )
        assert created is not None

        fetched = gitea_forge.get_issue(created.number)
        assert fetched is not None
        assert fetched.number == created.number
        assert fetched.title == "Get-test issue"
        assert fetched.body == "For get_issue() testing."

    def test_list_issues(self, gitea_forge: Any) -> None:
        """List open issues and verify results include our test issue."""
        created = gitea_forge.create_issue(
            title="List-test issue",
            body="For list_issues() testing.",
            labels=["loom:building"],
        )
        assert created is not None

        issues = gitea_forge.list_issues(labels=["loom:building"], state="open")
        assert len(issues) > 0
        numbers = [i.number for i in issues]
        assert created.number in numbers

    def test_close_issue(self, gitea_forge: Any) -> None:
        """Create and close an issue."""
        issue = gitea_forge.create_issue(
            title="Close-test issue",
            body="Will be closed.",
        )
        assert issue is not None

        result = gitea_forge.close_issue(issue.number)
        assert result is True

        # Verify it's closed
        fetched = gitea_forge.get_issue(issue.number)
        assert fetched is not None
        assert fetched.state == "CLOSED"

    def test_comment_on_issue(self, gitea_forge: Any) -> None:
        """Add a comment to an issue."""
        issue = gitea_forge.create_issue(
            title="Comment-test issue",
            body="Will receive a comment.",
        )
        assert issue is not None

        result = gitea_forge.comment_on_issue(issue.number, "Test comment body")
        assert result is True

    def test_list_issues_state_closed(self, gitea_forge: Any) -> None:
        """P1: List closed issues."""
        # Create and close an issue first
        issue = gitea_forge.create_issue(
            title="Closed-list-test",
            body="For closed listing test.",
        )
        assert issue is not None
        gitea_forge.close_issue(issue.number)

        closed = gitea_forge.list_issues(state="closed")
        assert len(closed) > 0
        assert all(i.state == "CLOSED" for i in closed)


# ============================================================================
# P0: Label Transitions
# ============================================================================


class TestLabelTransitions:
    """P0: Verify add_labels, remove_labels, and transition_labels with
    real Gitea integer ID resolution.
    """

    def test_add_labels(self, gitea_forge: Any) -> None:
        """Add labels to an issue."""
        issue = gitea_forge.create_issue(
            title="Add-labels test",
            body="Test adding labels.",
        )
        assert issue is not None

        result = gitea_forge.add_labels(
            "issue", issue.number, ["loom:issue", "loom:building"],
        )
        assert result is True

        # Verify labels are present
        fetched = gitea_forge.get_issue(issue.number)
        assert fetched is not None
        assert "loom:issue" in fetched.labels
        assert "loom:building" in fetched.labels

    def test_remove_labels(self, gitea_forge: Any) -> None:
        """Remove labels from an issue."""
        issue = gitea_forge.create_issue(
            title="Remove-labels test",
            body="Test removing labels.",
            labels=["loom:issue", "loom:building"],
        )
        assert issue is not None

        result = gitea_forge.remove_labels(
            "issue", issue.number, ["loom:building"],
        )
        assert result is True

        fetched = gitea_forge.get_issue(issue.number)
        assert fetched is not None
        assert "loom:issue" in fetched.labels
        assert "loom:building" not in fetched.labels

    def test_transition_labels(self, gitea_forge: Any) -> None:
        """Transition labels (add + remove atomically)."""
        issue = gitea_forge.create_issue(
            title="Transition-labels test",
            body="Test label transitions.",
            labels=["loom:issue"],
        )
        assert issue is not None

        result = gitea_forge.transition_labels(
            "issue",
            issue.number,
            add=["loom:building"],
            remove=["loom:issue"],
        )
        assert result is True

        fetched = gitea_forge.get_issue(issue.number)
        assert fetched is not None
        assert "loom:building" in fetched.labels
        assert "loom:issue" not in fetched.labels

    def test_label_cache_invalidation(
        self,
        gitea_forge: Any,
        gitea_url: str,
        gitea_token: str,
        gitea_nwo: str,
    ) -> None:
        """P1: Create a label mid-test and verify cache refreshes."""
        # Create a new label directly via API (bypassing forge cache)
        new_label = f"test-dynamic-{int(time.time())}"
        resp = requests.post(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/labels",
            headers={
                "Authorization": f"token {gitea_token}",
                "Content-Type": "application/json",
            },
            json={"name": new_label, "color": "#00ff00"},
            timeout=10,
        )
        resp.raise_for_status()

        try:
            # Force cache to be populated (if not already)
            issue = gitea_forge.create_issue(
                title="Cache-invalidation test",
                body="Testing label cache invalidation.",
            )
            assert issue is not None

            # This should trigger a cache refresh since the label wasn't
            # in the cache when it was first populated
            result = gitea_forge.add_labels(
                "issue", issue.number, [new_label],
            )
            assert result is True

            fetched = gitea_forge.get_issue(issue.number)
            assert fetched is not None
            assert new_label in fetched.labels
        finally:
            # Clean up dynamic label
            requests.delete(
                f"{gitea_url}/api/v1/repos/{gitea_nwo}/labels/{resp.json()['id']}",
                headers={"Authorization": f"token {gitea_token}"},
                timeout=10,
            )


# ============================================================================
# P0: PR Lifecycle
# ============================================================================


class TestPRLifecycle:
    """P0: Full pull request lifecycle — create, get, add labels, merge."""

    def test_create_and_get_pr(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Create a PR from a branch and fetch it."""
        branch_name = f"test-pr-{int(time.time())}"
        create_test_branch(branch_name)

        # Create a file on the branch to make it diverge from main
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-create-pr.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Test PR for get",
            body="Integration test PR.",
            head=branch_name,
            base="main",
        )
        assert pr is not None
        assert isinstance(pr, ForgePullRequest)
        assert pr.number > 0
        assert pr.title == "Test PR for get"
        assert pr.state == "OPEN"
        assert pr.head_branch == branch_name

        # Fetch it back
        fetched = gitea_forge.get_pull_request(pr.number)
        assert fetched is not None
        assert fetched.number == pr.number
        assert fetched.title == "Test PR for get"

    def test_pr_with_labels(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Create a PR with labels."""
        branch_name = f"test-pr-labels-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-labels.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Labeled test PR",
            body="PR with labels.",
            head=branch_name,
            base="main",
            labels=["loom:review-requested"],
        )
        assert pr is not None
        assert "loom:review-requested" in pr.labels

    def test_list_pull_requests(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """List PRs and verify our test PR appears."""
        branch_name = f"test-pr-list-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-list.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="List test PR",
            body="For list_pull_requests() testing.",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        prs = gitea_forge.list_pull_requests(state="open")
        assert len(prs) > 0
        numbers = [p.number for p in prs]
        assert pr.number in numbers

    def test_list_pull_requests_head_filter(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """P1: List PRs with head branch filter (client-side)."""
        branch_name = f"test-pr-head-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-head.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Head-filter test PR",
            body="For head filter testing.",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        prs = gitea_forge.list_pull_requests(head=branch_name, state="open")
        assert len(prs) == 1
        assert prs[0].number == pr.number

    def test_pr_label_transitions(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Add and remove labels on a PR."""
        branch_name = f"test-pr-trans-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-trans.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Label transition PR",
            body="For label transition testing.",
            head=branch_name,
            base="main",
            labels=["loom:review-requested"],
        )
        assert pr is not None

        # Transition: review-requested -> pr
        result = gitea_forge.transition_labels(
            "pr",
            pr.number,
            add=["loom:pr"],
            remove=["loom:review-requested"],
        )
        assert result is True

        fetched = gitea_forge.get_pull_request(pr.number)
        assert fetched is not None
        assert "loom:pr" in fetched.labels
        assert "loom:review-requested" not in fetched.labels

    def test_merge_pull_request_squash(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Create and merge a PR using squash method."""
        branch_name = f"test-pr-merge-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-merge.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Merge test PR",
            body="Will be squash-merged.",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        result = gitea_forge.merge_pull_request(pr.number, method="squash")
        assert result is True

        # Verify merged state
        fetched = gitea_forge.get_pull_request(pr.number)
        assert fetched is not None
        assert fetched.state == "MERGED"

    def test_close_pull_request_with_comment(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """P1: Close a PR with a comment."""
        branch_name = f"test-pr-close-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-close.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Close test PR",
            body="Will be closed.",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        result = gitea_forge.close_pull_request(
            pr.number, comment="Closing: not needed.",
        )
        assert result is True

        fetched = gitea_forge.get_pull_request(pr.number)
        assert fetched is not None
        assert fetched.state == "CLOSED"

    def test_comment_on_pull_request(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Add a comment to a PR."""
        branch_name = f"test-pr-comment-{int(time.time())}"
        create_test_branch(branch_name)
        self._commit_file_on_branch(
            gitea_forge, branch_name, "test-pr-comment.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Comment test PR",
            body="Will receive a comment.",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        result = gitea_forge.comment_on_pull_request(pr.number, "Test comment")
        assert result is True

    @staticmethod
    def _commit_file_on_branch(
        gitea_forge: Any,
        branch_name: str,
        filename: str,
    ) -> None:
        """Create a file on a branch via the Gitea API to make it diverge."""
        import base64

        nwo = gitea_forge.get_repo_nwo()
        url = gitea_forge._api_url(f"repos/{nwo}/contents/{filename}")
        content = base64.b64encode(
            f"Test file for {branch_name}\n".encode(),
        ).decode()
        resp = gitea_forge._session.post(
            url,
            json={
                "content": content,
                "message": f"Add {filename} for testing",
                "branch": branch_name,
            },
            timeout=10,
        )
        resp.raise_for_status()


# ============================================================================
# P0: find_pull_request_for_issue
# ============================================================================


class TestFindPRForIssue:
    """P0: Verify both branch-name and body-search paths."""

    def test_find_by_branch_name(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Find PR by branch naming convention (feature/issue-N)."""
        issue = gitea_forge.create_issue(
            title="Find-PR-branch test",
            body="Issue for PR discovery test.",
        )
        assert issue is not None

        branch_name = f"feature/issue-{issue.number}"
        create_test_branch(branch_name)
        TestPRLifecycle._commit_file_on_branch(
            gitea_forge, branch_name, f"find-branch-{issue.number}.txt",
        )

        pr = gitea_forge.create_pull_request(
            title=f"PR for issue #{issue.number}",
            body=f"Closes #{issue.number}",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        found = gitea_forge.find_pull_request_for_issue(issue.number)
        assert found == pr.number

    def test_find_by_body_search(
        self, gitea_forge: Any, create_test_branch: Any,
    ) -> None:
        """Find PR by body content fallback (non-standard branch name)."""
        issue = gitea_forge.create_issue(
            title="Find-PR-body test",
            body="Issue for body-search PR discovery.",
        )
        assert issue is not None

        # Use a non-standard branch name so branch matching fails
        branch_name = f"fix/custom-{int(time.time())}"
        create_test_branch(branch_name)
        TestPRLifecycle._commit_file_on_branch(
            gitea_forge, branch_name, f"find-body-{issue.number}.txt",
        )

        pr = gitea_forge.create_pull_request(
            title="Custom branch PR",
            body=f"Closes #{issue.number}",
            head=branch_name,
            base="main",
        )
        assert pr is not None

        found = gitea_forge.find_pull_request_for_issue(issue.number)
        assert found == pr.number


# ============================================================================
# P0: Repository Metadata
# ============================================================================


class TestRepoMetadata:
    """P0: Verify repository metadata methods."""

    def test_get_repo_nwo(self, gitea_forge: Any, gitea_nwo: str) -> None:
        """get_repo_nwo() should return the correct owner/repo."""
        nwo = gitea_forge.get_repo_nwo()
        assert nwo == gitea_nwo

    def test_get_repo_default_branch(self, gitea_forge: Any) -> None:
        """get_repo_default_branch() should return 'main'."""
        branch = gitea_forge.get_repo_default_branch()
        assert branch == "main"


# ============================================================================
# P1: CI Status
# ============================================================================


class TestCIStatus:
    """P1: Verify CI status reading from commit status API."""

    def test_ci_status_no_statuses(self, gitea_forge: Any) -> None:
        """When no CI statuses exist, should return unknown."""
        status = gitea_forge.get_default_branch_ci_status()
        # No CI is configured in the test repo, so "unknown" or "passing"
        # (depending on whether empty list counts as no statuses)
        assert status.status in ("unknown", "passing")

    def test_ci_status_with_success(
        self,
        gitea_forge: Any,
        gitea_url: str,
        gitea_token: str,
        gitea_nwo: str,
    ) -> None:
        """Create a success commit status and verify it's read correctly."""
        # Get the latest commit SHA on main
        resp = requests.get(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/branches/main",
            headers={"Authorization": f"token {gitea_token}"},
            timeout=10,
        )
        resp.raise_for_status()
        sha = resp.json()["commit"]["id"]

        # Create a success status
        requests.post(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/statuses/{sha}",
            headers={
                "Authorization": f"token {gitea_token}",
                "Content-Type": "application/json",
            },
            json={
                "state": "success",
                "context": "integration-test/ci",
                "description": "All tests passed",
            },
            timeout=10,
        ).raise_for_status()

        # Clear cached default branch so it re-fetches
        gitea_forge._default_branch_cache = None

        status = gitea_forge.get_default_branch_ci_status()
        assert status.status == "passing"

    def test_ci_status_with_failure(
        self,
        gitea_forge: Any,
        gitea_url: str,
        gitea_token: str,
        gitea_nwo: str,
    ) -> None:
        """Create a failure commit status and verify it's detected."""
        resp = requests.get(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/branches/main",
            headers={"Authorization": f"token {gitea_token}"},
            timeout=10,
        )
        resp.raise_for_status()
        sha = resp.json()["commit"]["id"]

        # Create a failure status
        requests.post(
            f"{gitea_url}/api/v1/repos/{gitea_nwo}/statuses/{sha}",
            headers={
                "Authorization": f"token {gitea_token}",
                "Content-Type": "application/json",
            },
            json={
                "state": "failure",
                "context": "integration-test/failing-check",
                "description": "Tests failed",
            },
            timeout=10,
        ).raise_for_status()

        gitea_forge._default_branch_cache = None

        status = gitea_forge.get_default_branch_ci_status()
        assert status.status == "failing"
        assert "integration-test/failing-check" in status.failed_runs


# ============================================================================
# P2: Batch Operations
# ============================================================================


class TestBatchOperations:
    """P2: Verify batch issue fetching."""

    def test_get_issues_batch(self, gitea_forge: Any) -> None:
        """Fetch multiple issues concurrently."""
        # Create a few issues
        issues = []
        for i in range(3):
            issue = gitea_forge.create_issue(
                title=f"Batch test issue {i}",
                body=f"Batch issue {i}",
            )
            assert issue is not None
            issues.append(issue)

        numbers = [i.number for i in issues]
        results = gitea_forge.get_issues_batch(numbers)

        assert len(results) == 3
        for num in numbers:
            assert num in results
            assert results[num] is not None
            assert results[num].number == num


# ============================================================================
# P2: NWO Parsing
# ============================================================================


class TestNWOParsing:
    """P2: Verify owner/repo parsing from various remote URL formats."""

    def test_parse_https_url(self) -> None:
        """Parse NWO from HTTPS remote URL."""
        from loom_tools.common.gitea import GiteaForge

        assert GiteaForge._parse_nwo("https://gitea.example.com/owner/repo.git") == "owner/repo"
        assert GiteaForge._parse_nwo("https://gitea.example.com/owner/repo") == "owner/repo"

    def test_parse_ssh_url(self) -> None:
        """Parse NWO from SSH remote URL."""
        from loom_tools.common.gitea import GiteaForge

        assert GiteaForge._parse_nwo("git@gitea.example.com:owner/repo.git") == "owner/repo"
        assert GiteaForge._parse_nwo("git@gitea.example.com:owner/repo") == "owner/repo"
