"""Judge phase implementation."""

from __future__ import annotations

import subprocess

from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)


class JudgePhase:
    """Phase 4: Judge - Review PR, approve or request changes."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if judge phase should be skipped.

        Skip if:
        - --from merge (and PR is already approved)
        """
        if ctx.config.should_skip_phase(Phase.JUDGE):
            # Verify PR is approved
            if ctx.pr_number and ctx.has_pr_label("loom:pr"):
                return True, f"skipped via --from {ctx.config.start_from.value}"
            # Can't skip if not approved
            return False, ""

        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run judge phase."""
        # Handle --from skip without approved PR
        if ctx.config.should_skip_phase(Phase.JUDGE):
            if not ctx.pr_number or not ctx.has_pr_label("loom:pr"):
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"cannot skip judge: PR #{ctx.pr_number} is not approved",
                    phase_name="judge",
                )
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message=f"skipped via --from, PR #{ctx.pr_number} already approved",
                phase_name="judge",
            )

        if ctx.pr_number is None:
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="no PR number available for judge phase",
                phase_name="judge",
            )

        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="judge",
            )

        # Report phase entry
        ctx.report_milestone("phase_entered", phase="judge")

        # Run judge worker with retry
        exit_code = run_phase_with_retry(
            ctx,
            role="judge",
            name=f"judge-issue-{ctx.config.issue}",
            timeout=ctx.config.judge_timeout,
            max_retries=ctx.config.stuck_max_retries,
            phase="judge",
            pr_number=ctx.pr_number,
            args=str(ctx.pr_number),
        )

        if exit_code == 3:
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected during judge",
                phase_name="judge",
            )

        if exit_code == 4:
            # Judge stuck
            self._mark_issue_blocked(ctx, "judge_stuck", "agent stuck after retry")
            return PhaseResult(
                status=PhaseStatus.STUCK,
                message="judge stuck after retry",
                phase_name="judge",
            )

        # Validate phase
        if not self.validate(ctx):
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="judge phase validation failed",
                phase_name="judge",
            )

        # Check result
        ctx.label_cache.invalidate_pr(ctx.pr_number)

        if ctx.has_pr_label("loom:pr"):
            return PhaseResult(
                status=PhaseStatus.SUCCESS,
                message=f"PR #{ctx.pr_number} approved by Judge",
                phase_name="judge",
                data={"approved": True},
            )

        if ctx.has_pr_label("loom:changes-requested"):
            return PhaseResult(
                status=PhaseStatus.SUCCESS,
                message=f"Judge requested changes on PR #{ctx.pr_number}",
                phase_name="judge",
                data={"changes_requested": True},
            )

        return PhaseResult(
            status=PhaseStatus.FAILED,
            message=f"unexpected state: PR #{ctx.pr_number} has neither loom:pr nor loom:changes-requested",
            phase_name="judge",
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate judge phase contract.

        Uses validate-phase.sh for comprehensive validation.
        """
        if ctx.pr_number is None:
            return False

        args = [
            "judge",
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
                "judge",
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
                f"**Shepherd blocked**: Judge agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)
