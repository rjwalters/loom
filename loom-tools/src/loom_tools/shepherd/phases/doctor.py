"""Doctor phase implementation."""

from __future__ import annotations

import subprocess

from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)


class DoctorPhase:
    """Phase 5: Doctor - Address requested changes from Judge."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Doctor phase is only run when changes are requested.

        This is handled by the orchestrator, not skip logic.
        """
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run doctor phase."""
        if ctx.pr_number is None:
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="no PR number available for doctor phase",
                phase_name="doctor",
            )

        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="doctor",
            )

        # Report phase entry
        ctx.report_milestone("phase_entered", phase="doctor")

        # Run doctor worker with retry
        exit_code = run_phase_with_retry(
            ctx,
            role="doctor",
            name=f"doctor-issue-{ctx.config.issue}",
            timeout=ctx.config.doctor_timeout,
            max_retries=ctx.config.stuck_max_retries,
            phase="doctor",
            worktree=ctx.worktree_path,
            pr_number=ctx.pr_number,
            args=str(ctx.pr_number),
        )

        if exit_code == 3:
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected during doctor",
                phase_name="doctor",
            )

        if exit_code == 4:
            # Doctor stuck
            self._mark_issue_blocked(ctx, "doctor_stuck", "agent stuck after retry")
            return PhaseResult(
                status=PhaseStatus.STUCK,
                message="doctor stuck after retry",
                phase_name="doctor",
            )

        # Validate phase
        if not self.validate(ctx):
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="doctor phase validation failed",
                phase_name="doctor",
            )

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="doctor applied fixes",
            phase_name="doctor",
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate doctor phase contract.

        Doctor must re-request review (loom:review-requested on PR).
        Uses validate-phase.sh for comprehensive validation.
        """
        if ctx.pr_number is None:
            return False

        args = [
            "doctor",
            str(ctx.config.issue),
            "--pr",
            str(ctx.pr_number),
            "--task-id",
            ctx.config.task_id,
        ]

        try:
            ctx.run_script("validate-phase.sh", args, check=True)
            return True
        except subprocess.CalledProcessError:
            return False

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

        # Record blocked reason
        ctx.run_script(
            "record-blocked-reason.sh",
            [
                str(ctx.config.issue),
                "--error-class",
                error_class,
                "--phase",
                "doctor",
                "--details",
                details,
            ],
            check=False,
        )

        # Update systematic failure tracking
        ctx.run_script("detect-systematic-failure.sh", ["--update"], check=False)

        # Add comment
        subprocess.run(
            [
                "gh",
                "issue",
                "comment",
                str(ctx.config.issue),
                "--body",
                f"**Shepherd blocked**: Doctor agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)
