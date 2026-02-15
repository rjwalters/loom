"""Thin wrapper around the ``gh`` CLI with dual-mode API support.

Supports both GraphQL (default ``gh`` CLI) and REST (``gh api``) modes,
with automatic fallback from GraphQL to REST on rate limit errors.

The API mode is controlled by the ``LOOM_GH_API_MODE`` environment variable:

- ``auto`` (default): Try GraphQL first, fall back to REST on rate limit.
- ``graphql``: Use GraphQL only (original behavior).
- ``rest``: Use REST only.
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
# Repository NWO (name with owner)
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
    """Return ``gh-cached`` if available, otherwise ``gh``."""
    if shutil.which("gh-cached"):
        return "gh-cached"
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
# High-level entity view functions
# ---------------------------------------------------------------------------


_DEFAULT_ISSUE_FIELDS = ["number", "url", "state", "title", "labels"]


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
            # Label may not exist — treat as success
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
