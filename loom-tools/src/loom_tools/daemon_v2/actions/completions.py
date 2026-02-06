"""Check and handle shepherd/support role completions."""

from __future__ import annotations

import pathlib
import subprocess
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import TYPE_CHECKING, Any

from loom_tools.common.github import gh_run
from loom_tools.common.issue_failures import (
    MAX_FAILURES_BEFORE_BLOCK,
    record_failure,
    record_success,
)
from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import TransientRetryEntry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext

# Transient error patterns that indicate API/infrastructure issues (not code bugs)
TRANSIENT_ERROR_PATTERNS = (
    "500 Internal Server Error",
    "Rate limit exceeded",
    "rate_limit",
    "overloaded",
    "temporarily unavailable",
    "503 Service",
    "502 Bad Gateway",
    "Connection refused",
    "ECONNREFUSED",
    "ETIMEDOUT",
    "ECONNRESET",
    "NetworkError",
    "network error",
    "socket hang up",
    "No messages returned",
)

# Maximum retries for transient errors per issue
MAX_TRANSIENT_RETRIES = 3

# Backoff duration in seconds before retrying a transiently-failed issue
TRANSIENT_BACKOFF_SECONDS = 300  # 5 minutes


@dataclass
class CompletionEntry:
    """A completed shepherd or support role."""

    type: str  # "shepherd" or "support_role"
    name: str  # e.g., "shepherd-1" or "guide"
    issue: int | None = None
    task_id: str | None = None
    success: bool = True
    pr_merged: bool = False
    is_transient_error: bool = False


def _check_transient_error(milestones: list[dict[str, Any]]) -> bool:
    """Check if progress milestones indicate a transient API error.

    Looks at error events in the milestones for patterns that match
    known transient API/infrastructure failures.
    """
    for milestone in reversed(milestones):
        if milestone.get("event") == "error":
            error_msg = milestone.get("data", {}).get("error", "")
            for pattern in TRANSIENT_ERROR_PATTERNS:
                if pattern.lower() in error_msg.lower():
                    return True
        if milestone.get("event") == "transient_error":
            return True
    return False


def check_completions(ctx: DaemonContext) -> list[CompletionEntry]:
    """Check for completed shepherds and support roles.

    Uses the snapshot's shepherd progress and daemon state to detect
    completed work.
    """
    if ctx.snapshot is None or ctx.state is None:
        return []

    completed: list[CompletionEntry] = []

    # Check shepherd progress files for completed status
    shepherd_progress = ctx.snapshot.get("shepherds", {}).get("progress", [])
    for progress in shepherd_progress:
        if progress.get("status") == "completed":
            task_id = progress.get("task_id")
            issue = progress.get("issue")

            # Find the shepherd entry in daemon state
            shepherd_name = None
            for name, entry in ctx.state.shepherds.items():
                if entry.task_id == task_id:
                    shepherd_name = name
                    break

            if shepherd_name:
                completed.append(CompletionEntry(
                    type="shepherd",
                    name=shepherd_name,
                    issue=issue,
                    task_id=task_id,
                    success=True,
                    pr_merged=True,
                ))

    # Check for errored shepherds (need cleanup but not success)
    for progress in shepherd_progress:
        if progress.get("status") == "errored":
            task_id = progress.get("task_id")
            issue = progress.get("issue")
            milestones = progress.get("milestones", [])

            shepherd_name = None
            for name, entry in ctx.state.shepherds.items():
                if entry.task_id == task_id:
                    shepherd_name = name
                    break

            if shepherd_name:
                is_transient = _check_transient_error(milestones)
                completed.append(CompletionEntry(
                    type="shepherd",
                    name=shepherd_name,
                    issue=issue,
                    task_id=task_id,
                    success=False,
                    is_transient_error=is_transient,
                ))

    return completed


def handle_completion(ctx: DaemonContext, completion: CompletionEntry) -> None:
    """Handle a completed shepherd or support role.

    Updates daemon state and triggers cleanup.
    """
    if ctx.state is None:
        return

    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    if completion.type == "shepherd":
        _handle_shepherd_completion(ctx, completion, timestamp)
    elif completion.type == "support_role":
        _handle_support_role_completion(ctx, completion, timestamp)


def _handle_shepherd_completion(
    ctx: DaemonContext,
    completion: CompletionEntry,
    timestamp: str,
) -> None:
    """Handle shepherd completion - update state and trigger cleanup."""
    if ctx.state is None:
        return

    shepherd_entry = ctx.state.shepherds.get(completion.name)
    if shepherd_entry is None:
        return

    # Update shepherd to idle
    shepherd_entry.status = "idle"
    shepherd_entry.idle_since = timestamp
    shepherd_entry.idle_reason = "completed_issue"
    shepherd_entry.last_issue = completion.issue
    shepherd_entry.last_completed = timestamp
    shepherd_entry.task_id = None
    shepherd_entry.output_file = None
    shepherd_entry.issue = None
    shepherd_entry.pr_number = None

    if completion.success:
        log_success(
            f"Shepherd {completion.name} completed issue #{completion.issue}"
        )

        # Track completed issues
        if completion.issue is not None:
            ctx.state.completed_issues.append(completion.issue)
            if completion.pr_merged:
                ctx.state.total_prs_merged += 1

        # Clear transient retry tracking on success
        if completion.issue is not None:
            issue_key = str(completion.issue)
            ctx.state.transient_retries.pop(issue_key, None)

            # Clear persistent failure log on success
            record_success(ctx.repo_root, completion.issue)

        # Trigger cleanup
        _trigger_shepherd_cleanup(ctx.repo_root, completion.issue)
    elif completion.is_transient_error and completion.issue is not None:
        _handle_transient_error_retry(ctx, completion, timestamp)
    else:
        log_warning(
            f"Shepherd {completion.name} failed on issue #{completion.issue}"
        )

        # Record non-transient failure in persistent log
        if completion.issue is not None:
            _record_persistent_failure(ctx, completion)


def _handle_transient_error_retry(
    ctx: DaemonContext,
    completion: CompletionEntry,
    timestamp: str,
) -> None:
    """Handle a shepherd failure caused by a transient API error.

    Re-queues the issue for retry if under the retry limit, with backoff.
    """
    if ctx.state is None or completion.issue is None:
        return

    issue_key = str(completion.issue)
    retry_entry = ctx.state.transient_retries.get(
        issue_key, TransientRetryEntry()
    )

    if retry_entry.retry_count >= MAX_TRANSIENT_RETRIES:
        log_warning(
            f"Issue #{completion.issue} exhausted transient retries "
            f"({retry_entry.retry_count}/{MAX_TRANSIENT_RETRIES}) - "
            f"not requeuing"
        )
        return

    # Increment retry count and set backoff
    retry_entry.retry_count += 1
    retry_entry.last_retry_at = timestamp
    backoff_until = (
        now_utc() + timedelta(seconds=TRANSIENT_BACKOFF_SECONDS)
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    retry_entry.backoff_until = backoff_until
    ctx.state.transient_retries[issue_key] = retry_entry

    log_warning(
        f"Shepherd {completion.name} failed on issue #{completion.issue} "
        f"due to transient API error (retry {retry_entry.retry_count}/"
        f"{MAX_TRANSIENT_RETRIES}). Requeuing with {TRANSIENT_BACKOFF_SECONDS}s backoff."
    )

    # Re-queue the issue: swap loom:building back to loom:issue
    _requeue_issue(completion.issue, retry_entry.retry_count, timestamp)


def _requeue_issue(
    issue: int,
    retry_count: int,
    timestamp: str,
) -> None:
    """Re-queue an issue by swapping labels and adding a comment."""
    try:
        result = gh_run(
            [
                "issue", "edit", str(issue),
                "--remove-label", "loom:building",
                "--add-label", "loom:issue",
            ],
            check=False,
        )
        if result.returncode == 0:
            log_info(f"Requeued issue #{issue} (loom:building -> loom:issue)")

            comment = (
                f"**Transient Error Recovery**\n\n"
                f"This issue was automatically requeued after a transient API error.\n\n"
                f"**Retry**: {retry_count}/{MAX_TRANSIENT_RETRIES}\n"
                f"**Backoff**: {TRANSIENT_BACKOFF_SECONDS}s before next attempt\n\n"
                f"---\n"
                f"*Recovered by daemon transient error handler at {timestamp}*"
            )
            gh_run(
                ["issue", "comment", str(issue), "--body", comment],
                check=False,
            )
        else:
            log_warning(f"Failed to requeue issue #{issue}")
    except Exception:
        log_warning(f"Failed to requeue issue #{issue}")


def _record_persistent_failure(
    ctx: DaemonContext,
    completion: CompletionEntry,
) -> None:
    """Record a non-transient shepherd failure to the persistent failure log.

    If the issue has reached MAX_FAILURES_BEFORE_BLOCK, automatically labels
    it as loom:blocked with a comment explaining the failure pattern.
    """
    if completion.issue is None:
        return

    # Determine error class and phase from shepherd entry
    shepherd_entry = ctx.state.shepherds.get(completion.name) if ctx.state else None
    phase = shepherd_entry.last_phase if shepherd_entry else "unknown"
    error_class = "shepherd_failure"

    entry = record_failure(
        ctx.repo_root,
        completion.issue,
        error_class=error_class,
        phase=phase or "",
        details=f"Shepherd {completion.name} failed during {phase or 'unknown'} phase",
    )

    if entry.should_auto_block:
        log_warning(
            f"Issue #{completion.issue} has failed {entry.total_failures} times "
            f"(>= {MAX_FAILURES_BEFORE_BLOCK}) - auto-labeling as loom:blocked"
        )
        _auto_block_issue(completion.issue, entry)


def _auto_block_issue(issue: int, entry: "IssueFailureEntry") -> None:
    """Auto-label an issue as loom:blocked after too many failures."""
    from loom_tools.common.issue_failures import IssueFailureEntry  # noqa: F811

    try:
        gh_run(
            [
                "issue", "edit", str(issue),
                "--remove-label", "loom:issue",
                "--add-label", "loom:blocked",
            ],
            check=False,
        )

        comment = (
            f"**Persistent Failure - Auto-Blocked**\n\n"
            f"This issue has failed **{entry.total_failures}** times across daemon sessions "
            f"and has been automatically blocked.\n\n"
            f"**Last error class**: `{entry.error_class}`\n"
            f"**Last phase**: `{entry.phase}`\n"
            f"**First failure**: {entry.first_failure_at or 'unknown'}\n"
            f"**Last failure**: {entry.last_failure_at or 'unknown'}\n\n"
            f"A maintainer should investigate before re-labeling as `loom:issue`.\n\n"
            f"---\n"
            f"*Auto-blocked by daemon persistent failure tracking*"
        )
        gh_run(
            ["issue", "comment", str(issue), "--body", comment],
            check=False,
        )
    except Exception:
        log_warning(f"Failed to auto-block issue #{issue}")


def _handle_support_role_completion(
    ctx: DaemonContext,
    completion: CompletionEntry,
    timestamp: str,
) -> None:
    """Handle support role completion - update state."""
    if ctx.state is None:
        return

    role_entry = ctx.state.support_roles.get(completion.name)
    if role_entry is None:
        return

    role_entry.status = "idle"
    role_entry.last_completed = timestamp
    role_entry.task_id = None
    role_entry.tmux_session = None

    log_info(f"Support role {completion.name} completed")


def _trigger_shepherd_cleanup(repo_root: pathlib.Path, issue: int | None) -> None:
    """Trigger shepherd cleanup via loom-daemon-cleanup."""
    if issue is None:
        return

    try:
        # Use loom-daemon-cleanup for event-driven cleanup
        venv_cleanup = repo_root / "loom-tools" / ".venv" / "bin" / "loom-daemon-cleanup"
        if venv_cleanup.is_file():
            subprocess.run(
                [str(venv_cleanup), "shepherd-complete", str(issue)],
                capture_output=True,
                timeout=60,
                cwd=repo_root,
            )
        else:
            # Try system-installed
            subprocess.run(
                ["loom-daemon-cleanup", "shepherd-complete", str(issue)],
                capture_output=True,
                timeout=60,
                cwd=repo_root,
            )
    except Exception:
        log_warning(f"Failed to trigger cleanup for issue #{issue}")
