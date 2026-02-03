"""Auto-promote proposals in force mode."""

from __future__ import annotations

from typing import TYPE_CHECKING

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_info, log_success, log_warning

if TYPE_CHECKING:
    from loom_tools.daemon_v2.context import DaemonContext


def promote_proposals(ctx: DaemonContext) -> int:
    """Promote proposals to loom:issue in force mode.

    In force mode, proposals (loom:architect, loom:hermit, loom:curated)
    are automatically promoted to loom:issue without human approval.

    Returns the number of proposals promoted.
    """
    if not ctx.config.force_mode:
        log_info("Skipping proposal promotion (not in force mode)")
        return 0

    proposals = ctx.get_promotable_proposals()
    if not proposals:
        log_info("No proposals to promote")
        return 0

    log_info(f"Promoting {len(proposals)} proposal(s) [force-mode]")
    promoted = 0

    for issue_num in proposals:
        if _promote_single_proposal(issue_num):
            promoted += 1

    return promoted


def _promote_single_proposal(issue_num: int) -> bool:
    """Promote a single proposal to loom:issue.

    Returns True if successful.
    """
    # First, get the current labels to determine which to remove
    try:
        result = gh_run([
            "issue", "view", str(issue_num),
            "--json", "labels",
            "--jq", '.labels[].name',
        ])
        current_labels = result.stdout.strip().splitlines() if result.stdout else []
    except Exception as e:
        log_warning(f"Failed to get labels for #{issue_num}: {e}")
        return False

    # Determine which proposal label to remove
    labels_to_remove = []
    for label in current_labels:
        if label in ("loom:architect", "loom:hermit", "loom:curated"):
            labels_to_remove.append(label)

    if not labels_to_remove:
        log_warning(f"Issue #{issue_num} has no proposal labels")
        return False

    # Build the gh command
    try:
        args = ["issue", "edit", str(issue_num)]
        for label in labels_to_remove:
            args.extend(["--remove-label", label])
        args.extend(["--add-label", "loom:issue"])

        gh_run(args)

        # Add a comment noting the auto-promotion
        comment = (
            "## Auto-Promoted [force-mode]\n\n"
            "This proposal was automatically promoted to `loom:issue` by the "
            "Loom daemon running in force mode.\n\n"
            f"**Labels removed**: {', '.join(f'`{l}`' for l in labels_to_remove)}\n"
            "**Label added**: `loom:issue`\n\n"
            "The issue is now available for a shepherd to pick up."
        )
        gh_run(["issue", "comment", str(issue_num), "--body", comment])

        log_success(f"Promoted proposal #{issue_num}")
        return True

    except Exception as e:
        log_warning(f"Failed to promote #{issue_num}: {e}")
        return False
