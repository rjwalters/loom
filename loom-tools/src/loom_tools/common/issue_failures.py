"""Persistent cross-session failure log for issues.

Writes failure history to ``.loom/issue-failures.json`` so that retry metadata
survives daemon restarts. The in-memory ``blocked_issue_retries`` dict in
``daemon-state.json`` is ephemeral per session; this file provides the
durable record.

File format::

    {
        "entries": {
            "42": {
                "issue": 42,
                "total_failures": 3,
                "error_class": "builder_stuck",
                "phase": "builder",
                "details": "timed out after 30 minutes",
                "first_failure_at": "2026-01-20T10:00:00Z",
                "last_failure_at": "2026-01-22T14:30:00Z",
                "last_success_at": null
            }
        },
        "updated_at": "2026-01-22T14:30:00Z"
    }
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from loom_tools.common.paths import LoomPaths
from loom_tools.common.state import read_json_file, write_json_file

logger = logging.getLogger(__name__)

# After this many total failures, auto-block the issue
MAX_FAILURES_BEFORE_BLOCK = 5

# Backoff schedule: attempt -> iterations to skip
# 1st failure: 0, 2nd: 2, 3rd: 4, 4th: 8, 5th: auto-block
BACKOFF_BASE = 2


def _now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


@dataclass
class IssueFailureEntry:
    """Persistent failure record for a single issue."""

    issue: int = 0
    total_failures: int = 0
    error_class: str = "unknown"
    phase: str = ""
    details: str = ""
    first_failure_at: str | None = None
    last_failure_at: str | None = None
    last_success_at: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> IssueFailureEntry:
        return cls(
            issue=data.get("issue", 0),
            total_failures=data.get("total_failures", 0),
            error_class=data.get("error_class", "unknown"),
            phase=data.get("phase", ""),
            details=data.get("details", ""),
            first_failure_at=data.get("first_failure_at"),
            last_failure_at=data.get("last_failure_at"),
            last_success_at=data.get("last_success_at"),
        )

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "issue": self.issue,
            "total_failures": self.total_failures,
            "error_class": self.error_class,
            "phase": self.phase,
            "details": self.details,
        }
        if self.first_failure_at is not None:
            d["first_failure_at"] = self.first_failure_at
        if self.last_failure_at is not None:
            d["last_failure_at"] = self.last_failure_at
        if self.last_success_at is not None:
            d["last_success_at"] = self.last_success_at
        return d

    def backoff_iterations(self) -> int:
        """Calculate number of daemon iterations to skip before retrying.

        Schedule:
            1st failure: 0 iterations (retry immediately)
            2nd failure: 2 iterations
            3rd failure: 4 iterations
            4th failure: 8 iterations
            5th+: should be auto-blocked (MAX_FAILURES_BEFORE_BLOCK)
        """
        if self.total_failures <= 1:
            return 0
        if self.total_failures >= MAX_FAILURES_BEFORE_BLOCK:
            return -1  # Signal: should be auto-blocked
        return BACKOFF_BASE ** (self.total_failures - 1)

    @property
    def should_auto_block(self) -> bool:
        return self.total_failures >= MAX_FAILURES_BEFORE_BLOCK


@dataclass
class IssueFailureLog:
    """The full persistent failure log."""

    entries: dict[str, IssueFailureEntry] = field(default_factory=dict)
    updated_at: str | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> IssueFailureLog:
        entries_raw = data.get("entries", {})
        entries = {
            k: IssueFailureEntry.from_dict(v)
            for k, v in entries_raw.items()
        }
        return cls(
            entries=entries,
            updated_at=data.get("updated_at"),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "entries": {k: v.to_dict() for k, v in self.entries.items()},
            "updated_at": self.updated_at,
        }


def load_failure_log(repo_root: Path) -> IssueFailureLog:
    """Load the persistent failure log from disk.

    Returns an empty log if the file is missing or corrupt.
    """
    paths = LoomPaths(repo_root)
    fpath = paths.issue_failures_file

    if not fpath.is_file():
        return IssueFailureLog()

    try:
        data = read_json_file(fpath)
        if isinstance(data, dict):
            return IssueFailureLog.from_dict(data)
    except Exception:
        logger.warning("Corrupt issue-failures.json, starting fresh")

    return IssueFailureLog()


def save_failure_log(repo_root: Path, log: IssueFailureLog) -> None:
    """Save the persistent failure log to disk."""
    paths = LoomPaths(repo_root)
    log.updated_at = _now_iso()
    write_json_file(paths.issue_failures_file, log.to_dict())


def record_failure(
    repo_root: Path,
    issue: int,
    *,
    error_class: str = "unknown",
    phase: str = "",
    details: str = "",
) -> IssueFailureEntry:
    """Record a failure for an issue in the persistent log.

    Increments total_failures and updates metadata. Returns the updated entry.
    """
    log = load_failure_log(repo_root)
    issue_key = str(issue)
    now = _now_iso()

    entry = log.entries.get(issue_key)
    if entry is None:
        entry = IssueFailureEntry(
            issue=issue,
            total_failures=0,
            first_failure_at=now,
        )

    entry.total_failures += 1
    entry.error_class = error_class
    entry.phase = phase
    entry.details = details
    entry.last_failure_at = now

    log.entries[issue_key] = entry
    save_failure_log(repo_root, log)

    logger.info(
        "Recorded failure #%d for issue #%d (class=%s, phase=%s)",
        entry.total_failures,
        issue,
        error_class,
        phase,
    )
    return entry


def record_success(repo_root: Path, issue: int) -> None:
    """Record a successful completion for an issue.

    Removes the issue from the persistent failure log since it succeeded.
    """
    log = load_failure_log(repo_root)
    issue_key = str(issue)

    if issue_key in log.entries:
        del log.entries[issue_key]
        save_failure_log(repo_root, log)
        logger.info("Cleared failure history for issue #%d (success)", issue)


def get_failure_entry(repo_root: Path, issue: int) -> IssueFailureEntry | None:
    """Get the failure entry for a specific issue, or None if not tracked."""
    log = load_failure_log(repo_root)
    return log.entries.get(str(issue))


def merge_into_daemon_state(
    repo_root: Path,
    blocked_issue_retries: dict[str, Any],
) -> dict[str, Any]:
    """Merge persistent failure log into daemon state's blocked_issue_retries.

    For each entry in the persistent failure log, if the daemon state doesn't
    already have retry data (or has lower counts), update it from the
    persistent log.

    Returns the merged blocked_issue_retries dict.
    """
    log = load_failure_log(repo_root)

    for issue_key, entry in log.entries.items():
        existing = blocked_issue_retries.get(issue_key, {})
        existing_count = existing.get("retry_count", 0)

        # Use persistent log's count if higher
        if entry.total_failures > existing_count:
            existing["retry_count"] = entry.total_failures
            existing["error_class"] = entry.error_class
            existing["last_blocked_phase"] = entry.phase
            existing["last_blocked_details"] = entry.details
            if entry.last_failure_at:
                existing["last_blocked_at"] = entry.last_failure_at
            if entry.total_failures >= MAX_FAILURES_BEFORE_BLOCK:
                existing["retry_exhausted"] = True
            blocked_issue_retries[issue_key] = existing

    return blocked_issue_retries
