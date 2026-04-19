"""Forge-agnostic protocol for issue tracker and code forge operations.

Defines the ``ForgeClient`` protocol that abstracts all forge operations
(issues, PRs, labels, CI, comments, etc.) behind a single interface.
Both ``GitHubForge`` and ``GiteaForge`` will implement this protocol.

This module is the foundation of the forge-agnostic abstraction layer.
It defines only the interface and data types -- no implementation.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Literal, Protocol, Sequence, runtime_checkable


EntityType = Literal["issue", "pr"]


# ---------------------------------------------------------------------------
# Forge-neutral data types
# ---------------------------------------------------------------------------


@dataclass
class ForgeIssue:
    """Normalized representation of an issue from any forge."""

    number: int
    state: str  # "OPEN", "CLOSED"
    title: str
    url: str
    labels: list[str] = field(default_factory=list)
    body: str | None = None


@dataclass
class ForgePullRequest:
    """Normalized representation of a pull request from any forge."""

    number: int
    state: str  # "OPEN", "CLOSED", "MERGED"
    title: str
    url: str
    labels: list[str] = field(default_factory=list)
    head_branch: str | None = None
    body: str | None = None
    closing_issues: list[int] = field(default_factory=list)


@dataclass
class ForgeLabel:
    """Normalized representation of a label from any forge."""

    name: str
    color: str | None = None
    description: str | None = None


@dataclass
class ForgeCIStatus:
    """CI status for the default branch."""

    status: str  # "passing", "failing", "unknown"
    failed_runs: list[str] = field(default_factory=list)
    total_runs: int = 0
    message: str = ""


# ---------------------------------------------------------------------------
# ForgeClient protocol
# ---------------------------------------------------------------------------


@runtime_checkable
class ForgeClient(Protocol):
    """Protocol defining the contract for forge operations.

    Any class that implements all methods with matching signatures
    satisfies this protocol via structural subtyping (no inheritance
    required). Use ``@runtime_checkable`` for ``isinstance()`` checks.

    Covers all forge operations currently used across the Loom codebase:

    - Issue CRUD (get, list, create, close, comment)
    - PR CRUD (get, list, create, close, merge, comment, reviews)
    - Label management (add, remove, transition)
    - CI status
    - Repository metadata
    - Batch operations and PR-issue linking
    """

    @property
    def forge_type(self) -> str:
        """Identifier for the forge backend (e.g. ``"github"``, ``"gitea"``)."""
        ...

    # --- Issue operations ---

    def get_issue(self, number: int) -> ForgeIssue | None:
        """Fetch a single issue by number.

        Returns ``None`` if the issue does not exist or cannot be fetched.
        """
        ...

    def list_issues(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        limit: int | None = None,
    ) -> list[ForgeIssue]:
        """List issues matching the given filters.

        Parameters
        ----------
        labels:
            Filter to issues with all of these labels.
        state:
            Issue state filter (``"open"``, ``"closed"``, ``"all"``).
        limit:
            Maximum number of results.
        """
        ...

    def create_issue(
        self,
        title: str,
        body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        """Create a new issue.

        Returns the created issue, or ``None`` on failure.
        """
        ...

    def close_issue(self, number: int) -> bool:
        """Close an issue. Returns ``True`` on success."""
        ...

    def comment_on_issue(self, number: int, body: str) -> bool:
        """Add a comment to an issue. Returns ``True`` on success."""
        ...

    # --- Pull request operations ---

    def get_pull_request(self, number: int) -> ForgePullRequest | None:
        """Fetch a single pull request by number.

        Returns ``None`` if the PR does not exist or cannot be fetched.
        """
        ...

    def list_pull_requests(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        head: str | None = None,
        search: str | None = None,
        limit: int | None = None,
    ) -> list[ForgePullRequest]:
        """List pull requests matching the given filters.

        Parameters
        ----------
        labels:
            Filter to PRs with all of these labels.
        state:
            PR state filter (``"open"``, ``"closed"``, ``"merged"``, ``"all"``).
        head:
            Filter PRs by head branch name.
        search:
            Free-text search query.
        limit:
            Maximum number of results.
        """
        ...

    def create_pull_request(
        self,
        title: str,
        body: str,
        head: str,
        base: str | None = None,
        labels: Sequence[str] | None = None,
    ) -> ForgePullRequest | None:
        """Create a new pull request.

        Parameters
        ----------
        title:
            PR title.
        body:
            PR body / description.
        head:
            Source branch name.
        base:
            Target branch name (defaults to the repo default branch).
        labels:
            Labels to apply to the new PR.

        Returns the created PR, or ``None`` on failure.
        """
        ...

    def close_pull_request(
        self, number: int, comment: str | None = None,
    ) -> bool:
        """Close a pull request, optionally leaving a comment.

        Returns ``True`` on success.
        """
        ...

    def merge_pull_request(
        self, number: int, method: str = "squash",
    ) -> bool:
        """Merge a pull request.

        Parameters
        ----------
        number:
            PR number.
        method:
            Merge method (``"squash"``, ``"merge"``, ``"rebase"``).

        Returns ``True`` on success.
        """
        ...

    def comment_on_pull_request(self, number: int, body: str) -> bool:
        """Add a comment to a pull request. Returns ``True`` on success."""
        ...

    def get_pull_request_reviews(
        self, number: int,
    ) -> list[dict[str, Any]]:
        """Fetch reviews for a pull request.

        Returns a list of review dicts. The exact shape is
        forge-dependent but must include at least ``state``
        (e.g. ``"APPROVED"``, ``"CHANGES_REQUESTED"``).
        """
        ...

    # --- Label operations ---

    def add_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Add labels to an issue or PR. Returns ``True`` on success."""
        ...

    def remove_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Remove labels from an issue or PR. Returns ``True`` on success."""
        ...

    def transition_labels(
        self,
        entity_type: EntityType,
        number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        """Atomically add and remove labels on an issue or PR.

        This combines ``add_labels`` and ``remove_labels`` into a single
        logical operation. Implementations may perform this in one API
        call or two, depending on forge capabilities.

        Returns ``True`` if all label changes succeeded.
        """
        ...

    # --- CI status ---

    def get_default_branch_ci_status(self) -> ForgeCIStatus:
        """Get CI status for the latest runs on the default branch."""
        ...

    # --- Repository metadata ---

    def get_repo_nwo(self) -> str | None:
        """Return the ``owner/repo`` identifier for the current repository.

        Returns ``None`` if it cannot be determined.
        """
        ...

    def get_repo_default_branch(self) -> str | None:
        """Return the name of the repository's default branch.

        Returns ``None`` if it cannot be determined.
        """
        ...

    # --- Batch operations ---

    def get_issues_batch(
        self, numbers: Sequence[int],
    ) -> dict[int, ForgeIssue | None]:
        """Fetch multiple issues by number in a single batch.

        Returns a mapping from issue number to ``ForgeIssue`` (or ``None``
        if that issue could not be fetched). Implementations may use
        concurrent requests or batch API calls.
        """
        ...

    def find_pull_request_for_issue(
        self, issue: int, state: str = "open",
    ) -> int | None:
        """Find a pull request associated with a given issue.

        Searches by branch naming convention (``feature/issue-N``) and/or
        closing references in PR bodies.

        Returns the PR number, or ``None`` if no matching PR is found.
        """
        ...
