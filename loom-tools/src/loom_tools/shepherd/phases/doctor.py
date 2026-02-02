"""Doctor phase implementation."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any

from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)


class DoctorFailureMode(Enum):
    """Classification of doctor phase failure modes.

    These modes help the daemon make smarter retry/escalate decisions:
    - NO_PROGRESS: Doctor made no commits, retry is unlikely to help
    - INSUFFICIENT_CHANGES: Doctor committed but problem persists, may retry
    - VALIDATION_FAILED: Doctor worked but label state is inconsistent
    """

    NO_PROGRESS = "no_progress"  # Doctor made no commits
    INSUFFICIENT_CHANGES = "insufficient_changes"  # Doctor committed but didn't fix
    VALIDATION_FAILED = "validation_failed"  # Label transition incomplete


@dataclass
class DoctorDiagnostics:
    """Diagnostic information from doctor phase execution.

    This provides visibility into what the doctor actually accomplished,
    enabling better retry decisions.
    """

    commits_made: int = 0
    has_uncommitted_changes: bool = False
    pr_labels: list[str] | None = None
    failure_mode: DoctorFailureMode | None = None

    @property
    def made_progress(self) -> bool:
        """True if doctor made any commits."""
        return self.commits_made > 0

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for PhaseResult.data."""
        return {
            "commits_made": self.commits_made,
            "has_uncommitted_changes": self.has_uncommitted_changes,
            "pr_labels": self.pr_labels,
            "failure_mode": self.failure_mode.value if self.failure_mode else None,
        }


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

        # Capture commit count before doctor runs (for progress detection)
        commits_before = self._get_commit_count(ctx)

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

        # Diagnose what doctor accomplished (regardless of exit code)
        diagnostics = self._diagnose_doctor_outcome(ctx, commits_before)

        # Handle non-zero exit codes with diagnostic info
        if exit_code != 0:
            diagnostics.failure_mode = (
                DoctorFailureMode.NO_PROGRESS
                if not diagnostics.made_progress
                else DoctorFailureMode.INSUFFICIENT_CHANGES
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"doctor failed with exit code {exit_code} ({diagnostics.failure_mode.value})",
                phase_name="doctor",
                data=diagnostics.to_dict(),
            )

        # Validate phase - doctor must re-request review
        if not self.validate(ctx):
            # Doctor ran successfully but didn't complete label transition
            diagnostics.failure_mode = DoctorFailureMode.VALIDATION_FAILED

            # If doctor made commits but validation failed, attempt label recovery
            if diagnostics.made_progress:
                self._attempt_label_recovery(ctx, diagnostics)

            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"doctor phase validation failed ({diagnostics.failure_mode.value})",
                phase_name="doctor",
                data=diagnostics.to_dict(),
            )

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="doctor applied fixes",
            phase_name="doctor",
            data=diagnostics.to_dict(),
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

    def _get_commit_count(self, ctx: ShepherdContext) -> int:
        """Get the current commit count in the worktree ahead of main.

        Returns 0 if worktree doesn't exist or git command fails.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return 0

        result = subprocess.run(
            ["git", "rev-list", "--count", "origin/main..HEAD"],
            cwd=ctx.worktree_path,
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0:
            try:
                return int(result.stdout.strip())
            except ValueError:
                return 0
        return 0

    def _diagnose_doctor_outcome(
        self, ctx: ShepherdContext, commits_before: int
    ) -> DoctorDiagnostics:
        """Diagnose what the doctor accomplished after running.

        Checks:
        - How many commits were made
        - Whether there are uncommitted changes
        - Current PR labels

        Args:
            ctx: Shepherd context
            commits_before: Number of commits ahead of main before doctor ran

        Returns:
            DoctorDiagnostics with gathered information
        """
        diagnostics = DoctorDiagnostics()

        # Check commits made
        commits_after = self._get_commit_count(ctx)
        diagnostics.commits_made = max(0, commits_after - commits_before)

        # Check for uncommitted changes in worktree
        if ctx.worktree_path and ctx.worktree_path.is_dir():
            result = subprocess.run(
                ["git", "status", "--porcelain"],
                cwd=ctx.worktree_path,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0 and result.stdout.strip():
                diagnostics.has_uncommitted_changes = True

        # Get current PR labels
        if ctx.pr_number is not None:
            result = subprocess.run(
                [
                    "gh",
                    "pr",
                    "view",
                    str(ctx.pr_number),
                    "--json",
                    "labels",
                    "--jq",
                    ".labels[].name",
                ],
                cwd=ctx.repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                diagnostics.pr_labels = result.stdout.strip().splitlines()

        return diagnostics

    def _attempt_label_recovery(
        self, ctx: ShepherdContext, diagnostics: DoctorDiagnostics
    ) -> bool:
        """Attempt to recover label state when doctor made progress but validation failed.

        When doctor commits changes but fails to transition labels, we try to
        reset to a known state (loom:review-requested) so the Judge can re-evaluate.

        Args:
            ctx: Shepherd context
            diagnostics: Doctor diagnostics with PR label state

        Returns:
            True if recovery was successful
        """
        if ctx.pr_number is None:
            return False

        # Check current label state
        labels = diagnostics.pr_labels or []

        # If PR still has loom:changes-requested but doctor made progress,
        # transition to loom:review-requested for Judge re-evaluation
        if "loom:changes-requested" in labels and "loom:review-requested" not in labels:
            result = subprocess.run(
                [
                    "gh",
                    "pr",
                    "edit",
                    str(ctx.pr_number),
                    "--remove-label",
                    "loom:changes-requested",
                    "--add-label",
                    "loom:review-requested",
                ],
                cwd=ctx.repo_root,
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode == 0:
                ctx.report_milestone(
                    "heartbeat",
                    action=f"label recovery: transitioned PR #{ctx.pr_number} to loom:review-requested",
                )
                ctx.label_cache.invalidate_pr(ctx.pr_number)
                return True

        return False

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
                - FAILED: Doctor could not fix the tests (with failure mode info)
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

        # Capture commit count before doctor runs (for progress detection)
        commits_before = self._get_commit_count(ctx)

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
            # Doctor stuck - diagnose what was accomplished
            diagnostics = self._diagnose_doctor_outcome(ctx, commits_before)
            return PhaseResult(
                status=PhaseStatus.STUCK,
                message="doctor stuck during test-fix after retry",
                phase_name="doctor",
                data=diagnostics.to_dict(),
            )

        if exit_code == 5:
            # Doctor explicitly signaled failures are pre-existing
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message="doctor determined test failures are pre-existing",
                phase_name="doctor",
                data={"preexisting": True},
            )

        # Diagnose what doctor accomplished
        diagnostics = self._diagnose_doctor_outcome(ctx, commits_before)

        if exit_code != 0:
            diagnostics.failure_mode = (
                DoctorFailureMode.NO_PROGRESS
                if not diagnostics.made_progress
                else DoctorFailureMode.INSUFFICIENT_CHANGES
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"doctor test-fix failed with exit code {exit_code} ({diagnostics.failure_mode.value})",
                phase_name="doctor",
                data=diagnostics.to_dict(),
            )

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="doctor applied test fixes",
            phase_name="doctor",
            data=diagnostics.to_dict(),
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
