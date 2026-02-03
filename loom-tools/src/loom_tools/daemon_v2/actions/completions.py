"""Check and handle shepherd/support role completions."""

from __future__ import annotations

import pathlib
import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from loom_tools.common.logging import log_info, log_success, log_warning
from loom_tools.common.time_utils import now_utc

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


@dataclass
class CompletionEntry:
    """A completed shepherd or support role."""

    type: str  # "shepherd" or "support_role"
    name: str  # e.g., "shepherd-1" or "guide"
    issue: int | None = None
    task_id: str | None = None
    success: bool = True
    pr_merged: bool = False


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
                    success=False,
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

        # Trigger cleanup
        _trigger_shepherd_cleanup(ctx.repo_root, completion.issue)
    else:
        log_warning(
            f"Shepherd {completion.name} failed on issue #{completion.issue}"
        )


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
