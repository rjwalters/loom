"""Escalate spinning issues — PRs stuck in review cycles."""

from __future__ import annotations

from typing import Any

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.time_utils import now_utc


def escalate_spinning_issues(
    spinning_prs: list[dict[str, Any]],
) -> int:
    """Block linked issues for PRs stuck in review cycles.

    For each spinning PR, closes the PR with a comment explaining the
    review cycle pattern, and labels the linked issue as ``loom:blocked``
    so a human can investigate.

    Returns the number of issues escalated.
    """
    escalated = 0

    for entry in spinning_prs:
        pr_number = entry.get("pr_number")
        review_cycles = entry.get("review_cycles", 0)
        linked_issue = entry.get("linked_issue")

        if pr_number is None:
            continue

        log_warning(
            f"Spinning PR #{pr_number} detected: {review_cycles} review cycles"
            + (f" (linked to issue #{linked_issue})" if linked_issue else "")
        )

        # Add comment to the PR explaining the escalation
        timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
        pr_comment = (
            f"**Spinning Issue Detected — Auto-Escalated**\n\n"
            f"This PR has been through **{review_cycles}** review cycles "
            f"(judge requests changes → doctor fixes → judge requests changes again) "
            f"without converging.\n\n"
            f"The review loop has been automatically escalated for human intervention.\n\n"
            f"---\n"
            f"*Detected by daemon spinning issue detection at {timestamp}*"
        )
        try:
            gh_run(
                ["pr", "comment", str(pr_number), "--body", pr_comment],
                check=False,
            )
        except Exception:
            log_warning(f"Failed to comment on PR #{pr_number}")

        # Close the spinning PR to stop the cycle
        try:
            gh_run(
                ["pr", "close", str(pr_number)],
                check=False,
            )
            log_info(f"Closed spinning PR #{pr_number}")
        except Exception:
            log_warning(f"Failed to close PR #{pr_number}")

        # Block the linked issue if we found one
        if linked_issue is not None:
            if _block_linked_issue(linked_issue, pr_number, review_cycles, timestamp):
                escalated += 1

    if escalated > 0:
        log_info(f"Escalated {escalated} spinning issue(s)")

    return escalated


def _block_linked_issue(
    issue: int,
    pr_number: int,
    review_cycles: int,
    timestamp: str,
) -> bool:
    """Label a linked issue as blocked due to spinning PR.

    Returns True if the issue was successfully blocked.
    """
    try:
        gh_run(
            [
                "issue", "edit", str(issue),
                "--remove-label", "loom:building",
                "--add-label", "loom:blocked",
            ],
            check=False,
        )

        comment = (
            f"**Spinning Issue — Auto-Blocked**\n\n"
            f"PR #{pr_number} went through **{review_cycles}** review cycles without "
            f"converging (judge requests changes → doctor fixes → repeat).\n\n"
            f"The PR has been closed and this issue has been blocked for human review.\n\n"
            f"**Possible causes:**\n"
            f"- Judge standards exceed the doctor's ability to fix\n"
            f"- Fundamental design issue requiring a different approach\n"
            f"- Conflicting or unclear acceptance criteria\n\n"
            f"A maintainer should investigate the review history on PR #{pr_number} "
            f"before re-labeling as `loom:issue`.\n\n"
            f"---\n"
            f"*Auto-blocked by daemon spinning issue detection at {timestamp}*"
        )
        gh_run(
            ["issue", "comment", str(issue), "--body", comment],
            check=False,
        )
        log_info(f"Blocked issue #{issue} (linked to spinning PR #{pr_number})")
        return True
    except Exception:
        log_warning(f"Failed to block issue #{issue}")
        return False
