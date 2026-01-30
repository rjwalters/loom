"""Label caching and manipulation for GitHub issues and PRs."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path


class LabelCache:
    """Cache for issue/PR labels with invalidation.

    Reduces GitHub API calls by caching label lookups.
    Call invalidate() after any label mutation (add/remove).
    """

    def __init__(self, repo_root: Path | None = None) -> None:
        self._issue_labels: dict[int, set[str]] = {}
        self._pr_labels: dict[int, set[str]] = {}
        self._repo_root = repo_root
        self._gh_cmd = self._find_gh_cmd()

    def _find_gh_cmd(self) -> str:
        """Return gh-cached if available, otherwise gh."""
        if self._repo_root:
            gh_cached = self._repo_root / ".loom" / "scripts" / "gh-cached"
            if gh_cached.is_file() and gh_cached.stat().st_mode & 0o111:
                return str(gh_cached)
        return "gh"

    def _run_gh(self, args: list[str]) -> str:
        """Run gh command and return stdout."""
        cmd = [self._gh_cmd, *args]
        result = subprocess.run(
            cmd, capture_output=True, text=True, check=False, cwd=self._repo_root
        )
        if result.returncode != 0:
            return ""
        return result.stdout.strip()

    def _fetch_issue_labels(self, issue: int) -> set[str]:
        """Fetch labels for an issue from GitHub."""
        output = self._run_gh(
            ["issue", "view", str(issue), "--json", "labels", "--jq", ".labels[].name"]
        )
        if not output:
            return set()
        return set(output.splitlines())

    def _fetch_pr_labels(self, pr: int) -> set[str]:
        """Fetch labels for a PR from GitHub."""
        output = self._run_gh(
            ["pr", "view", str(pr), "--json", "labels", "--jq", ".labels[].name"]
        )
        if not output:
            return set()
        return set(output.splitlines())

    def get_issue_labels(self, issue: int, *, refresh: bool = False) -> set[str]:
        """Get labels for an issue, using cache if available.

        Args:
            issue: Issue number
            refresh: Force refresh from API (default False)

        Returns:
            Set of label names
        """
        if refresh or issue not in self._issue_labels:
            self._issue_labels[issue] = self._fetch_issue_labels(issue)
        return self._issue_labels[issue]

    def get_pr_labels(self, pr: int, *, refresh: bool = False) -> set[str]:
        """Get labels for a PR, using cache if available.

        Args:
            pr: PR number
            refresh: Force refresh from API (default False)

        Returns:
            Set of label names
        """
        if refresh or pr not in self._pr_labels:
            self._pr_labels[pr] = self._fetch_pr_labels(pr)
        return self._pr_labels[pr]

    def has_issue_label(self, issue: int, label: str) -> bool:
        """Check if an issue has a specific label."""
        return label in self.get_issue_labels(issue)

    def has_pr_label(self, pr: int, label: str) -> bool:
        """Check if a PR has a specific label."""
        return label in self.get_pr_labels(pr)

    def set_issue_labels(self, issue: int, labels: set[str]) -> None:
        """Pre-populate cache with labels (e.g., from initial metadata fetch)."""
        self._issue_labels[issue] = labels

    def invalidate_issue(self, issue: int | None = None) -> None:
        """Invalidate cached issue labels.

        Args:
            issue: Specific issue to invalidate, or None for all
        """
        if issue is not None:
            self._issue_labels.pop(issue, None)
        else:
            self._issue_labels.clear()

    def invalidate_pr(self, pr: int | None = None) -> None:
        """Invalidate cached PR labels.

        Args:
            pr: Specific PR to invalidate, or None for all
        """
        if pr is not None:
            self._pr_labels.pop(pr, None)
        else:
            self._pr_labels.clear()

    def invalidate(self) -> None:
        """Invalidate all cached labels."""
        self._issue_labels.clear()
        self._pr_labels.clear()


def add_issue_label(issue: int, label: str, repo_root: Path | None = None) -> bool:
    """Add a label to an issue.

    Returns True if successful.
    """
    cmd = ["gh", "issue", "edit", str(issue), "--add-label", label]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False, cwd=repo_root
    )
    return result.returncode == 0


def remove_issue_label(issue: int, label: str, repo_root: Path | None = None) -> bool:
    """Remove a label from an issue.

    Returns True if successful (or label didn't exist).
    """
    cmd = ["gh", "issue", "edit", str(issue), "--remove-label", label]
    subprocess.run(cmd, capture_output=True, text=True, check=False, cwd=repo_root)
    # Always return True - label may not have existed
    return True


def add_pr_label(pr: int, label: str, repo_root: Path | None = None) -> bool:
    """Add a label to a PR.

    Returns True if successful.
    """
    cmd = ["gh", "pr", "edit", str(pr), "--add-label", label]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False, cwd=repo_root
    )
    return result.returncode == 0


def remove_pr_label(pr: int, label: str, repo_root: Path | None = None) -> bool:
    """Remove a label from a PR.

    Returns True if successful (or label didn't exist).
    """
    cmd = ["gh", "pr", "edit", str(pr), "--remove-label", label]
    subprocess.run(cmd, capture_output=True, text=True, check=False, cwd=repo_root)
    # Always return True - label may not have existed
    return True


def get_issue_metadata(issue: int, repo_root: Path | None = None) -> dict | None:
    """Fetch issue metadata (url, state, title, labels) in a single API call.

    Returns None if issue doesn't exist.
    """
    cmd = ["gh", "issue", "view", str(issue), "--json", "url,state,title,labels"]
    result = subprocess.run(
        cmd, capture_output=True, text=True, check=False, cwd=repo_root
    )
    if result.returncode != 0 or not result.stdout.strip():
        return None
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return None


def get_pr_for_issue(
    issue: int, state: str = "open", repo_root: Path | None = None
) -> int | None:
    """Find a PR for an issue using multiple search patterns.

    Tries in order:
    1. Branch name: feature/issue-{issue}
    2. Body search: "Closes #{issue}"
    3. Body search: "Fixes #{issue}"
    4. Body search: "Resolves #{issue}"

    Returns PR number if found, None otherwise.
    """

    def _run_gh(args: list[str]) -> str:
        cmd = ["gh", *args]
        result = subprocess.run(
            cmd, capture_output=True, text=True, check=False, cwd=repo_root
        )
        if result.returncode != 0:
            return ""
        return result.stdout.strip()

    # Method 1: Branch-based lookup (deterministic, no indexing lag)
    output = _run_gh(
        [
            "pr",
            "list",
            "--head",
            f"feature/issue-{issue}",
            "--state",
            state,
            "--json",
            "number",
            "--jq",
            ".[0].number",
        ]
    )
    if output and output != "null":
        try:
            return int(output)
        except ValueError:
            pass

    # Methods 2-4: Search body for linking keywords (has indexing lag)
    for pattern in [f"Closes #{issue}", f"Fixes #{issue}", f"Resolves #{issue}"]:
        output = _run_gh(
            [
                "pr",
                "list",
                "--search",
                pattern,
                "--state",
                state,
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ]
        )
        if output and output != "null":
            try:
                return int(output)
            except ValueError:
                pass

    return None
