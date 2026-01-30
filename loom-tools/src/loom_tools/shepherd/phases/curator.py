"""Curator phase implementation."""

from __future__ import annotations

from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.labels import add_issue_label, remove_issue_label
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)


class CuratorPhase:
    """Phase 1: Curator - Enhance issue with implementation guidance."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if curator phase should be skipped.

        Skip if:
        - --from argument skips this phase
        - Issue already has loom:curated label
        """
        # Check --from argument
        if ctx.config.should_skip_phase(Phase.CURATOR):
            return True, f"skipped via --from {ctx.config.start_from.value}"

        # Check if already curated
        if ctx.has_issue_label("loom:curated"):
            return True, "issue already curated"

        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run curator phase."""
        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="curator",
            )

        # Report phase entry
        ctx.report_milestone("phase_entered", phase="curator")

        # Run curator worker with retry
        exit_code = run_phase_with_retry(
            ctx,
            role="curator",
            name=f"curator-issue-{ctx.config.issue}",
            timeout=ctx.config.curator_timeout,
            max_retries=ctx.config.stuck_max_retries,
            phase="curator",
            args=str(ctx.config.issue),
        )

        if exit_code == 3:
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected during curator",
                phase_name="curator",
            )

        if exit_code == 4:
            # Curator stuck - not critical, can proceed
            return PhaseResult(
                status=PhaseStatus.SKIPPED,
                message="curator stuck after retry - skipping curation",
                phase_name="curator",
            )

        # Validate phase
        if not self.validate(ctx):
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="curator phase validation failed",
                phase_name="curator",
            )

        # Belt-and-suspenders: ensure loom:curating is removed
        remove_issue_label(ctx.config.issue, "loom:curating", ctx.repo_root)

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message="curator phase complete",
            phase_name="curator",
        )

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate curator phase contract.

        Curator must add loom:curated label.
        If missing, attempt recovery by adding the label.
        """
        # Refresh cache
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        if ctx.has_issue_label("loom:curated"):
            return True

        # Attempt recovery: apply loom:curated label
        remove_issue_label(ctx.config.issue, "loom:curating", ctx.repo_root)
        if add_issue_label(ctx.config.issue, "loom:curated", ctx.repo_root):
            ctx.report_milestone("heartbeat", action="recovery: applied loom:curated label")
            return True

        return False
