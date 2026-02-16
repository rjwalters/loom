"""Doctor phase implementation."""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Any

from loom_tools.common.logging import log_info, log_warning
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)

# Default timeout for waiting for CI to complete (5 minutes)
DEFAULT_CI_TIMEOUT_SECONDS = 300
# Poll interval when waiting for CI
CI_POLL_INTERVAL_SECONDS = 15


class CIStatus(Enum):
    """Status of CI checks on a PR."""

    PASSED = "passed"  # All checks passed
    FAILED = "failed"  # At least one check failed
    PENDING = "pending"  # Checks are still running
    UNKNOWN = "unknown"  # Could not determine status (e.g., API error)


@dataclass
class CIResult:
    """Result of checking CI status."""

    status: CIStatus
    message: str
    checks_total: int = 0
    checks_passed: int = 0
    checks_failed: int = 0
    checks_pending: int = 0

    @property
    def is_complete(self) -> bool:
        """True if CI is no longer running (passed, failed, or unknown)."""
        return self.status in (CIStatus.PASSED, CIStatus.FAILED, CIStatus.UNKNOWN)

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for logging/debugging."""
        return {
            "status": self.status.value,
            "message": self.message,
            "checks_total": self.checks_total,
            "checks_passed": self.checks_passed,
            "checks_failed": self.checks_failed,
            "checks_pending": self.checks_pending,
        }


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

        # If doctor made commits, wait for CI to complete before proceeding
        # This prevents validation from failing due to CI still running
        if diagnostics.made_progress:
            ci_result = self._wait_for_ci(ctx)
            diagnostics_dict = diagnostics.to_dict()
            diagnostics_dict["ci_status"] = ci_result.to_dict()

            if ci_result.status == CIStatus.FAILED:
                # CI failed after doctor's changes - this is a real failure
                log_warning(f"[doctor] CI failed after doctor commits: {ci_result.message}")
                diagnostics.failure_mode = DoctorFailureMode.INSUFFICIENT_CHANGES
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"CI failed after doctor fixes: {ci_result.message}",
                    phase_name="doctor",
                    data=diagnostics_dict,
                )

            if ci_result.status == CIStatus.PENDING:
                # CI timed out - report as warning but continue to validation
                # The Judge phase will also check CI status
                log_warning(
                    f"[doctor] CI still pending after timeout, proceeding with validation"
                )
                ctx.report_milestone(
                    "heartbeat",
                    action="CI still pending after timeout, proceeding with validation",
                )

            # Check for shutdown during CI wait
            if ci_result.status == CIStatus.UNKNOWN and "Shutdown" in ci_result.message:
                return PhaseResult(
                    status=PhaseStatus.SHUTDOWN,
                    message="shutdown signal received while waiting for CI",
                    phase_name="doctor",
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

    def _get_ci_status(self, ctx: ShepherdContext) -> CIResult:
        """Get the current CI status for the PR.

        Uses ``gh pr view`` to inspect the status check rollup and determine
        if checks have passed, failed, or are still pending.

        Args:
            ctx: Shepherd context with pr_number set

        Returns:
            CIResult with current status and check counts
        """
        if ctx.pr_number is None:
            return CIResult(
                status=CIStatus.UNKNOWN,
                message="No PR number available",
            )

        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "statusCheckRollup",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0:
            return CIResult(
                status=CIStatus.UNKNOWN,
                message=f"Failed to get PR status: {result.stderr.strip()}",
            )

        try:
            import json

            data = json.loads(result.stdout)
        except (json.JSONDecodeError, ValueError) as e:
            return CIResult(
                status=CIStatus.UNKNOWN,
                message=f"Failed to parse PR status: {e}",
            )

        checks = data.get("statusCheckRollup", [])
        if not checks:
            # No checks configured - treat as passed
            return CIResult(
                status=CIStatus.PASSED,
                message="No CI checks configured",
                checks_total=0,
            )

        # Count check states
        passed = 0
        failed = 0
        pending = 0

        for check in checks:
            conclusion = check.get("conclusion", "")
            status = check.get("status", "")

            if conclusion in ("SUCCESS", "NEUTRAL", "SKIPPED"):
                passed += 1
            elif conclusion in ("FAILURE", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED"):
                failed += 1
            elif status in ("IN_PROGRESS", "QUEUED", "PENDING", "WAITING"):
                pending += 1
            elif conclusion == "":
                # Empty conclusion with no status usually means pending
                pending += 1
            else:
                # Unknown state - treat as failed to be safe
                failed += 1

        total = passed + failed + pending

        if failed > 0:
            return CIResult(
                status=CIStatus.FAILED,
                message=f"CI failed: {failed}/{total} checks failed",
                checks_total=total,
                checks_passed=passed,
                checks_failed=failed,
                checks_pending=pending,
            )

        if pending > 0:
            return CIResult(
                status=CIStatus.PENDING,
                message=f"CI pending: {pending}/{total} checks still running",
                checks_total=total,
                checks_passed=passed,
                checks_failed=failed,
                checks_pending=pending,
            )

        return CIResult(
            status=CIStatus.PASSED,
            message=f"CI passed: {passed}/{total} checks succeeded",
            checks_total=total,
            checks_passed=passed,
            checks_failed=failed,
            checks_pending=pending,
        )

    def _wait_for_ci(
        self,
        ctx: ShepherdContext,
        timeout_seconds: int = DEFAULT_CI_TIMEOUT_SECONDS,
    ) -> CIResult:
        """Wait for CI checks to complete on the PR.

        Polls the PR status at regular intervals until CI completes or times out.
        Reports progress via heartbeat milestones so the operator can see status.

        Args:
            ctx: Shepherd context with pr_number set
            timeout_seconds: Maximum time to wait for CI (default 5 minutes)

        Returns:
            CIResult with final status:
            - PASSED: All checks passed
            - FAILED: At least one check failed
            - PENDING: Timeout reached while checks still running
            - UNKNOWN: Error fetching status
        """
        start_time = time.time()
        last_status_message = ""

        log_info(f"[doctor] Waiting for CI checks on PR #{ctx.pr_number}")
        ctx.report_milestone(
            "heartbeat",
            action=f"waiting for CI checks on PR #{ctx.pr_number}",
        )

        while True:
            ci_result = self._get_ci_status(ctx)

            # Log status changes
            if ci_result.message != last_status_message:
                log_info(f"[doctor] CI status: {ci_result.message}")
                last_status_message = ci_result.message

            # If CI is complete (passed, failed, or error), return immediately
            if ci_result.is_complete:
                log_info(f"[doctor] CI complete: {ci_result.status.value}")
                return ci_result

            # Check timeout
            elapsed = time.time() - start_time
            if elapsed >= timeout_seconds:
                log_warning(
                    f"[doctor] CI timeout after {int(elapsed)}s - "
                    f"{ci_result.checks_pending} checks still pending"
                )
                ctx.report_milestone(
                    "heartbeat",
                    action=f"CI timeout: {ci_result.checks_pending} checks still pending",
                )
                # Return pending status to indicate timeout, not failure
                return CIResult(
                    status=CIStatus.PENDING,
                    message=f"CI timeout after {int(elapsed)}s: {ci_result.checks_pending} checks still running",
                    checks_total=ci_result.checks_total,
                    checks_passed=ci_result.checks_passed,
                    checks_failed=ci_result.checks_failed,
                    checks_pending=ci_result.checks_pending,
                )

            # Check for shutdown signal
            if ctx.check_shutdown():
                log_info("[doctor] Shutdown signal detected while waiting for CI")
                return CIResult(
                    status=CIStatus.UNKNOWN,
                    message="Shutdown signal received",
                )

            # Report periodic progress
            if int(elapsed) % 60 == 0 and elapsed > 0:
                ctx.report_milestone(
                    "heartbeat",
                    action=f"waiting for CI: {ci_result.checks_pending} checks pending ({int(elapsed)}s elapsed)",
                )

            # Wait before next poll
            time.sleep(CI_POLL_INTERVAL_SECONDS)

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

        # Run doctor worker with retry (shorter timeout for focused test-fix)
        exit_code = run_phase_with_retry(
            ctx,
            role="doctor",
            name=f"doctor-test-fix-{ctx.config.issue}",
            timeout=ctx.config.doctor_test_fix_timeout,
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
            # Doctor stuck - check if it made commits before getting stuck
            diagnostics = self._diagnose_doctor_outcome(ctx, commits_before)
            if diagnostics.made_progress:
                log_info(
                    "[doctor-test-fix] subprocess stuck but commits detected"
                    " - treating as success"
                )
                ctx.report_milestone(
                    "heartbeat",
                    action="doctor-test-fix stuck but committed fix, treating as success",
                )
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message="doctor applied test fixes (subprocess hung after commit)",
                    phase_name="doctor",
                    data=diagnostics.to_dict(),
                )
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
            context_file.write_text(json.dumps(context_data, indent=2) + "\n")
            return context_file
        except OSError:
            return None
