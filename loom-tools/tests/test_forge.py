"""Tests for the ForgeClient protocol and forge-neutral data types."""

from __future__ import annotations

from typing import Any, Sequence

from loom_tools.common.forge import (
    EntityType,
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgeLabel,
    ForgePullRequest,
)


# ---------------------------------------------------------------------------
# Dataclass construction tests
# ---------------------------------------------------------------------------


class TestForgeIssue:
    """Tests for ForgeIssue dataclass."""

    def test_minimal_construction(self) -> None:
        issue = ForgeIssue(number=42, state="OPEN", title="Test", url="https://example.com/42")
        assert issue.number == 42
        assert issue.state == "OPEN"
        assert issue.title == "Test"
        assert issue.url == "https://example.com/42"
        assert issue.labels == []
        assert issue.body is None

    def test_full_construction(self) -> None:
        issue = ForgeIssue(
            number=1,
            state="CLOSED",
            title="Done",
            url="https://example.com/1",
            labels=["bug", "urgent"],
            body="Some description",
        )
        assert issue.labels == ["bug", "urgent"]
        assert issue.body == "Some description"
        assert issue.state == "CLOSED"

    def test_labels_default_is_independent(self) -> None:
        """Each instance gets its own labels list (no shared mutable default)."""
        a = ForgeIssue(number=1, state="OPEN", title="A", url="u")
        b = ForgeIssue(number=2, state="OPEN", title="B", url="u")
        a.labels.append("x")
        assert b.labels == []


class TestForgePullRequest:
    """Tests for ForgePullRequest dataclass."""

    def test_minimal_construction(self) -> None:
        pr = ForgePullRequest(number=10, state="OPEN", title="PR", url="https://example.com/pr/10")
        assert pr.number == 10
        assert pr.state == "OPEN"
        assert pr.labels == []
        assert pr.head_branch is None
        assert pr.body is None
        assert pr.closing_issues == []

    def test_full_construction(self) -> None:
        pr = ForgePullRequest(
            number=99,
            state="MERGED",
            title="Big change",
            url="https://example.com/pr/99",
            labels=["loom:pr"],
            head_branch="feature/issue-42",
            body="Closes #42",
            closing_issues=[42],
        )
        assert pr.state == "MERGED"
        assert pr.head_branch == "feature/issue-42"
        assert pr.closing_issues == [42]

    def test_closing_issues_default_is_independent(self) -> None:
        a = ForgePullRequest(number=1, state="OPEN", title="A", url="u")
        b = ForgePullRequest(number=2, state="OPEN", title="B", url="u")
        a.closing_issues.append(42)
        assert b.closing_issues == []


class TestForgeLabel:
    """Tests for ForgeLabel dataclass."""

    def test_minimal_construction(self) -> None:
        label = ForgeLabel(name="bug")
        assert label.name == "bug"
        assert label.color is None
        assert label.description is None

    def test_full_construction(self) -> None:
        label = ForgeLabel(name="loom:pr", color="0e8a16", description="Ready to merge")
        assert label.color == "0e8a16"
        assert label.description == "Ready to merge"


class TestForgeCIStatus:
    """Tests for ForgeCIStatus dataclass."""

    def test_minimal_construction(self) -> None:
        ci = ForgeCIStatus(status="passing")
        assert ci.status == "passing"
        assert ci.failed_runs == []
        assert ci.total_runs == 0
        assert ci.message == ""

    def test_full_construction(self) -> None:
        ci = ForgeCIStatus(
            status="failing",
            failed_runs=["lint", "test"],
            total_runs=3,
            message="CI failing: 2 workflow(s) failed",
        )
        assert ci.failed_runs == ["lint", "test"]
        assert ci.total_runs == 3

    def test_failed_runs_default_is_independent(self) -> None:
        a = ForgeCIStatus(status="passing")
        b = ForgeCIStatus(status="passing")
        a.failed_runs.append("build")
        assert b.failed_runs == []


# ---------------------------------------------------------------------------
# Protocol compliance tests
# ---------------------------------------------------------------------------


class MockForgeClient:
    """Minimal implementation of ForgeClient for protocol compliance testing.

    Every method returns a stub value of the correct type. This class is
    *not* imported from production code -- its purpose is to verify that
    a concrete class can satisfy the protocol structurally.
    """

    @property
    def forge_type(self) -> str:
        return "mock"

    # --- Issue operations ---

    def get_issue(self, number: int) -> ForgeIssue | None:
        return ForgeIssue(number=number, state="OPEN", title="mock", url="u")

    def list_issues(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        limit: int | None = None,
    ) -> list[ForgeIssue]:
        return []

    def create_issue(
        self,
        title: str,
        body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        return ForgeIssue(number=1, state="OPEN", title=title, url="u")

    def close_issue(self, number: int) -> bool:
        return True

    def comment_on_issue(self, number: int, body: str) -> bool:
        return True

    # --- PR operations ---

    def get_pull_request(self, number: int) -> ForgePullRequest | None:
        return ForgePullRequest(number=number, state="OPEN", title="mock", url="u")

    def list_pull_requests(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        head: str | None = None,
        search: str | None = None,
        limit: int | None = None,
    ) -> list[ForgePullRequest]:
        return []

    def create_pull_request(
        self,
        title: str,
        body: str,
        head: str,
        base: str | None = None,
        labels: Sequence[str] | None = None,
    ) -> ForgePullRequest | None:
        return ForgePullRequest(number=1, state="OPEN", title=title, url="u")

    def close_pull_request(
        self, number: int, comment: str | None = None,
    ) -> bool:
        return True

    def merge_pull_request(
        self, number: int, method: str = "squash",
    ) -> bool:
        return True

    def comment_on_pull_request(self, number: int, body: str) -> bool:
        return True

    def get_pull_request_reviews(
        self, number: int,
    ) -> list[dict[str, Any]]:
        return []

    # --- Label operations ---

    def add_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        return True

    def remove_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        return True

    def transition_labels(
        self,
        entity_type: EntityType,
        number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        return True

    # --- CI status ---

    def get_default_branch_ci_status(self) -> ForgeCIStatus:
        return ForgeCIStatus(status="passing")

    # --- Repository metadata ---

    def get_repo_nwo(self) -> str | None:
        return "owner/repo"

    def get_repo_default_branch(self) -> str | None:
        return "main"

    # --- Batch operations ---

    def get_issues_batch(
        self, numbers: Sequence[int],
    ) -> dict[int, ForgeIssue | None]:
        return {n: self.get_issue(n) for n in numbers}

    def find_pull_request_for_issue(
        self, issue: int, state: str = "open",
    ) -> int | None:
        return None


class TestProtocolCompliance:
    """Verify that a concrete implementation satisfies the ForgeClient protocol."""

    def test_isinstance_check(self) -> None:
        """MockForgeClient satisfies the runtime_checkable protocol."""
        client = MockForgeClient()
        assert isinstance(client, ForgeClient)

    def test_forge_type_property(self) -> None:
        client = MockForgeClient()
        assert client.forge_type == "mock"

    def test_get_issue(self) -> None:
        client = MockForgeClient()
        issue = client.get_issue(42)
        assert issue is not None
        assert isinstance(issue, ForgeIssue)
        assert issue.number == 42

    def test_list_issues(self) -> None:
        client = MockForgeClient()
        result = client.list_issues(labels=["bug"], state="open", limit=10)
        assert isinstance(result, list)

    def test_create_issue(self) -> None:
        client = MockForgeClient()
        issue = client.create_issue("Title", "Body", labels=["loom:issue"])
        assert issue is not None
        assert issue.title == "Title"

    def test_close_issue(self) -> None:
        client = MockForgeClient()
        assert client.close_issue(1) is True

    def test_comment_on_issue(self) -> None:
        client = MockForgeClient()
        assert client.comment_on_issue(1, "hello") is True

    def test_get_pull_request(self) -> None:
        client = MockForgeClient()
        pr = client.get_pull_request(10)
        assert pr is not None
        assert isinstance(pr, ForgePullRequest)

    def test_list_pull_requests(self) -> None:
        client = MockForgeClient()
        result = client.list_pull_requests(
            labels=["loom:pr"], state="open", head="feature/x", search="test", limit=5,
        )
        assert isinstance(result, list)

    def test_create_pull_request(self) -> None:
        client = MockForgeClient()
        pr = client.create_pull_request("PR Title", "body", "feature/x", base="main", labels=["loom:review-requested"])
        assert pr is not None
        assert pr.title == "PR Title"

    def test_close_pull_request(self) -> None:
        client = MockForgeClient()
        assert client.close_pull_request(1, comment="closing") is True

    def test_merge_pull_request(self) -> None:
        client = MockForgeClient()
        assert client.merge_pull_request(1, method="squash") is True

    def test_comment_on_pull_request(self) -> None:
        client = MockForgeClient()
        assert client.comment_on_pull_request(1, "LGTM") is True

    def test_get_pull_request_reviews(self) -> None:
        client = MockForgeClient()
        reviews = client.get_pull_request_reviews(1)
        assert isinstance(reviews, list)

    def test_add_labels(self) -> None:
        client = MockForgeClient()
        assert client.add_labels("issue", 1, ["bug"]) is True

    def test_remove_labels(self) -> None:
        client = MockForgeClient()
        assert client.remove_labels("pr", 1, ["draft"]) is True

    def test_transition_labels(self) -> None:
        client = MockForgeClient()
        assert client.transition_labels("issue", 1, add=["loom:building"], remove=["loom:issue"]) is True

    def test_get_default_branch_ci_status(self) -> None:
        client = MockForgeClient()
        ci = client.get_default_branch_ci_status()
        assert isinstance(ci, ForgeCIStatus)
        assert ci.status == "passing"

    def test_get_repo_nwo(self) -> None:
        client = MockForgeClient()
        assert client.get_repo_nwo() == "owner/repo"

    def test_get_repo_default_branch(self) -> None:
        client = MockForgeClient()
        assert client.get_repo_default_branch() == "main"

    def test_get_issues_batch(self) -> None:
        client = MockForgeClient()
        result = client.get_issues_batch([1, 2, 3])
        assert isinstance(result, dict)
        assert set(result.keys()) == {1, 2, 3}
        for v in result.values():
            assert isinstance(v, ForgeIssue)

    def test_find_pull_request_for_issue(self) -> None:
        client = MockForgeClient()
        result = client.find_pull_request_for_issue(42, state="open")
        assert result is None


class TestProtocolNonCompliance:
    """Verify that incomplete implementations are NOT protocol-compliant."""

    def test_empty_class_fails(self) -> None:
        """A class with no methods does not satisfy ForgeClient."""

        class Empty:
            pass

        assert not isinstance(Empty(), ForgeClient)

    def test_missing_forge_type_fails(self) -> None:
        """A class missing the forge_type property fails the check.

        Note: runtime_checkable only checks method/property existence,
        not signatures. This tests the most basic structural requirement.
        """

        class MissingForgeType:
            def get_issue(self, number: int) -> ForgeIssue | None:
                return None

        assert not isinstance(MissingForgeType(), ForgeClient)

    def test_partial_implementation_fails(self) -> None:
        """A class with only some methods does not satisfy ForgeClient."""

        class Partial:
            @property
            def forge_type(self) -> str:
                return "partial"

            def get_issue(self, number: int) -> ForgeIssue | None:
                return None

        assert not isinstance(Partial(), ForgeClient)


# ---------------------------------------------------------------------------
# Edge case tests
# ---------------------------------------------------------------------------


class TestEdgeCases:
    """Test edge cases for data types."""

    def test_forge_issue_empty_labels(self) -> None:
        issue = ForgeIssue(number=1, state="OPEN", title="T", url="u", labels=[])
        assert issue.labels == []

    def test_forge_pr_multiple_closing_issues(self) -> None:
        pr = ForgePullRequest(
            number=1, state="MERGED", title="T", url="u",
            closing_issues=[10, 20, 30],
        )
        assert len(pr.closing_issues) == 3

    def test_forge_ci_status_unknown(self) -> None:
        ci = ForgeCIStatus(status="unknown", message="No CI configured")
        assert ci.status == "unknown"
        assert ci.total_runs == 0

    def test_entity_type_literal(self) -> None:
        """EntityType is a Literal type accepting 'issue' and 'pr'."""
        val: EntityType = "issue"
        assert val == "issue"
        val = "pr"
        assert val == "pr"
