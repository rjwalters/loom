"""Merge phase implementation."""

from __future__ import annotations

import subprocess

from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import PhaseResult, PhaseStatus


class MergePhase:
    """Phase 6: Merge Gate - Auto-merge (force mode) or exit at loom:pr."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Merge phase never skips via --from."""
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run merge phase."""
        if ctx.pr_number is None:
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="no PR number available for merge phase",
                phase_name="merge",
            )

        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="merge",
            )

        # In force mode, auto-merge
        if ctx.config.is_force_mode:
            # Merge via merge-pr.sh
            try:
                ctx.run_script(
                    "merge-pr.sh",
                    [str(ctx.pr_number), "--cleanup-worktree"],
                    check=True,
                )
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=f"PR #{ctx.pr_number} merged successfully",
                    phase_name="merge",
                    data={"merged": True},
                )
            except FileNotFoundError as exc:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=str(exc),
                    phase_name="merge",
                )
            except subprocess.CalledProcessError:
                self._mark_issue_blocked(
                    ctx, "merge_failed", f"failed to merge PR #{ctx.pr_number}"
                )
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"failed to merge PR #{ctx.pr_number}",
                    phase_name="merge",
                )

        # Default mode: exit at loom:pr state (Champion handles merge)
        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message=f"PR #{ctx.pr_number} approved, ready for Champion to merge",
            phase_name="merge",
            data={"awaiting_merge": True},
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate merge phase contract.

        In force mode, PR should be merged.
        In default mode, PR should have loom:pr label.
        """
        if ctx.pr_number is None:
            return False

        if ctx.config.is_force_mode:
            # Check PR is merged
            result = subprocess.run(
                [
                    "gh",
                    "pr",
                    "view",
                    str(ctx.pr_number),
                    "--json",
                    "state",
                    "--jq",
                    ".state",
                ],
                cwd=ctx.repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            return result.stdout.strip() == "MERGED"

        # Default mode: PR should have loom:pr label
        ctx.label_cache.invalidate_pr(ctx.pr_number)
        return ctx.has_pr_label("loom:pr")

    def _mark_issue_blocked(
        self, ctx: ShepherdContext, error_class: str, details: str
    ) -> None:
        """Mark issue as blocked with diagnostic info."""
        # Atomic transition: loom:building -> loom:blocked
        subprocess.run(
            [
                "gh",
                "issue",
                "edit",
                str(ctx.config.issue),
                "--remove-label",
                "loom:building",
                "--add-label",
                "loom:blocked",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        # Record blocked reason and update systematic failure tracking
        from loom_tools.common.systematic_failure import (
            detect_systematic_failure,
            record_blocked_reason,
        )

        record_blocked_reason(
            ctx.repo_root,
            ctx.config.issue,
            error_class=error_class,
            phase="merge",
            details=details,
        )
        detect_systematic_failure(ctx.repo_root)

        # Add comment
        subprocess.run(
            [
                "gh",
                "issue",
                "comment",
                str(ctx.config.issue),
                "--body",
                f"**Shepherd blocked**: Failed to merge PR #{ctx.pr_number}. Branch may be out of date or have merge conflicts.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)
