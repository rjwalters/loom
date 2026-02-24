"""Retry blocked issues that have passed their cooldown period."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.time_utils import now_utc
from loom_tools.models.daemon_state import BlockedIssueRetry

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


def escalate_blocked_issues(
    escalation_needed: list[dict[str, Any]],
    ctx: "DaemonContext",
) -> int:
    """Escalate retry-exhausted blocked issues to the human input queue.

    For each issue in *escalation_needed*:
    1. Add an entry to ``daemon_state.needs_human_input``
    2. Add a GitHub comment explaining that the issue needs human review
    3. Mark ``escalated_to_human = True`` in ``blocked_issue_retries`` to
       prevent duplicate escalations on subsequent iterations

    Returns the number of issues successfully escalated.
    """
    if not escalation_needed:
        return 0

    escalated = 0
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    for item in escalation_needed:
        issue_num = item.get("number")
        if issue_num is None:
            continue

        error_class = item.get("error_class", "unknown")
        retry_count = item.get("retry_count", 0)
        reason = item.get("reason", f"Retry budget exhausted for {error_class}")

        # Add to needs_human_input in daemon state
        if ctx.state is not None:
            human_entry: dict[str, Any] = {
                "type": "exhausted_retry",
                "issue": issue_num,
                "error_class": error_class,
                "retry_count": retry_count,
                "reason": reason,
                "escalated_at": timestamp,
            }
            # Deduplicate: only add if not already present
            already_present = any(
                e.get("type") == "exhausted_retry" and e.get("issue") == issue_num
                for e in ctx.state.needs_human_input
            )
            if not already_present:
                ctx.state.needs_human_input.append(human_entry)

            # Mark escalated in blocked_issue_retries to prevent re-escalation
            issue_key = str(issue_num)
            retry_entry = ctx.state.blocked_issue_retries.get(issue_key)
            if retry_entry is None:
                retry_entry = BlockedIssueRetry()
                ctx.state.blocked_issue_retries[issue_key] = retry_entry
            retry_entry.escalated_to_human = True

        # Add GitHub comment
        comment = (
            f"**Blocked Issue: Human Review Required**\n\n"
            f"This issue has exceeded its automatic retry budget and needs human attention.\n\n"
            f"**Error class**: `{error_class}`\n"
            f"**Retry attempts**: {retry_count}\n"
            f"**Reason**: {reason}\n\n"
            f"Please review this issue and either:\n"
            f"- Fix the underlying problem and remove the `loom:blocked` label to re-queue it, or\n"
            f"- Close the issue if it is no longer valid\n\n"
            f"---\n"
            f"*Escalated by daemon retry manager at {timestamp}*"
        )
        try:
            gh_run(
                ["issue", "comment", str(issue_num), "--body", comment],
                check=False,
            )
        except Exception:
            log_warning(f"Failed to add escalation comment on issue #{issue_num}")

        log_info(
            f"Escalated blocked issue #{issue_num} to human input queue "
            f"(error_class={error_class}, retry_count={retry_count})"
        )
        escalated += 1

    if escalated > 0:
        log_info(f"Escalated {escalated} retry-exhausted issue(s) to human input queue")

    return escalated


def retry_blocked_issues(
    retryable_issues: list[dict[str, Any]],
    ctx: DaemonContext,
) -> int:
    """Retry blocked issues that have passed their cooldown period.

    For each retryable issue:
    1. Increment ``retry_count`` and set ``last_retry_at`` in daemon state
    2. Swap GitHub labels from ``loom:blocked`` to ``loom:issue``
    3. Add a comment explaining the retry attempt

    If ``retry_count`` reaches ``max_retry_count`` after increment, marks
    the issue as permanently blocked with ``retry_exhausted = True``.

    Returns the number of issues successfully retried.
    """
    if not retryable_issues:
        return 0

    retried = 0
    for item in retryable_issues:
        issue_num = item.get("number")
        retry_count = item.get("retry_count", 0)

        if issue_num is None:
            continue

        if _retry_single_issue(issue_num, retry_count, ctx):
            retried += 1

    if retried > 0:
        log_info(f"Retried {retried} blocked issue(s)")

    return retried


def _retry_single_issue(
    issue: int,
    current_retry_count: int,
    ctx: DaemonContext,
) -> bool:
    """Retry a single blocked issue.

    Updates daemon state, swaps GitHub labels, and adds a comment.
    Returns True if the issue was successfully re-queued.
    """
    new_retry_count = current_retry_count + 1
    timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Update retry metadata in daemon state
    _update_retry_state(ctx, issue, new_retry_count, timestamp)

    # Check if this issue still has loom:blocked label before swapping
    # (a human may have already unblocked it)
    if not _issue_has_label(issue, "loom:blocked"):
        log_info(
            f"Issue #{issue} no longer has loom:blocked label, skipping retry"
        )
        return False

    # Swap labels: loom:blocked -> loom:issue
    try:
        gh_run(
            [
                "issue", "edit", str(issue),
                "--remove-label", "loom:blocked",
                "--add-label", "loom:issue",
            ],
            check=False,
        )
    except Exception:
        log_warning(f"Failed to swap labels for issue #{issue}")
        return False

    # Add comment explaining the retry
    comment = (
        f"**Blocked Issue Retry (attempt {new_retry_count})**\n\n"
        f"This issue was previously blocked and has been automatically "
        f"re-queued for another attempt after the cooldown period elapsed.\n\n"
        f"**Retry**: {new_retry_count}/{_get_max_retries(ctx)}\n\n"
        f"---\n"
        f"*Retried by daemon blocked issue retry at {timestamp}*"
    )
    try:
        gh_run(
            ["issue", "comment", str(issue), "--body", comment],
            check=False,
        )
    except Exception:
        log_warning(f"Failed to comment on issue #{issue}")

    log_info(
        f"Retried blocked issue #{issue} "
        f"(attempt {new_retry_count}/{_get_max_retries(ctx)})"
    )
    return True


def _update_retry_state(
    ctx: DaemonContext,
    issue: int,
    new_retry_count: int,
    timestamp: str,
) -> None:
    """Update the retry metadata in daemon state."""
    if ctx.state is None:
        return

    issue_key = str(issue)
    retry_entry = ctx.state.blocked_issue_retries.get(issue_key)

    if retry_entry is None:
        retry_entry = BlockedIssueRetry()
        ctx.state.blocked_issue_retries[issue_key] = retry_entry

    retry_entry.retry_count = new_retry_count
    retry_entry.last_retry_at = timestamp


def _get_max_retries(ctx: DaemonContext) -> int:
    """Get max retry count from snapshot config."""
    if ctx.snapshot is None:
        return 3
    return ctx.snapshot.get("config", {}).get("max_retry_count", 3)


def _issue_has_label(issue: int, label: str) -> bool:
    """Check if an issue currently has a specific label."""
    try:
        result = gh_run(
            [
                "issue", "view", str(issue),
                "--json", "labels",
                "--jq", f'[.labels[].name] | any(. == "{label}")',
            ],
            check=False,
        )
        return result.stdout.strip().lower() == "true"
    except Exception:
        # If we can't check, assume label is present and proceed
        return True
