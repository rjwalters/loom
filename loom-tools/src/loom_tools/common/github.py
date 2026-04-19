"""Thin wrapper around the ``gh`` CLI with dual-mode API support.

Supports both GraphQL (default ``gh`` CLI) and REST (``gh api``) modes,
with automatic fallback from GraphQL to REST on rate limit errors.

The API mode is controlled by the ``LOOM_GH_API_MODE`` environment variable:

- ``auto`` (default): Try GraphQL first, fall back to REST on rate limit.
- ``graphql``: Use GraphQL only (original behavior).
- ``rest``: Use REST only.

This module provides both a ``GitHubForge`` class implementing the
``ForgeClient`` protocol and backward-compatible module-level functions
that delegate to a singleton ``GitHubForge`` instance.
"""

from __future__ import annotations

import enum
import logging
import os
import re
import shutil
import subprocess
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any, Literal, Sequence

from loom_tools.common.forge import (
    ForgeCIStatus,
    ForgeClient,
    ForgeIssue,
    ForgePullRequest,
)
from loom_tools.common.state import parse_command_output, safe_parse_json

EntityType = Literal["issue", "pr"]

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# API mode configuration
# ---------------------------------------------------------------------------


class ApiMode(enum.Enum):
    """GitHub API access mode."""

    AUTO = "auto"
    GRAPHQL = "graphql"
    REST = "rest"


def get_api_mode() -> ApiMode:
    """Return the configured API mode from ``LOOM_GH_API_MODE`` env var.

    Defaults to :attr:`ApiMode.AUTO` when not set or invalid.
    """
    raw = os.environ.get("LOOM_GH_API_MODE", "auto").lower().strip()
    try:
        return ApiMode(raw)
    except ValueError:
        logger.warning("Invalid LOOM_GH_API_MODE=%r, defaulting to 'auto'", raw)
        return ApiMode.AUTO


# ---------------------------------------------------------------------------
# Repository NWO (name with owner) — module-level for backward compatibility
# ---------------------------------------------------------------------------

_nwo_cache: str | None = None


def get_repo_nwo(cwd: Path | None = None) -> str | None:
    """Parse ``owner/repo`` from git remote origin URL.

    Handles both SSH (``git@github.com:owner/repo.git``) and HTTPS
    (``https://github.com/owner/repo.git``) remote formats.

    Results are cached at module level for the process lifetime.

    Returns ``None`` if the remote URL cannot be parsed.
    """
    global _nwo_cache
    if _nwo_cache is not None:
        return _nwo_cache

    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return None

        nwo = _parse_nwo(result.stdout.strip())
        if nwo:
            _nwo_cache = nwo
        return nwo
    except OSError:
        return None


def _parse_nwo(url: str) -> str | None:
    """Extract ``owner/repo`` from a git remote URL.

    Supports:
    - SSH: ``git@github.com:owner/repo.git``
    - HTTPS: ``https://github.com/owner/repo.git``
    - HTTPS without .git: ``https://github.com/owner/repo``
    """
    # SSH format: git@github.com:owner/repo.git
    ssh_match = re.match(r"git@[^:]+:(.+?)(?:\.git)?$", url)
    if ssh_match:
        return ssh_match.group(1)

    # HTTPS format: https://github.com/owner/repo.git
    https_match = re.match(r"https?://[^/]+/(.+?)(?:\.git)?$", url)
    if https_match:
        return https_match.group(1)

    return None


def _reset_nwo_cache() -> None:
    """Reset the NWO cache (for testing)."""
    global _nwo_cache
    _nwo_cache = None


# ---------------------------------------------------------------------------
# Rate limit detection
# ---------------------------------------------------------------------------

# Patterns that indicate GraphQL rate limiting in gh CLI stderr
_RATE_LIMIT_PATTERNS = [
    "API rate limit exceeded",
    "rate limit",
    "abuse detection",
    "secondary rate limit",
    "HTTP 403",
    "You have exceeded a secondary rate limit",
]


def _is_rate_limited(result: subprocess.CompletedProcess[str]) -> bool:
    """Check if a ``gh`` command failed due to rate limiting.

    Examines both stderr and stdout for rate limit indicators.
    """
    if result.returncode == 0:
        return False

    text = (result.stderr or "") + (result.stdout or "")
    text_lower = text.lower()
    return any(pattern.lower() in text_lower for pattern in _RATE_LIMIT_PATTERNS)


# ---------------------------------------------------------------------------
# Core gh command execution
# ---------------------------------------------------------------------------


def _gh_cmd() -> str:
    """Return ``gh-cached`` if available and functional, otherwise ``gh``.

    Beyond checking PATH availability, we probe with ``--version`` to catch
    broken Python runtimes (e.g. unaccepted Xcode license, missing interpreter).
    The probe is lightweight -- ``gh-cached --version`` delegates to
    ``gh --version`` with no API calls and no cache interaction.
    """
    if shutil.which("gh-cached"):
        try:
            subprocess.run(
                ["gh-cached", "--version"],
                capture_output=True,
                timeout=5,
                check=True,
            )
            return "gh-cached"
        except (subprocess.SubprocessError, OSError):
            pass
    return "gh"


def gh_run(
    args: Sequence[str],
    *,
    check: bool = True,
    capture: bool = True,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a ``gh`` (or ``gh-cached``) command and return the result.

    Parameters
    ----------
    args:
        Arguments passed after the ``gh`` binary name.
    check:
        Raise on non-zero exit code (default ``True``).
    capture:
        Capture stdout/stderr (default ``True``).
    cwd:
        Working directory for the subprocess (default ``None``).
    """
    cmd = [_gh_cmd(), *args]
    return subprocess.run(
        cmd,
        check=check,
        text=True,
        capture_output=capture,
        cwd=cwd,
    )


# ---------------------------------------------------------------------------
# REST API helpers
# ---------------------------------------------------------------------------


def gh_api_rest(
    endpoint: str,
    *,
    method: str = "GET",
    fields: dict[str, str] | None = None,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    """Make a REST API call using ``gh api``.

    Parameters
    ----------
    endpoint:
        REST endpoint path (e.g., ``repos/owner/repo/issues/42``).
    method:
        HTTP method (default ``GET``).
    fields:
        Key-value pairs sent as request body fields.
    cwd:
        Working directory for the subprocess.
    """
    args = ["api", endpoint, "--method", method]
    if fields:
        for key, value in fields.items():
            args.extend(["-f", f"{key}={value}"])
    return gh_run(args, check=False, cwd=cwd)


# ---------------------------------------------------------------------------
# Response normalization (REST -> GraphQL-compatible shape)
# ---------------------------------------------------------------------------


def _normalize_rest_labels(labels: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Normalize REST label objects to match ``gh`` CLI output shape.

    REST returns full label objects; ``gh --json labels`` returns objects
    with at least ``name``, ``id``, ``color``, ``description``.
    We normalize to include ``name`` which is the field most callers check.
    """
    return [{"name": label.get("name", "")} for label in labels]


def _normalize_rest_entity(
    data: dict[str, Any],
    fields: Sequence[str] | None = None,
) -> dict[str, Any]:
    """Normalize a REST API issue/PR response to match ``gh`` CLI ``--json`` output.

    Key transformations:
    - ``state``: uppercase (``OPEN``, ``CLOSED``) to match GraphQL
    - ``html_url`` -> ``url``
    - ``labels``: simplified to ``[{"name": ...}]``
    """
    # Map REST field names to GraphQL-compatible names
    normalized: dict[str, Any] = {}

    field_map: dict[str, str] = {
        "html_url": "url",
    }

    for key, value in data.items():
        mapped_key = field_map.get(key, key)

        if mapped_key == "state" and isinstance(value, str):
            normalized[mapped_key] = value.upper()
        elif mapped_key == "labels" and isinstance(value, list):
            normalized[mapped_key] = _normalize_rest_labels(value)
        else:
            normalized[mapped_key] = value

    # Ensure url field exists (REST uses html_url)
    if "url" not in normalized and "html_url" in data:
        normalized["url"] = data["html_url"]

    # Filter to requested fields if specified
    if fields:
        normalized = {k: v for k, v in normalized.items() if k in fields}

    return normalized


# ---------------------------------------------------------------------------
# GitHubForge — ForgeClient implementation
# ---------------------------------------------------------------------------

_DEFAULT_ISSUE_FIELDS = ["number", "url", "state", "title", "labels"]


class GitHubForge:
    """GitHub implementation of the ``ForgeClient`` protocol.

    Wraps the existing ``gh`` CLI functions behind the forge-agnostic
    interface. Maintains backward compatibility: all existing module-level
    functions continue to work by delegating to a singleton instance.

    This class is **not** required to inherit from ``ForgeClient`` —
    Python's structural subtyping (``Protocol``) means it satisfies the
    protocol as long as all methods/properties match. We do *not*
    inherit to keep the class lightweight and avoid metaclass conflicts.

    GitHub-specific methods like ``run()``, ``api_rest()``, and
    ``parallel_queries()`` are available but are NOT part of the
    ``ForgeClient`` protocol.
    """

    def __init__(self, cwd: Path | None = None) -> None:
        self._cwd = cwd
        self._nwo_cache: str | None = None

    # --- ForgeClient.forge_type ---

    @property
    def forge_type(self) -> str:
        """Identifier for the forge backend."""
        return "github"

    # --- GitHub-specific: raw command execution (NOT on ForgeClient) ---

    def run(
        self,
        args: Sequence[str],
        *,
        check: bool = True,
        capture: bool = True,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run a ``gh`` CLI command. GitHub-specific escape hatch.

        This method is intentionally NOT part of the ``ForgeClient`` protocol.
        It exists for callers that need raw ``gh`` access (e.g., ``gh pr merge``,
        ``gh issue create`` with custom args). These callers will be migrated
        to protocol-level methods in follow-up issues.
        """
        return gh_run(args, check=check, capture=capture, cwd=cwd or self._cwd)

    def api_rest(
        self,
        endpoint: str,
        *,
        method: str = "GET",
        fields: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Make a REST API call. GitHub-specific helper."""
        return gh_api_rest(
            endpoint, method=method, fields=fields, cwd=cwd or self._cwd,
        )

    # --- Issue operations (ForgeClient) ---

    def get_issue(self, number: int) -> ForgeIssue | None:
        """Fetch a single issue by number."""
        data = gh_issue_view(number, cwd=self._cwd)
        if data is None:
            return None
        return _dict_to_forge_issue(data)

    def list_issues(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        limit: int | None = None,
    ) -> list[ForgeIssue]:
        """List issues matching the given filters."""
        results = gh_list(
            "issue", labels=labels, state=state, limit=limit,
        )
        return [_dict_to_forge_issue(d) for d in results]

    def create_issue(
        self,
        title: str,
        body: str,
        labels: Sequence[str] | None = None,
    ) -> ForgeIssue | None:
        """Create a new issue."""
        args = ["issue", "create", "--title", title, "--body", body]
        if labels:
            for label in labels:
                args.extend(["--label", label])
        args.extend(["--json", "number,url,state,title,labels"])

        result = gh_run(args, check=False, cwd=self._cwd)
        if result.returncode != 0 or not result.stdout.strip():
            return None
        data = safe_parse_json(result.stdout)
        if not isinstance(data, dict):
            return None
        return _dict_to_forge_issue(data)

    def close_issue(self, number: int) -> bool:
        """Close an issue."""
        result = gh_run(
            ["issue", "close", str(number)], check=False, cwd=self._cwd,
        )
        return result.returncode == 0

    def comment_on_issue(self, number: int, body: str) -> bool:
        """Add a comment to an issue."""
        return gh_issue_comment(number, body, cwd=self._cwd)

    # --- Pull request operations (ForgeClient) ---

    def get_pull_request(self, number: int) -> ForgePullRequest | None:
        """Fetch a single pull request by number."""
        fields = ["number", "state", "title", "labels", "url", "headRefName", "body"]
        data = gh_pr_view(number, fields=fields, cwd=self._cwd)
        if data is None:
            return None
        return _dict_to_forge_pr(data)

    def list_pull_requests(
        self,
        *,
        labels: Sequence[str] | None = None,
        state: str = "open",
        head: str | None = None,
        search: str | None = None,
        limit: int | None = None,
    ) -> list[ForgePullRequest]:
        """List pull requests matching the given filters."""
        results = gh_list(
            "pr", labels=labels, state=state, head=head,
            search=search, limit=limit,
        )
        return [_dict_to_forge_pr(d) for d in results]

    def create_pull_request(
        self,
        title: str,
        body: str,
        head: str,
        base: str | None = None,
        labels: Sequence[str] | None = None,
    ) -> ForgePullRequest | None:
        """Create a new pull request."""
        args = ["pr", "create", "--title", title, "--body", body, "--head", head]
        if base:
            args.extend(["--base", base])
        if labels:
            for label in labels:
                args.extend(["--label", label])
        args.extend(["--json", "number,url,state,title,labels,headRefName"])

        result = gh_run(args, check=False, cwd=self._cwd)
        if result.returncode != 0 or not result.stdout.strip():
            return None
        data = safe_parse_json(result.stdout)
        if not isinstance(data, dict):
            return None
        return _dict_to_forge_pr(data)

    def close_pull_request(
        self, number: int, comment: str | None = None,
    ) -> bool:
        """Close a pull request, optionally leaving a comment."""
        if comment:
            gh_run(
                ["pr", "comment", str(number), "--body", comment],
                check=False, cwd=self._cwd,
            )
        result = gh_run(
            ["pr", "close", str(number)], check=False, cwd=self._cwd,
        )
        return result.returncode == 0

    def merge_pull_request(
        self, number: int, method: str = "squash",
    ) -> bool:
        """Merge a pull request."""
        args = ["pr", "merge", str(number), f"--{method}", "--delete-branch"]
        result = gh_run(args, check=False, cwd=self._cwd)
        return result.returncode == 0

    def comment_on_pull_request(self, number: int, body: str) -> bool:
        """Add a comment to a pull request."""
        result = gh_run(
            ["pr", "comment", str(number), "--body", body],
            check=False, cwd=self._cwd,
        )
        return result.returncode == 0

    def get_pull_request_reviews(
        self, number: int,
    ) -> list[dict[str, Any]]:
        """Fetch reviews for a pull request."""
        result = gh_run(
            ["pr", "view", str(number), "--json", "reviews"],
            check=False, cwd=self._cwd,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return []
        data = safe_parse_json(result.stdout)
        if isinstance(data, dict) and "reviews" in data:
            reviews = data["reviews"]
            return reviews if isinstance(reviews, list) else []
        return []

    # --- Label operations (ForgeClient) ---

    def add_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Add labels to an issue or PR."""
        return gh_entity_edit(
            entity_type, number, add_labels=labels, cwd=self._cwd,
        )

    def remove_labels(
        self, entity_type: EntityType, number: int, labels: Sequence[str],
    ) -> bool:
        """Remove labels from an issue or PR."""
        return gh_entity_edit(
            entity_type, number, remove_labels=labels, cwd=self._cwd,
        )

    def transition_labels(
        self,
        entity_type: EntityType,
        number: int,
        add: Sequence[str] | None = None,
        remove: Sequence[str] | None = None,
    ) -> bool:
        """Atomically add and remove labels."""
        return gh_entity_edit(
            entity_type, number, add_labels=add, remove_labels=remove,
            cwd=self._cwd,
        )

    # --- CI status (ForgeClient) ---

    def get_default_branch_ci_status(self) -> ForgeCIStatus:
        """Get CI status for the latest runs on the default branch."""
        raw = gh_get_default_branch_ci_status()
        return ForgeCIStatus(
            status=raw.get("status", "unknown"),
            failed_runs=raw.get("failed_runs", []),
            total_runs=raw.get("total_runs", 0),
            message=raw.get("message", ""),
        )

    # --- Repository metadata (ForgeClient) ---

    def get_repo_nwo(self) -> str | None:
        """Return the ``owner/repo`` identifier."""
        return get_repo_nwo(self._cwd)

    def get_repo_default_branch(self) -> str | None:
        """Return the name of the repository's default branch."""
        result = gh_run(
            ["repo", "view", "--json", "defaultBranchRef"],
            check=False, cwd=self._cwd,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return None
        data = safe_parse_json(result.stdout)
        if isinstance(data, dict):
            ref = data.get("defaultBranchRef")
            if isinstance(ref, dict):
                return ref.get("name")
        return None

    # --- Batch operations (ForgeClient) ---

    def get_issues_batch(
        self, numbers: Sequence[int],
    ) -> dict[int, ForgeIssue | None]:
        """Fetch multiple issues by number concurrently."""
        results: dict[int, ForgeIssue | None] = {}

        def _fetch(num: int) -> tuple[int, ForgeIssue | None]:
            return (num, self.get_issue(num))

        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(_fetch, n) for n in numbers]
            for f in futures:
                num, issue = f.result()
                results[num] = issue

        return results

    def find_pull_request_for_issue(
        self, issue: int, state: str = "open",
    ) -> int | None:
        """Find a PR associated with a given issue."""
        # Try branch naming convention first
        prs = gh_list(
            "pr", head=f"feature/issue-{issue}", state=state,
            fields=["number"],
        )
        if prs:
            return prs[0].get("number")

        # Fall back to search by closing reference
        prs = gh_list(
            "pr", search=f"Closes #{issue}", state=state,
            fields=["number"],
        )
        if prs:
            return prs[0].get("number")

        return None

    # --- GitHub-specific methods (NOT on ForgeClient) ---

    def parallel_queries(
        self,
        queries: Sequence[tuple[Sequence[str]]],
        *,
        max_workers: int = 4,
    ) -> list[list[dict[str, Any]]]:
        """Execute multiple ``gh`` JSON queries concurrently.

        GitHub-specific. Not part of the ``ForgeClient`` protocol.
        """
        return gh_parallel_queries(queries, max_workers=max_workers)


# ---------------------------------------------------------------------------
# Conversion helpers (dict -> forge dataclasses)
# ---------------------------------------------------------------------------


def _extract_label_names(labels_data: Any) -> list[str]:
    """Extract label name strings from various label representations.

    Handles both ``[{"name": "bug"}]`` (gh CLI output) and
    ``["bug"]`` (plain string list) formats.
    """
    if not isinstance(labels_data, list):
        return []
    names: list[str] = []
    for item in labels_data:
        if isinstance(item, dict):
            name = item.get("name", "")
            if name:
                names.append(name)
        elif isinstance(item, str):
            names.append(item)
    return names


def _dict_to_forge_issue(data: dict[str, Any]) -> ForgeIssue:
    """Convert a raw ``gh`` JSON dict to a ``ForgeIssue``."""
    return ForgeIssue(
        number=data.get("number", 0),
        state=data.get("state", "OPEN"),
        title=data.get("title", ""),
        url=data.get("url", ""),
        labels=_extract_label_names(data.get("labels")),
        body=data.get("body"),
    )


def _dict_to_forge_pr(data: dict[str, Any]) -> ForgePullRequest:
    """Convert a raw ``gh`` JSON dict to a ``ForgePullRequest``."""
    return ForgePullRequest(
        number=data.get("number", 0),
        state=data.get("state", "OPEN"),
        title=data.get("title", ""),
        url=data.get("url", ""),
        labels=_extract_label_names(data.get("labels")),
        head_branch=data.get("headRefName"),
        body=data.get("body"),
    )


# ---------------------------------------------------------------------------
# Singleton forge instance
# ---------------------------------------------------------------------------

_forge_instance: GitHubForge | None = None


def get_forge(cwd: Path | None = None) -> GitHubForge:
    """Return a singleton ``GitHubForge`` instance.

    The first call determines the ``cwd``; subsequent calls return the
    same instance regardless of ``cwd`` argument. Use ``_reset_forge()``
    in tests to clear the singleton.
    """
    global _forge_instance
    if _forge_instance is None:
        _forge_instance = GitHubForge(cwd=cwd)
    return _forge_instance


def _reset_forge() -> None:
    """Reset the singleton forge instance (for testing)."""
    global _forge_instance
    _forge_instance = None


# ---------------------------------------------------------------------------
# High-level entity view functions (backward-compatible module-level)
# ---------------------------------------------------------------------------


def gh_issue_view(
    issue: int,
    fields: Sequence[str] | None = None,
    *,
    cwd: Path | None = None,
) -> dict[str, Any] | None:
    """View issue details with dual-mode API support.

    Tries GraphQL (``gh issue view``) first, falls back to REST
    (``gh api``) on rate limit in ``auto`` mode.

    Parameters
    ----------
    issue:
        Issue number.
    fields:
        JSON fields to request (default: number, url, state, title, labels).
    cwd:
        Working directory for the subprocess.

    Returns
    -------
    dict or None
        Issue metadata, or ``None`` if not found.
    """
    effective_fields = list(fields or _DEFAULT_ISSUE_FIELDS)
    mode = get_api_mode()

    # Try GraphQL first (unless REST-only mode)
    if mode != ApiMode.REST:
        result = gh_run(
            ["issue", "view", str(issue), "--json", ",".join(effective_fields)],
            check=False,
            cwd=cwd,
        )
        if result.returncode == 0 and result.stdout.strip():
            parsed = safe_parse_json(result.stdout)
            if isinstance(parsed, dict):
                return parsed

        # Only fall back if auto mode and rate limited
        if mode == ApiMode.GRAPHQL or not _is_rate_limited(result):
            return None

        logger.info("GraphQL rate limited, falling back to REST for issue #%d", issue)

    # REST fallback
    nwo = get_repo_nwo(cwd)
    if not nwo:
        logger.warning("Cannot determine repo NWO for REST fallback")
        return None

    result = gh_api_rest(f"repos/{nwo}/issues/{issue}", cwd=cwd)
    if result.returncode != 0 or not result.stdout.strip():
        return None

    data = safe_parse_json(result.stdout)
    if not isinstance(data, dict):
        return None

    # REST issues endpoint also returns PRs; filter them out
    if data.get("pull_request"):
        return None

    return _normalize_rest_entity(data, effective_fields)


def gh_pr_view(
    pr: int,
    fields: Sequence[str] | None = None,
    *,
    cwd: Path | None = None,
) -> dict[str, Any] | None:
    """View PR details with dual-mode API support.

    Tries GraphQL (``gh pr view``) first, falls back to REST
    (``gh api``) on rate limit in ``auto`` mode.

    Parameters
    ----------
    pr:
        PR number.
    fields:
        JSON fields to request.
    cwd:
        Working directory for the subprocess.

    Returns
    -------
    dict or None
        PR metadata, or ``None`` if not found.
    """
    effective_fields = list(fields or ["number", "state", "title", "labels"])
    mode = get_api_mode()

    # Try GraphQL first (unless REST-only mode)
    if mode != ApiMode.REST:
        result = gh_run(
            ["pr", "view", str(pr), "--json", ",".join(effective_fields)],
            check=False,
            cwd=cwd,
        )
        if result.returncode == 0 and result.stdout.strip():
            parsed = safe_parse_json(result.stdout)
            if isinstance(parsed, dict):
                return parsed

        if mode == ApiMode.GRAPHQL or not _is_rate_limited(result):
            return None

        logger.info("GraphQL rate limited, falling back to REST for PR #%d", pr)

    # REST fallback
    nwo = get_repo_nwo(cwd)
    if not nwo:
        logger.warning("Cannot determine repo NWO for REST fallback")
        return None

    result = gh_api_rest(f"repos/{nwo}/pulls/{pr}", cwd=cwd)
    if result.returncode != 0 or not result.stdout.strip():
        return None

    data = safe_parse_json(result.stdout)
    if not isinstance(data, dict):
        return None

    return _normalize_rest_entity(data, effective_fields)


# ---------------------------------------------------------------------------
# High-level entity edit / comment functions
# ---------------------------------------------------------------------------


def gh_entity_edit(
    entity_type: EntityType,
    number: int,
    *,
    add_labels: Sequence[str] | None = None,
    remove_labels: Sequence[str] | None = None,
    cwd: Path | None = None,
) -> bool:
    """Edit issue/PR labels with dual-mode support.

    Uses ``gh issue/pr edit`` for GraphQL or ``gh api`` for REST.

    Returns ``True`` if successful.
    """
    mode = get_api_mode()

    # Try GraphQL first
    if mode != ApiMode.REST:
        args = [entity_type, "edit", str(number)]
        for label in (remove_labels or []):
            args.extend(["--remove-label", label])
        for label in (add_labels or []):
            args.extend(["--add-label", label])

        result = gh_run(args, check=False, cwd=cwd)
        if result.returncode == 0:
            return True

        if mode == ApiMode.GRAPHQL or not _is_rate_limited(result):
            return False

        logger.info(
            "GraphQL rate limited, falling back to REST for %s #%d edit",
            entity_type, number,
        )

    # REST fallback for label operations
    nwo = get_repo_nwo(cwd)
    if not nwo:
        return False

    # REST requires separate add/remove calls via the labels API
    entity_path = "issues" if entity_type == "issue" else "issues"
    success = True

    for label in (add_labels or []):
        result = gh_api_rest(
            f"repos/{nwo}/{entity_path}/{number}/labels",
            method="POST",
            fields={"labels[]": label},
            cwd=cwd,
        )
        if result.returncode != 0:
            success = False

    for label in (remove_labels or []):
        result = gh_run(
            ["api", f"repos/{nwo}/{entity_path}/{number}/labels/{label}",
             "--method", "DELETE"],
            check=False,
            cwd=cwd,
        )
        if result.returncode != 0:
            # Label may not exist -- treat as success
            pass

    return success


def gh_issue_comment(
    issue: int,
    body: str,
    *,
    cwd: Path | None = None,
) -> bool:
    """Add a comment to an issue with dual-mode support.

    Returns ``True`` if successful.
    """
    mode = get_api_mode()

    # Try GraphQL first
    if mode != ApiMode.REST:
        result = gh_run(
            ["issue", "comment", str(issue), "--body", body],
            check=False,
            cwd=cwd,
        )
        if result.returncode == 0:
            return True

        if mode == ApiMode.GRAPHQL or not _is_rate_limited(result):
            return False

        logger.info(
            "GraphQL rate limited, falling back to REST for issue #%d comment",
            issue,
        )

    # REST fallback
    nwo = get_repo_nwo(cwd)
    if not nwo:
        return False

    result = gh_api_rest(
        f"repos/{nwo}/issues/{issue}/comments",
        method="POST",
        fields={"body": body},
        cwd=cwd,
    )
    return result.returncode == 0


# ---------------------------------------------------------------------------
# Existing list / query functions
# ---------------------------------------------------------------------------


def gh_list(
    entity_type: EntityType,
    *,
    labels: Sequence[str] | None = None,
    state: str = "open",
    fields: Sequence[str] | None = None,
    search: str | None = None,
    head: str | None = None,
    limit: int | None = None,
) -> list[dict[str, Any]]:
    """List issues or PRs via ``gh`` CLI with unified interface.

    Parameters
    ----------
    entity_type:
        Either ``"issue"`` or ``"pr"``.
    labels:
        Filter by labels (comma-joined).
    state:
        Filter by state (``open``, ``closed``, ``all``).
    fields:
        JSON fields to return (default: ``number``, ``title``, ``labels``, ``state``).
    search:
        GitHub search query (for PRs).
    head:
        Filter PRs by head branch.
    limit:
        Maximum results to return.

    Returns
    -------
    list[dict[str, Any]]
        List of matching issues/PRs, or empty list on error.
    """
    default_fields = ["number", "title", "labels", "state"]
    field_list = ",".join(fields or default_fields)

    args = [entity_type, "list", "--json", field_list, "--state", state]

    if labels:
        args.extend(["--label", ",".join(labels)])
    if search:
        args.extend(["--search", search])
    if head:
        args.extend(["--head", head])
    if limit is not None:
        args.extend(["--limit", str(limit)])

    result = gh_run(args, check=False)
    parsed = parse_command_output(result, default=[])
    return parsed if isinstance(parsed, list) else []


def gh_issue_list(
    labels: Sequence[str] | None = None,
    state: str = "open",
    fields: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """List issues via ``gh issue list --json``.

    Thin wrapper around :func:`gh_list`.
    """
    return gh_list("issue", labels=labels, state=state, fields=fields)


def gh_pr_list(
    labels: Sequence[str] | None = None,
    state: str = "open",
    fields: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """List pull requests via ``gh pr list --json``.

    Thin wrapper around :func:`gh_list`.
    """
    return gh_list("pr", labels=labels, state=state, fields=fields)


def gh_parallel_queries(
    queries: Sequence[tuple[Sequence[str]]],
    *,
    max_workers: int = 4,
) -> list[list[dict[str, Any]]]:
    """Execute multiple ``gh`` JSON queries concurrently.

    Each element of *queries* is a tuple of args passed to :func:`gh_run`.
    Returns a list of parsed JSON results in the same order.
    """

    def _run(args: Sequence[str]) -> list[dict[str, Any]]:
        result = gh_run(args, check=False)
        parsed = parse_command_output(result, default=[])
        return parsed if isinstance(parsed, list) else []

    with ThreadPoolExecutor(max_workers=max_workers) as pool:
        futures = [pool.submit(_run, q[0] if isinstance(q, tuple) else q) for q in queries]
        return [f.result() for f in futures]


def gh_get_default_branch_ci_status() -> dict[str, Any]:
    """Get CI status for the default branch's latest workflow runs.

    Returns a dict with:
        - status: "passing", "failing", or "unknown"
        - failed_runs: list of failed workflow run names
        - total_runs: total recent workflow runs checked
        - message: human-readable summary
    """
    try:
        # Get recent workflow runs on the default branch
        result = gh_run(
            ["run", "list", "--branch", "main", "--limit", "5", "--json",
             "name,conclusion,status,headBranch"],
            check=False,
        )
        runs = parse_command_output(result, default=[])
        if not isinstance(runs, list) or not runs:
            return {"status": "unknown", "failed_runs": [], "total_runs": 0, "message": "No recent workflow runs found"}

        # Group by workflow name, keep only the most recent run for each workflow
        latest_by_name: dict[str, dict[str, Any]] = {}
        for run in runs:
            name = run.get("name", "Unknown")
            if name not in latest_by_name:
                latest_by_name[name] = run

        # Check for failures (excluding in-progress runs)
        failed_runs = []
        for name, run in latest_by_name.items():
            conclusion = run.get("conclusion", "")
            status = run.get("status", "")
            # Only count completed runs that failed
            if status == "completed" and conclusion == "failure":
                failed_runs.append(name)

        total_runs = len(latest_by_name)
        if failed_runs:
            return {
                "status": "failing",
                "failed_runs": failed_runs,
                "total_runs": total_runs,
                "message": f"CI failing: {len(failed_runs)} workflow(s) failed on main",
            }

        return {
            "status": "passing",
            "failed_runs": [],
            "total_runs": total_runs,
            "message": "CI passing on main",
        }

    except (OSError, subprocess.SubprocessError):
        return {"status": "unknown", "failed_runs": [], "total_runs": 0, "message": "Error checking CI status"}
