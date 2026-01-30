"""Thin wrapper around the ``gh`` CLI."""

from __future__ import annotations

import json
import shutil
import subprocess
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Sequence


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


def gh_issue_list(
    labels: Sequence[str] | None = None,
    state: str = "open",
    fields: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """List issues via ``gh issue list --json``."""
    default_fields = ["number", "title", "labels", "state"]
    field_list = ",".join(fields or default_fields)

    args = ["issue", "list", "--json", field_list, "--state", state]
    if labels:
        args.extend(["--label", ",".join(labels)])

    result = gh_run(args)
    return json.loads(result.stdout) if result.stdout.strip() else []


def gh_pr_list(
    labels: Sequence[str] | None = None,
    state: str = "open",
    fields: Sequence[str] | None = None,
) -> list[dict[str, Any]]:
    """List pull requests via ``gh pr list --json``."""
    default_fields = ["number", "title", "labels", "state"]
    field_list = ",".join(fields or default_fields)

    args = ["pr", "list", "--json", field_list, "--state", state]
    if labels:
        args.extend(["--label", ",".join(labels)])

    result = gh_run(args)
    return json.loads(result.stdout) if result.stdout.strip() else []


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
        if result.returncode != 0 or not result.stdout.strip():
            return []
        return json.loads(result.stdout)

    with ThreadPoolExecutor(max_workers=max_workers) as pool:
        futures = [pool.submit(_run, q[0] if isinstance(q, tuple) else q) for q in queries]
        return [f.result() for f in futures]
