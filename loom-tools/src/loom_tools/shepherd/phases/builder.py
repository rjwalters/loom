"""Builder phase implementation."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import RateLimitError, WorktreeError
from loom_tools.shepherd.labels import (
    add_issue_label,
    get_pr_for_issue,
    remove_issue_label,
)
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)


class BuilderPhase:
    """Phase 3: Builder - Create worktree, implement, create PR."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if builder phase should be skipped.

        Skip if:
        - --from argument skips this phase (requires existing PR)
        - PR already exists for this issue
        """
        # Check --from argument
        if ctx.config.should_skip_phase(Phase.BUILDER):
            # Verify PR exists
            pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
            if pr is None:
                # Can't skip without existing PR - will be handled in run()
                return False, ""
            ctx.pr_number = pr
            return True, f"skipped via --from {ctx.config.start_from.value}"

        # Check for existing PR
        pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if pr is not None:
            ctx.pr_number = pr
            return True, f"PR #{pr} already exists"

        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run builder phase."""
        # Handle --from skip without existing PR
        if ctx.config.should_skip_phase(Phase.BUILDER):
            pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
            if pr is None:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"cannot skip builder: no PR found for issue #{ctx.config.issue}",
                    phase_name="builder",
                )
            ctx.pr_number = pr
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message=f"skipped via --from, using PR #{pr}",
                phase_name="builder",
            )

        # Check for existing PR
        pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if pr is not None:
            ctx.pr_number = pr
            # Report milestone
            ctx.report_milestone("pr_created", pr_number=pr)
            # Ensure issue has loom:building label
            if not ctx.has_issue_label("loom:building"):
                remove_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
                add_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
                ctx.label_cache.invalidate_issue(ctx.config.issue)
            # Create marker if worktree exists
            if ctx.worktree_path and ctx.worktree_path.is_dir():
                self._create_worktree_marker(ctx)
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message=f"PR #{pr} already exists",
                phase_name="builder",
                data={"pr_number": pr},
            )

        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="builder",
            )

        # Check rate limits
        if self._is_rate_limited(ctx):
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"API rate limit exceeded (threshold: {ctx.config.rate_limit_threshold}%)",
                phase_name="builder",
            )

        # Report phase entry
        ctx.report_milestone("phase_entered", phase="builder")

        # Claim the issue
        remove_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
        add_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        # Create worktree
        if ctx.worktree_path and not ctx.worktree_path.is_dir():
            try:
                ctx.run_script(
                    "worktree.sh",
                    [str(ctx.config.issue)],
                    check=True,
                    capture=True,
                )
                ctx.report_milestone("worktree_created", path=str(ctx.worktree_path))
            except subprocess.CalledProcessError:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message="failed to create worktree",
                    phase_name="builder",
                )

        # Create marker to prevent premature cleanup
        self._create_worktree_marker(ctx)

        # Run builder worker with retry
        exit_code = run_phase_with_retry(
            ctx,
            role="builder",
            name=f"builder-issue-{ctx.config.issue}",
            timeout=ctx.config.builder_timeout,
            max_retries=ctx.config.stuck_max_retries,
            phase="builder",
            worktree=ctx.worktree_path,
            args=str(ctx.config.issue),
        )

        if exit_code == 3:
            # Revert claim on shutdown
            remove_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
            add_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
            ctx.label_cache.invalidate_issue(ctx.config.issue)
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected during builder",
                phase_name="builder",
            )

        if exit_code == 4:
            # Builder stuck
            self._mark_issue_blocked(ctx, "builder_stuck", "agent stuck after retry")
            return PhaseResult(
                status=PhaseStatus.STUCK,
                message="builder stuck after retry",
                phase_name="builder",
            )

        # Validate phase
        if not self.validate(ctx):
            # Cleanup stale worktree
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="builder phase validation failed",
                phase_name="builder",
            )

        # Get PR number
        pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if pr is None:
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"could not find PR for issue #{ctx.config.issue}",
                phase_name="builder",
            )

        ctx.pr_number = pr

        # Report PR created
        ctx.report_milestone("pr_created", pr_number=pr)

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message=f"builder phase complete - PR #{pr} created",
            phase_name="builder",
            data={"pr_number": pr},
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate builder phase contract.

        Uses validate-phase.sh for comprehensive validation with recovery.
        """
        args = ["builder", str(ctx.config.issue)]

        if ctx.worktree_path:
            args.extend(["--worktree", str(ctx.worktree_path)])

        args.extend(["--task-id", ctx.config.task_id])

        try:
            ctx.run_script("validate-phase.sh", args, check=True)
            return True
        except subprocess.CalledProcessError:
            return False

    def _is_rate_limited(self, ctx: ShepherdContext) -> bool:
        """Check if Claude API usage is too high."""
        script = ctx.scripts_dir / "check-usage.sh"
        if not script.is_file():
            return False

        try:
            result = ctx.run_script("check-usage.sh", [], check=False)
            if result.returncode != 0 or not result.stdout.strip():
                return False

            data = json.loads(result.stdout)
            session_pct = data.get("session_percent", 0)
            if session_pct is None:
                return False

            return float(session_pct) >= ctx.config.rate_limit_threshold
        except (json.JSONDecodeError, ValueError):
            return False

    def _create_worktree_marker(self, ctx: ShepherdContext) -> None:
        """Create marker file to prevent premature cleanup."""
        if ctx.worktree_path and ctx.worktree_path.is_dir():
            marker_path = ctx.worktree_path / ctx.config.worktree_marker_file
            import datetime

            content = {
                "shepherd_task_id": ctx.config.task_id,
                "issue": ctx.config.issue,
                "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "pid": None,  # Python doesn't have equivalent to $$
            }
            marker_path.write_text(json.dumps(content, indent=2))

    def _remove_worktree_marker(self, ctx: ShepherdContext) -> None:
        """Remove worktree marker file."""
        if ctx.worktree_path:
            marker_path = ctx.worktree_path / ctx.config.worktree_marker_file
            if marker_path.is_file():
                marker_path.unlink()

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
                "builder",
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
                f"**Shepherd blocked**: Builder agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)

    def _cleanup_stale_worktree(self, ctx: ShepherdContext) -> None:
        """Clean up worktree if it has no commits or changes."""
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return

        # Check for commits
        has_commits = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "log", "--oneline", "@{upstream}..HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )

        # Check for changes
        has_changes = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )

        if not has_commits.stdout.strip() and not has_changes.stdout.strip():
            # Get branch name before removal
            branch_result = subprocess.run(
                ["git", "-C", str(ctx.worktree_path), "rev-parse", "--abbrev-ref", "HEAD"],
                capture_output=True,
                text=True,
                check=False,
            )
            branch = branch_result.stdout.strip() if branch_result.returncode == 0 else ""

            # Remove worktree
            subprocess.run(
                ["git", "worktree", "remove", str(ctx.worktree_path), "--force"],
                cwd=ctx.repo_root,
                capture_output=True,
                check=False,
            )

            # Remove empty branch
            if branch and branch != "main":
                subprocess.run(
                    ["git", "-C", str(ctx.repo_root), "branch", "-d", branch],
                    capture_output=True,
                    check=False,
                )
