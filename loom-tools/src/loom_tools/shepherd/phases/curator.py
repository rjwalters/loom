"""Curator phase implementation."""

from __future__ import annotations

from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.labels import remove_issue_label
from loom_tools.shepherd.phases.base import (
    BasePhase,
    PhaseResult,
    run_phase_with_retry,
)


class CuratorPhase(BasePhase):
    """Phase 1: Curator - Enhance issue with implementation guidance."""

    phase_name = "curator"

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
            return self.shutdown("shutdown signal detected")

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
            return self.shutdown("shutdown signal detected during curator")

        if exit_code == 4:
            # Curator stuck - not critical, can proceed
            return self.skipped("curator stuck after retry - skipping curation")

        if exit_code in (6, 7, 9):
            # All sessions failed to start - curation didn't happen.
            # Curation is optional so skip rather than fail, but message
            # must be honest about what happened.
            reason = {
                6: "all sessions failed to start (instant-exit after retries)",
                7: "all sessions failed to start (MCP server failure after retries)",
                9: "all sessions failed to start (auth pre-flight failure)",
            }[exit_code]
            return self.skipped(f"curator phase skipped: {reason}")

        # Validate phase
        if not self.validate(ctx):
            return self.failed("curator phase validation failed")

        # Belt-and-suspenders: ensure loom:curating is removed
        remove_issue_label(ctx.config.issue, "loom:curating", ctx.repo_root)

        return self.success("curator phase complete")

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate curator phase contract.

        Calls the Python validate_phase module directly for consistent
        validation with recovery across all phases.
        """
        from loom_tools.validate_phase import validate_phase

        result = validate_phase(
            phase="curator",
            issue=ctx.config.issue,
            repo_root=ctx.repo_root,
            task_id=ctx.config.task_id,
        )
        return result.satisfied
