"""Doctor phase implementation."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

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

        if exit_code == 5:
            # Doctor explicitly signaled failures are pre-existing
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message="doctor determined failures are pre-existing",
                phase_name="doctor",
                data={"preexisting": True},
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
        Calls the Python validate_phase module directly.
        """
        if ctx.pr_number is None:
            return False

        from loom_tools.validate_phase import validate_phase

        result = validate_phase(
            phase="doctor",
            issue=ctx.config.issue,
            repo_root=ctx.repo_root,
            pr_number=ctx.pr_number,
            task_id=ctx.config.task_id,
        )
        return result.satisfied

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
            phase="doctor",
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
                f"**Shepherd blocked**: Doctor agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)

    def run_test_fix(
        self, ctx: ShepherdContext, test_failure_data: dict[str, Any]
    ) -> PhaseResult:
        """Run doctor phase in test-fix mode.

        This is invoked by the orchestrator when builder tests fail. Doctor
        receives the test failure context and attempts to fix the failing tests.

        Args:
            ctx: Shepherd context
            test_failure_data: Test failure information from BuilderPhase including:
                - test_output_tail: Last 10 lines of test output
                - test_summary: Parsed test summary
                - test_command: The test command that was run
                - changed_files: Files the builder modified

        Returns:
            PhaseResult with:
                - SUCCESS: Doctor made commits to fix tests
                - SKIPPED: Doctor determined failures are pre-existing (exit code 5)
                - FAILED: Doctor could not fix the tests
        """
        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="doctor",
            )

        # Report phase entry
        ctx.report_milestone("phase_entered", phase="doctor-test-fix")

        # Build args for Doctor in test-fix mode
        # Format: --test-fix <issue> --context <path>
        context_file = self._write_test_failure_context(ctx, test_failure_data)
        if context_file:
            args = f"--test-fix {ctx.config.issue} --context {context_file}"
        else:
            args = f"--test-fix {ctx.config.issue}"

        # Run doctor worker with retry
        exit_code = run_phase_with_retry(
            ctx,
            role="doctor",
            name=f"doctor-test-fix-{ctx.config.issue}",
            timeout=ctx.config.doctor_timeout,
            max_retries=ctx.config.stuck_max_retries,
            phase="doctor",
            worktree=ctx.worktree_path,
            args=args,
        )

        if exit_code == 3:
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected during doctor test-fix",
                phase_name="doctor",
            )

        if exit_code == 4:
            # Doctor stuck
            return PhaseResult(
                status=PhaseStatus.STUCK,
                message="doctor stuck during test-fix after retry",
                phase_name="doctor",
            )

        if exit_code == 5:
            # Doctor explicitly signaled failures are pre-existing
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message="doctor determined test failures are pre-existing",
                phase_name="doctor",
                data={"preexisting": True},
            )

        if exit_code != 0:
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"doctor test-fix failed with exit code {exit_code}",
                phase_name="doctor",
            )

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="doctor applied test fixes",
            phase_name="doctor",
        )

    def _write_test_failure_context(
        self, ctx: ShepherdContext, test_failure_data: dict[str, Any]
    ) -> Path | None:
        """Write test failure context to a JSON file for Doctor to read.

        Returns the path to the context file, or None if writing failed.
        """
        import json

        if not ctx.worktree_path:
            return None

        context_file = ctx.worktree_path / ".loom-test-failure-context.json"
        context_data = {
            "issue": ctx.config.issue,
            "failure_message": test_failure_data.get("test_failure_message", "test verification failed"),
            "test_command": test_failure_data.get("test_command", ""),
            "test_output_tail": test_failure_data.get("test_output_tail", ""),
            "test_summary": test_failure_data.get("test_summary", ""),
            "changed_files": test_failure_data.get("changed_files", []),
        }

        try:
            context_file.write_text(json.dumps(context_data, indent=2))
            return context_file
        except OSError:
            return None
