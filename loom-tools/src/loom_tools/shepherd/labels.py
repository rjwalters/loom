"""Label caching and manipulation for issues and PRs.

Uses the ForgeClient abstraction for all forge operations, enabling
support for both GitHub and Gitea backends.
"""

from __future__ import annotations

from pathlib import Path
from typing import Literal

from loom_tools.common.forge import ForgeClient, get_forge

# Entity type for generic label operations
EntityType = Literal["issue", "pr"]

# Label exclusion groups: only one label from each group should be present
# on an entity at a time. Used by transition_labels(enforce_exclusion=True)
# and by the daemon health check to detect contradictory states.
LABEL_EXCLUSION_GROUPS: list[frozenset[str]] = [
    frozenset({"loom:pr", "loom:changes-requested", "loom:review-requested"}),
    frozenset({"loom:issue", "loom:building", "loom:blocked"}),
]


def get_exclusion_conflicts(labels: set[str]) -> list[dict[str, object]]:
    """Check a set of labels for exclusion group violations.

    Returns a list of dicts with 'group' (the frozenset) and
    'conflicting' (the set of labels that conflict) for each violation.
    Returns an empty list if no conflicts.
    """
    conflicts: list[dict[str, object]] = []
    for group in LABEL_EXCLUSION_GROUPS:
        matching = labels & group
        if len(matching) > 1:
            conflicts.append({"group": group, "conflicting": matching})
    return conflicts


def _get_forge_client(repo_root: Path | None = None) -> ForgeClient:
    """Get a ForgeClient instance for the given repo root."""
    return get_forge(repo_root)


class LabelCache:
    """Cache for issue/PR labels with invalidation.

    Reduces forge API calls by caching label lookups.
    Call invalidate() after any label mutation (add/remove).
    """

    def __init__(
        self,
        repo_root: Path | None = None,
        forge: ForgeClient | None = None,
    ) -> None:
        self._labels: dict[tuple[EntityType, int], set[str]] = {}
        self._repo_root = repo_root
        self._forge = forge

    def _get_forge(self) -> ForgeClient:
        """Get or lazily create the ForgeClient instance."""
        if self._forge is None:
            self._forge = _get_forge_client(self._repo_root)
        return self._forge

    def _fetch_labels(self, entity_type: EntityType, number: int) -> set[str]:
        """Fetch labels for an issue or PR via ForgeClient."""
        forge = self._get_forge()
        if entity_type == "issue":
            entity = forge.get_issue(number)
        else:
            entity = forge.get_pull_request(number)
        if entity is None:
            return set()
        return set(entity.labels)

    def get_labels(
        self, entity_type: EntityType, number: int, *, refresh: bool = False
    ) -> set[str]:
        """Get labels for an entity, using cache if available.

        Args:
            entity_type: "issue" or "pr"
            number: Issue or PR number
            refresh: Force refresh from API (default False)

        Returns:
            Set of label names
        """
        key = (entity_type, number)
        if refresh or key not in self._labels:
            self._labels[key] = self._fetch_labels(entity_type, number)
        return self._labels[key]

    def has_label(self, entity_type: EntityType, number: int, label: str) -> bool:
        """Check if an entity has a specific label."""
        return label in self.get_labels(entity_type, number)

    def set_labels(
        self, entity_type: EntityType, number: int, labels: set[str]
    ) -> None:
        """Pre-populate cache with labels (e.g., from initial metadata fetch)."""
        self._labels[(entity_type, number)] = labels

    def invalidate_entity(
        self, entity_type: EntityType, number: int | None = None
    ) -> None:
        """Invalidate cached labels for entities of a given type.

        Args:
            entity_type: "issue" or "pr"
            number: Specific entity to invalidate, or None for all of that type
        """
        if number is not None:
            self._labels.pop((entity_type, number), None)
        else:
            # Remove all entries of this type
            keys_to_remove = [k for k in self._labels if k[0] == entity_type]
            for key in keys_to_remove:
                del self._labels[key]

    def invalidate(self) -> None:
        """Invalidate all cached labels."""
        self._labels.clear()

    # -------------------------------------------------------------------------
    # Backward-compatible convenience methods
    # -------------------------------------------------------------------------

    def _fetch_issue_labels(self, issue: int) -> set[str]:
        """Fetch labels for an issue from the forge.

        Deprecated: Use _fetch_labels("issue", issue) instead.
        """
        return self._fetch_labels("issue", issue)

    def _fetch_pr_labels(self, pr: int) -> set[str]:
        """Fetch labels for a PR from the forge.

        Deprecated: Use _fetch_labels("pr", pr) instead.
        """
        return self._fetch_labels("pr", pr)

    def get_issue_labels(self, issue: int, *, refresh: bool = False) -> set[str]:
        """Get labels for an issue, using cache if available.

        Args:
            issue: Issue number
            refresh: Force refresh from API (default False)

        Returns:
            Set of label names
        """
        return self.get_labels("issue", issue, refresh=refresh)

    def get_pr_labels(self, pr: int, *, refresh: bool = False) -> set[str]:
        """Get labels for a PR, using cache if available.

        Args:
            pr: PR number
            refresh: Force refresh from API (default False)

        Returns:
            Set of label names
        """
        return self.get_labels("pr", pr, refresh=refresh)

    def has_issue_label(self, issue: int, label: str) -> bool:
        """Check if an issue has a specific label."""
        return self.has_label("issue", issue, label)

    def has_pr_label(self, pr: int, label: str) -> bool:
        """Check if a PR has a specific label."""
        return self.has_label("pr", pr, label)

    def set_issue_labels(self, issue: int, labels: set[str]) -> None:
        """Pre-populate cache with labels (e.g., from initial metadata fetch)."""
        self.set_labels("issue", issue, labels)

    def invalidate_issue(self, issue: int | None = None) -> None:
        """Invalidate cached issue labels.

        Args:
            issue: Specific issue to invalidate, or None for all
        """
        self.invalidate_entity("issue", issue)

    def invalidate_pr(self, pr: int | None = None) -> None:
        """Invalidate cached PR labels.

        Args:
            pr: Specific PR to invalidate, or None for all
        """
        self.invalidate_entity("pr", pr)

    # -------------------------------------------------------------------------
    # Internal backward compatibility for tests
    # -------------------------------------------------------------------------

    @property
    def _issue_labels(self) -> dict[int, set[str]]:
        """Backward-compatible view of issue labels for tests.

        This property provides a dict-like interface mapping issue numbers
        to their labels, built from the unified cache.
        """
        return _EntityLabelView(self, "issue")

    @property
    def _pr_labels(self) -> dict[int, set[str]]:
        """Backward-compatible view of PR labels for tests.

        This property provides a dict-like interface mapping PR numbers
        to their labels, built from the unified cache.
        """
        return _EntityLabelView(self, "pr")


class _EntityLabelView:
    """Dict-like view into LabelCache for backward compatibility.

    This class provides a dict-like interface (getitem, setitem, contains, pop, clear)
    that maps entity numbers to labels, filtering by entity type.
    """

    def __init__(self, cache: LabelCache, entity_type: EntityType) -> None:
        self._cache = cache
        self._entity_type = entity_type

    def __getitem__(self, number: int) -> set[str]:
        key = (self._entity_type, number)
        return self._cache._labels[key]

    def __setitem__(self, number: int, labels: set[str]) -> None:
        key = (self._entity_type, number)
        self._cache._labels[key] = labels

    def __contains__(self, number: int) -> bool:
        key = (self._entity_type, number)
        return key in self._cache._labels

    def __eq__(self, other: object) -> bool:
        if isinstance(other, dict):
            return self._to_dict() == other
        return NotImplemented

    def _to_dict(self) -> dict[int, set[str]]:
        """Convert to a regular dict."""
        return {
            num: labels
            for (etype, num), labels in self._cache._labels.items()
            if etype == self._entity_type
        }

    def pop(self, number: int, *args: set[str]) -> set[str]:
        key = (self._entity_type, number)
        if args:
            return self._cache._labels.pop(key, args[0])
        return self._cache._labels.pop(key)

    def clear(self) -> None:
        keys_to_remove = [k for k in self._cache._labels if k[0] == self._entity_type]
        for key in keys_to_remove:
            del self._cache._labels[key]


def add_issue_label(issue: int, label: str, repo_root: Path | None = None) -> bool:
    """Add a label to an issue.

    Returns True if successful.
    """
    return add_label("issue", issue, label, repo_root)


def remove_issue_label(issue: int, label: str, repo_root: Path | None = None) -> bool:
    """Remove a label from an issue.

    Returns True if successful (or label didn't exist).
    """
    return remove_label("issue", issue, label, repo_root)


def add_pr_label(pr: int, label: str, repo_root: Path | None = None) -> bool:
    """Add a label to a PR.

    Returns True if successful.
    """
    return add_label("pr", pr, label, repo_root)


def remove_pr_label(pr: int, label: str, repo_root: Path | None = None) -> bool:
    """Remove a label from a PR.

    Returns True if successful (or label didn't exist).
    """
    return remove_label("pr", pr, label, repo_root)


def add_label(
    entity_type: EntityType, number: int, label: str, repo_root: Path | None = None
) -> bool:
    """Add a label to an issue or PR.

    Args:
        entity_type: "issue" or "pr"
        number: Issue or PR number
        label: Label name to add
        repo_root: Repository root path (optional)

    Returns:
        True if successful
    """
    forge = _get_forge_client(repo_root)
    return forge.add_labels(entity_type, number, [label])


def remove_label(
    entity_type: EntityType, number: int, label: str, repo_root: Path | None = None
) -> bool:
    """Remove a label from an issue or PR.

    Args:
        entity_type: "issue" or "pr"
        number: Issue or PR number
        label: Label name to remove
        repo_root: Repository root path (optional)

    Returns:
        True if successful (or label didn't exist)
    """
    forge = _get_forge_client(repo_root)
    forge.remove_labels(entity_type, number, [label])
    # Always return True - label may not have existed
    return True


def transition_labels(
    entity_type: EntityType,
    number: int,
    add: list[str] | None = None,
    remove: list[str] | None = None,
    repo_root: Path | None = None,
    *,
    enforce_exclusion: bool = False,
) -> bool:
    """Atomically add and remove labels in a single API call.

    This function performs label transitions atomically using the
    ForgeClient's transition_labels method. The enforce_exclusion logic
    is applied above the ForgeClient layer to ensure consistent behavior
    across all forge backends.

    Args:
        entity_type: "issue" or "pr"
        number: Issue or PR number
        add: Labels to add (optional)
        remove: Labels to remove (optional)
        repo_root: Repository root path (optional)
        enforce_exclusion: When True, automatically remove all other labels
            in the same exclusion group as each label being added. This
            prevents contradictory label states. Default False for backward
            compatibility.

    Returns:
        True if successful, False otherwise.

    Example:
        # Atomic: loom:issue -> loom:building
        transition_labels("issue", 42, add=["loom:building"], remove=["loom:issue"])

        # Atomic: loom:building -> loom:blocked
        transition_labels("issue", 42, add=["loom:blocked"], remove=["loom:building"])

        # Auto-remove conflicting labels from exclusion groups
        transition_labels("pr", 100, add=["loom:pr"], enforce_exclusion=True)
        # Also removes loom:changes-requested and loom:review-requested
    """
    if not add and not remove:
        return True  # Nothing to do

    # Build the effective remove set -- enforce_exclusion logic stays above
    # ForgeClient to ensure consistent behavior across all forge backends.
    effective_remove = list(remove or [])

    if enforce_exclusion and add:
        extra_remove: set[str] = set()
        for label in add:
            for group in LABEL_EXCLUSION_GROUPS:
                if label in group:
                    # Remove all other labels in the group
                    extra_remove |= group - {label}
        # Merge without duplicates
        existing = set(effective_remove)
        for label in sorted(extra_remove):
            if label not in existing:
                effective_remove.append(label)

    forge = _get_forge_client(repo_root)
    return forge.transition_labels(
        entity_type,
        number,
        add=add,
        remove=effective_remove if effective_remove else None,
    )


def transition_issue_labels(
    issue: int,
    add: list[str] | None = None,
    remove: list[str] | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Atomically add and remove labels on an issue.

    Convenience wrapper around transition_labels() for issues.

    Args:
        issue: Issue number
        add: Labels to add (optional)
        remove: Labels to remove (optional)
        repo_root: Repository root path (optional)

    Returns:
        True if successful, False otherwise.
    """
    return transition_labels("issue", issue, add=add, remove=remove, repo_root=repo_root)


def transition_pr_labels(
    pr: int,
    add: list[str] | None = None,
    remove: list[str] | None = None,
    repo_root: Path | None = None,
) -> bool:
    """Atomically add and remove labels on a PR.

    Convenience wrapper around transition_labels() for PRs.

    Args:
        pr: PR number
        add: Labels to add (optional)
        remove: Labels to remove (optional)
        repo_root: Repository root path (optional)

    Returns:
        True if successful, False otherwise.
    """
    return transition_labels("pr", pr, add=add, remove=remove, repo_root=repo_root)


def get_issue_metadata(issue: int, repo_root: Path | None = None) -> dict | None:
    """Fetch issue metadata (url, state, title, labels) in a single API call.

    Uses the ForgeClient abstraction for forge-agnostic operation.

    Returns None if issue doesn't exist.
    """
    forge = _get_forge_client(repo_root)
    forge_issue = forge.get_issue(issue)
    if forge_issue is None:
        return None
    return {
        "url": forge_issue.url,
        "state": forge_issue.state,
        "title": forge_issue.title,
        "labels": [{"name": label} for label in forge_issue.labels],
    }


def get_pr_for_issue(
    issue: int, state: str = "open", repo_root: Path | None = None
) -> int | None:
    """Find a PR for an issue.

    Delegates to ForgeClient.find_pull_request_for_issue() which handles
    the search logic for each forge backend.

    Returns PR number if found, None otherwise.
    """
    forge = _get_forge_client(repo_root)
    return forge.find_pull_request_for_issue(issue, state=state)
