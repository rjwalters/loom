"""Judge phase implementation."""

from __future__ import annotations

import re
import subprocess
import time

from loom_tools.common.logging import log_info, log_warning
from loom_tools.common.state import parse_command_output
from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.phases.base import (
    PhaseResult,
    PhaseStatus,
    run_phase_with_retry,
)

# Retry settings for post-judge validation.
# The judge worker applies comment and label in two separate API calls;
# validation can race between them (see issue #1764).
VALIDATION_MAX_RETRIES = 3
VALIDATION_RETRY_DELAY_SECONDS = 2

# Patterns that indicate an approval comment from the judge.
# Matched case-insensitively against the full comment body.
# Only standalone approval signals count — negation prefixes are excluded
# in _has_approval_comment() via NEGATIVE_PREFIXES.
APPROVAL_PATTERNS: list[re.Pattern[str]] = [
    re.compile(r"\bapproved?\b", re.IGNORECASE),
    re.compile(r"\blgtm\b", re.IGNORECASE),
    re.compile(r"\bship\s*it\b", re.IGNORECASE),
    re.compile(r"\u2705"),  # ✅
    re.compile(r"\U0001f44d"),  # 👍
]

# Patterns that indicate a rejection / changes-requested comment from the judge.
# Matched case-insensitively against the full comment body.
# Only standalone rejection signals count — negation prefixes are excluded
# in _has_rejection_comment() via NEGATIVE_PREFIXES.
REJECTION_PATTERNS: list[re.Pattern[str]] = [
    re.compile(r"\bchanges\s+requested\b", re.IGNORECASE),
    re.compile(r"\brequest\s+changes\b", re.IGNORECASE),
    re.compile(r"\bneeds?\s+(?:changes|fixes|work)\b", re.IGNORECASE),
    re.compile(r"\u274c"),  # ❌
]

# If an approval pattern match is preceded by one of these prefixes
# (within the same line), the match is considered a false positive.
NEGATIVE_PREFIXES: list[re.Pattern[str]] = [
    re.compile(r"\bnot\s+", re.IGNORECASE),
    re.compile(r"\bnot\b", re.IGNORECASE),
    re.compile(r"\bdon'?t\s+", re.IGNORECASE),
    re.compile(r"\bnever\s+", re.IGNORECASE),
    re.compile(r"\bcan'?t\s+", re.IGNORECASE),
    re.compile(r"\bno\s+", re.IGNORECASE),
]


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

        # Invalidate caches BEFORE validation so the first attempt
        # fetches fresh data instead of stale cached labels.
        ctx.label_cache.invalidate_pr(ctx.pr_number)

        # Retry validation with backoff to handle the race condition
        # where the judge applies comment and label in separate API calls
        # (see issue #1764).
        validated = False
        for attempt in range(VALIDATION_MAX_RETRIES):
            if self.validate(ctx):
                validated = True
                break
            if attempt < VALIDATION_MAX_RETRIES - 1:
                time.sleep(VALIDATION_RETRY_DELAY_SECONDS)
                # Re-invalidate cache before each retry to get fresh data
                ctx.label_cache.invalidate_pr(ctx.pr_number)

        if not validated:
            # In force mode, attempt fallback detection before giving up.
            # First try approval (judge approved but failed to apply loom:pr label),
            # then try changes-requested (judge rejected but failed to apply
            # loom:changes-requested label).
            if ctx.config.is_force_mode and self._try_fallback_approval(ctx):
                validated = True
            elif ctx.config.is_force_mode and self._try_fallback_changes_requested(ctx):
                # Fallback detected changes-requested — route to doctor loop
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=f"[force-mode] Fallback detected changes requested on PR #{ctx.pr_number}",
                    phase_name="judge",
                    data={"changes_requested": True},
                )
            else:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message="judge phase validation failed",
                    phase_name="judge",
                )

        # Check result — cache was already invalidated above, but
        # invalidate once more to ensure the label checks below
        # reflect the latest state.
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

        Calls the Python validate_phase module directly.
        """
        if ctx.pr_number is None:
            return False

        from loom_tools.validate_phase import validate_phase

        result = validate_phase(
            phase="judge",
            issue=ctx.config.issue,
            repo_root=ctx.repo_root,
            pr_number=ctx.pr_number,
            task_id=ctx.config.task_id,
        )
        return result.satisfied

    def _try_fallback_approval(self, ctx: ShepherdContext) -> bool:
        """Attempt fallback approval detection in force mode.

        When the standard label-based validation fails in force mode, check
        for approval signals in PR comments combined with healthy PR status
        (CI checks passing and mergeable state). If both conditions are met,
        apply the loom:pr label and return True.

        This handles the scenario where the judge worker approved the PR
        (left an approval comment) but failed to apply the label due to a
        GitHub API error or timing issue.

        Returns:
            True if fallback approval was detected and label applied.
        """
        assert ctx.pr_number is not None

        log_warning(
            f"[force-mode] Label validation failed for PR #{ctx.pr_number}, "
            "attempting fallback approval detection"
        )

        has_approval = self._has_approval_comment(ctx)
        checks_ok = self._pr_checks_passing(ctx)

        if not has_approval:
            log_info("[force-mode] No approval comment found in PR — fallback denied")
            return False

        if not checks_ok:
            log_info("[force-mode] PR checks not passing — fallback denied")
            return False

        # Both signals present — apply the label for consistency
        log_warning(
            f"[force-mode] Fallback approval: PR #{ctx.pr_number} has approval "
            "comment and passing checks — applying loom:pr label"
        )

        result = subprocess.run(
            [
                "gh",
                "pr",
                "edit",
                str(ctx.pr_number),
                "--add-label",
                "loom:pr",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            log_warning("[force-mode] Failed to apply loom:pr label via fallback")
            return False

        ctx.label_cache.invalidate_pr(ctx.pr_number)
        return True

    def _try_fallback_changes_requested(self, ctx: ShepherdContext) -> bool:
        """Attempt fallback changes-requested detection in force mode.

        When the standard label-based validation fails in force mode, check
        for rejection signals in PR comments or a CHANGES_REQUESTED review
        state. If either condition is met, apply the loom:changes-requested
        label and return True so the caller can route to the doctor loop.

        This handles the scenario where the judge worker requested changes
        but failed to apply the label due to a GitHub API error or timing
        issue.

        Returns:
            True if fallback changes-requested was detected and label applied.
        """
        assert ctx.pr_number is not None

        log_warning(
            f"[force-mode] Approval fallback failed for PR #{ctx.pr_number}, "
            "attempting fallback changes-requested detection"
        )

        has_rejection = self._has_rejection_comment(ctx)
        has_cr_review = self._has_changes_requested_review(ctx)

        if not has_rejection and not has_cr_review:
            log_info(
                "[force-mode] No rejection comment or CHANGES_REQUESTED review "
                "found — fallback denied"
            )
            return False

        signal = (
            "rejection comment" if has_rejection else "CHANGES_REQUESTED review state"
        )
        log_warning(
            f"[force-mode] Fallback changes-requested: PR #{ctx.pr_number} has "
            f"{signal} — applying loom:changes-requested label"
        )

        result = subprocess.run(
            [
                "gh",
                "pr",
                "edit",
                str(ctx.pr_number),
                "--add-label",
                "loom:changes-requested",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            log_warning(
                "[force-mode] Failed to apply loom:changes-requested label via fallback"
            )
            return False

        ctx.label_cache.invalidate_pr(ctx.pr_number)
        return True

    def _has_approval_comment(self, ctx: ShepherdContext) -> bool:
        """Check PR comments for approval signals.

        Searches the most recent comments for patterns indicating the judge
        approved the PR (e.g., "Approved", "LGTM", checkmark emoji).
        Rejects false positives where the match is preceded by a negation
        (e.g., "Not approved").

        Returns:
            True if an approval comment was found.
        """
        assert ctx.pr_number is not None

        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "comments",
                "--jq",
                ".comments[-5:][].body",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0 or not result.stdout.strip():
            return False

        comment_text = result.stdout

        for line in comment_text.splitlines():
            for pattern in APPROVAL_PATTERNS:
                match = pattern.search(line)
                if match is None:
                    continue
                # Check for negation prefix on the same line
                prefix_text = line[: match.start()]
                if any(neg.search(prefix_text) for neg in NEGATIVE_PREFIXES):
                    continue
                return True

        return False

    def _has_rejection_comment(self, ctx: ShepherdContext) -> bool:
        """Check PR comments for rejection / changes-requested signals.

        Searches the most recent comments for patterns indicating the judge
        requested changes (e.g., "changes requested", "needs fixes", ❌ emoji).
        Rejects false positives where the match is preceded by a negation
        (e.g., "no changes requested").

        Returns:
            True if a rejection comment was found.
        """
        assert ctx.pr_number is not None

        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "comments",
                "--jq",
                ".comments[-5:][].body",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode != 0 or not result.stdout.strip():
            return False

        comment_text = result.stdout

        for line in comment_text.splitlines():
            for pattern in REJECTION_PATTERNS:
                match = pattern.search(line)
                if match is None:
                    continue
                # Check for negation prefix on the same line
                prefix_text = line[: match.start()]
                if any(neg.search(prefix_text) for neg in NEGATIVE_PREFIXES):
                    continue
                return True

        return False

    def _has_changes_requested_review(self, ctx: ShepherdContext) -> bool:
        """Check if PR has a CHANGES_REQUESTED review state.

        Uses ``gh pr view --json reviews`` to inspect review states.

        Returns:
            True if any review has a CHANGES_REQUESTED state.
        """
        assert ctx.pr_number is not None

        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "reviews",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

        data = parse_command_output(result)
        if not isinstance(data, dict):
            return False

        reviews = data.get("reviews", [])
        return any(r.get("state") == "CHANGES_REQUESTED" for r in reviews)

    def _pr_checks_passing(self, ctx: ShepherdContext) -> bool:
        """Check if PR status checks are passing and PR is mergeable.

        Uses ``gh pr view`` to inspect the overall status check rollup
        and the mergeable state.

        Returns:
            True if checks are passing (or no checks configured) and
            the PR is in a mergeable state.
        """
        assert ctx.pr_number is not None

        result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "statusCheckRollup,mergeable",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

        data = parse_command_output(result)
        if not isinstance(data, dict):
            return False

        # Check mergeable state
        mergeable = data.get("mergeable", "")
        if mergeable not in ("MERGEABLE", "UNKNOWN"):
            # CONFLICTING or other non-mergeable states
            return False

        # Check status checks
        checks = data.get("statusCheckRollup", [])
        if not checks:
            # No checks configured — treat as passing
            return True

        for check in checks:
            conclusion = check.get("conclusion", "")
            status = check.get("status", "")
            # A check is OK if it has concluded successfully or is still pending
            if conclusion in ("SUCCESS", "NEUTRAL", "SKIPPED", ""):
                continue
            if status == "IN_PROGRESS":
                continue
            # Any failure/error means checks aren't passing
            return False

        return True

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
            phase="judge",
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
                f"**Shepherd blocked**: Judge agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)
