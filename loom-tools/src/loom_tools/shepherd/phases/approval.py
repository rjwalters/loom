"""Approval gate phase implementation."""

from __future__ import annotations

import time

from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.labels import add_issue_label
from loom_tools.shepherd.phases.base import PhaseResult, PhaseStatus


class ApprovalPhase:
    """Phase 2: Approval Gate - Wait for loom:issue label."""

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Approval gate never skips - always check approval status."""
        return False, ""

    def run(self, ctx: ShepherdContext) -> PhaseResult:
        """Run approval gate phase."""
        # Check for shutdown
        if ctx.check_shutdown():
            return PhaseResult(
                status=PhaseStatus.SHUTDOWN,
                message="shutdown signal detected",
                phase_name="approval",
            )

        # Check if already approved
        if ctx.has_issue_label("loom:issue"):
            return PhaseResult(
                status=PhaseStatus.SUCCESS,
                message="issue already approved (has loom:issue label)",
                phase_name="approval",
                data={"summary": "already approved"},
            )

        # Check if pre-approved by daemon (daemon claims issue by swapping
        # loom:issue -> loom:building before spawning shepherd)
        if ctx.has_issue_label("loom:building"):
            return PhaseResult(
                status=PhaseStatus.SUCCESS,
                message="issue pre-approved (claimed by daemon, has loom:building label)",
                phase_name="approval",
                data={"summary": "daemon-claimed", "method": "building-label"},
            )

        # In default or force mode, auto-approve past the approval gate
        if ctx.config.should_auto_approve:
            add_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
            ctx.label_cache.invalidate_issue(ctx.config.issue)
            return PhaseResult(
                status=PhaseStatus.SUCCESS,
                message="issue auto-approved",
                phase_name="approval",
                data={"summary": "auto-approved"},
            )

        # Wait for human approval
        start_time = time.time()
        while True:
            # Check timeout
            elapsed = time.time() - start_time
            if elapsed > ctx.config.approval_timeout:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"approval timed out after {int(elapsed)}s",
                    phase_name="approval",
                )

            # Refresh cache and check
            ctx.label_cache.invalidate_issue(ctx.config.issue)
            if ctx.has_issue_label("loom:issue"):
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message="issue approved by human",
                    phase_name="approval",
                    data={"summary": "human approved"},
                )

            # Check for shutdown
            if ctx.check_shutdown():
                return PhaseResult(
                    status=PhaseStatus.SHUTDOWN,
                    message="shutdown signal detected during approval wait",
                    phase_name="approval",
                )

            # Report heartbeat so daemon knows we're waiting, not stuck
            ctx.report_milestone("heartbeat", action="waiting for approval")

            time.sleep(ctx.config.poll_interval)

    def validate(self, ctx: ShepherdContext) -> bool:
        """Validate approval phase contract.

        Issue must have loom:issue or loom:building label.
        """
        ctx.label_cache.invalidate_issue(ctx.config.issue)
        return ctx.has_issue_label("loom:issue") or ctx.has_issue_label("loom:building")
