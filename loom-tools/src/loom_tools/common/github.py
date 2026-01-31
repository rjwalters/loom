"""Thin wrapper around the ``gh`` CLI."""

from __future__ import annotations

import shutil
import subprocess
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Literal, Sequence

from loom_tools.common.state import parse_command_output

EntityType = Literal["issue", "pr"]


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
    """
    cmd = [_gh_cmd(), *args]
    return subprocess.run(
        cmd,
        check=check,
        text=True,
        capture_output=capture,
    )


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
