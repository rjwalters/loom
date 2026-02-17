"""Judge phase implementation."""

from __future__ import annotations

import re
import subprocess
import time
from pathlib import Path
from typing import Any

from loom_tools.common.logging import log_info, log_warning, strip_ansi
from loom_tools.common.paths import LoomPaths
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

# Minimum threshold (in characters) for meaningful agent output.
# If the log file contains fewer non-ANSI characters than this after
# stripping escape sequences, the agent likely didn't produce any
# substantive work (e.g., only terminal control sequences like
# "\x1b[?2026l" for disabling bracketed paste).
# See issue #1978 for details on the failure mode this detects.
MINIMUM_MEANINGFUL_OUTPUT_CHARS = 100

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
    """Phase 4: Judge - Evaluate PR, approve or request changes."""

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

        # Record phase start time for stale log detection (issue #2327).
        # When MCP retry creates a new session, the old log file may persist
        # with content from a much earlier run.  _gather_diagnostics uses
        # this timestamp to flag such stale logs.
        phase_start_time = time.time()

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

        if exit_code == 6:
            # Low output: CLI sessions produced no meaningful output after retries.
            # In force mode, try infrastructure bypass before marking blocked
            # (issue #2402): if CI is green and PR is mergeable, auto-approve.
            if ctx.config.is_force_mode:
                bypass_result = self._try_infrastructure_bypass(
                    ctx, failure_reason="low output (CLI sessions produced no meaningful output)"
                )
                if bypass_result is not None:
                    return bypass_result

            self._mark_issue_blocked(
                ctx, "judge_low_output", "agent low output after retry"
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="judge low output after retry (CLI sessions produced no meaningful output)",
                phase_name="judge",
                data={"low_output": True},
            )

        if exit_code == 7:
            # MCP server failure: Claude CLI exited because MCP server failed to init.
            # In force mode, try infrastructure bypass before marking blocked
            # (issue #2402): if CI is green and PR is mergeable, auto-approve.
            if ctx.config.is_force_mode:
                bypass_result = self._try_infrastructure_bypass(
                    ctx, failure_reason="MCP server failure (MCP failed to initialize)"
                )
                if bypass_result is not None:
                    return bypass_result

            self._mark_issue_blocked(
                ctx, "judge_mcp_failure", "MCP server failure after retry"
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="judge MCP server failure after retry (MCP server failed to initialize)",
                phase_name="judge",
                data={"mcp_failure": True},
            )

        if exit_code == 10:
            # Ghost session: CLI spawned but exited instantly with no work.
            # In force mode, try infrastructure bypass before marking blocked.
            # See issue #2604.
            if ctx.config.is_force_mode:
                bypass_result = self._try_infrastructure_bypass(
                    ctx, failure_reason="ghost session (0s duration, no meaningful output)"
                )
                if bypass_result is not None:
                    return bypass_result

            self._mark_issue_blocked(
                ctx, "judge_ghost_session", "ghost session after retry (0s duration, no output)"
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message="judge ghost session after retry (0s duration, no meaningful output)",
                phase_name="judge",
                data={"ghost_session": True},
            )

        # Invalidate caches BEFORE validation so the first attempt
        # fetches fresh data instead of stale cached labels.
        ctx.label_cache.invalidate_pr(ctx.pr_number)

        # Retry validation with backoff to handle the race condition
        # where the judge applies comment and label in separate API calls
        # (see issue #1764).
        # Use check_only=True for ALL attempts so that _mark_phase_failed
        # is only called after fallback recovery also fails (#2588).
        # Previously the final attempt used check_only=False which posted
        # "Phase contract failed" comments before fallback had a chance
        # to recover (#2558, #2588).
        validated = False
        for attempt in range(VALIDATION_MAX_RETRIES):
            if self.validate(ctx, check_only=True):
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
            #
            # Issue #1998: After Doctor applies fixes, the PR may have
            # loom:review-requested but neither loom:pr nor loom:changes-requested.
            # This is an expected intermediate state - the judge worker just ran
            # but may not have applied its outcome label yet.
            #
            # Issue #2083: When fallback succeeds, return immediately without
            # re-querying GitHub labels. The `gh pr edit` return code is
            # authoritative - if it succeeded, the label was applied. Querying
            # GitHub API immediately after can race with label propagation.
            if ctx.config.is_force_mode and self._try_fallback_approval(ctx):
                # Fallback already applied loom:pr — trust it and return success
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=f"[force-mode] Fallback approval applied to PR #{ctx.pr_number}",
                    phase_name="judge",
                    data={"approved": True, "fallback_used": True},
                )
            elif ctx.config.is_force_mode and self._try_fallback_changes_requested(ctx):
                # Fallback detected changes-requested — route to doctor loop
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=f"[force-mode] Fallback detected changes requested on PR #{ctx.pr_number}",
                    phase_name="judge",
                    data={"changes_requested": True, "fallback_used": True},
                )
            else:
                # All recovery paths exhausted — NOW post the failure
                # comment and apply the failure label (#2588).
                from loom_tools.validate_phase import _mark_phase_failed

                _mark_phase_failed(
                    ctx.config.issue,
                    "judge",
                    f"Judge phase did not produce a review decision on PR #{ctx.pr_number}.",
                    ctx.repo_root,
                    failure_label="loom:failed:judge",
                )
                diag = self._gather_diagnostics(ctx, phase_start_time)
                # Add context about loom:review-requested state (issue #1998)
                if ctx.has_pr_label("loom:review-requested"):
                    diag["intermediate_state"] = "doctor_fixed_awaiting_judging"
                    log_info(
                        f"PR #{ctx.pr_number} has loom:review-requested (Doctor applied fixes) "
                        "but judge did not produce outcome label"
                    )
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"judge phase validation failed: {diag['summary']}"
                    ),
                    phase_name="judge",
                    data=diag,
                )

        # Standard validation passed — check the result labels.
        # Cache was already invalidated above, but invalidate once more
        # to ensure the label checks below reflect the latest state.
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

    def validate(self, ctx: ShepherdContext, *, check_only: bool = False) -> bool:
        """Validate judge phase contract.

        Calls the Python validate_phase module directly.

        Args:
            check_only: If True, skip side-effects (comments, label changes)
                on failure.  Used by the retry loop to avoid posting duplicate
                "Phase contract failed" comments on non-final attempts (#2558).
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
            check_only=check_only,
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
        has_rejection = self._has_rejection_comment(ctx)
        checks_ok = self._pr_checks_passing(ctx)

        if not has_approval:
            log_info("[force-mode] No approval comment found in PR — fallback denied")
            return False

        # Rejection signals override approval signals (issue #2598).
        # When both are present (e.g., a checklist-style review with ✅ for
        # passing items and ❌ for the overall verdict), this is a rejection.
        if has_rejection:
            log_info(
                "[force-mode] Both approval and rejection signals found "
                "— deferring to rejection fallback"
            )
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
        for rejection signals in PR comments. If found, apply the
        loom:changes-requested label and return True so the caller can route
        to the doctor loop.

        This handles the scenario where the judge worker requested changes
        (left a rejection comment) but failed to apply the label due to a
        GitHub API error or timing issue.

        Note: The judge agent uses a comment + label workflow ("judging")
        rather than GitHub's native review API, so only comment-based
        detection is used here.

        Returns:
            True if fallback changes-requested was detected and label applied.
        """
        assert ctx.pr_number is not None

        log_warning(
            f"[force-mode] Approval fallback failed for PR #{ctx.pr_number}, "
            "attempting fallback changes-requested detection"
        )

        has_rejection = self._has_rejection_comment(ctx)

        if not has_rejection:
            log_info(
                "[force-mode] No rejection comment found — fallback denied"
            )
            return False

        log_warning(
            f"[force-mode] Fallback changes-requested: PR #{ctx.pr_number} has "
            "rejection comment — applying loom:changes-requested label"
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

    def _try_infrastructure_bypass(
        self, ctx: ShepherdContext, *, failure_reason: str
    ) -> PhaseResult | None:
        """Attempt infrastructure bypass in force mode (issue #2402).

        When the judge phase fails due to infrastructure issues (low-output,
        MCP failure) rather than code quality concerns, auto-approve the PR if:
        - CI checks are all passing
        - PR is in a mergeable state

        This is a last-resort fallback for force-mode: the judge never ran at
        all, so there are no comments to detect.  The only signals available
        are CI and merge status.

        Posts an audit trail comment clearly indicating this was NOT a code
        review but an infrastructure bypass.

        Args:
            ctx: Shepherd context.
            failure_reason: Human-readable description of the infrastructure
                failure (for the audit trail comment).

        Returns:
            PhaseResult with SUCCESS if bypass was applied, None otherwise.
        """
        assert ctx.pr_number is not None

        log_warning(
            f"[force-mode] Judge infrastructure failure for PR #{ctx.pr_number}, "
            "attempting infrastructure bypass (issue #2402)"
        )

        checks_ok = self._pr_checks_passing(ctx)
        if not checks_ok:
            log_info(
                "[force-mode] Infrastructure bypass denied: "
                "PR checks not passing or not mergeable"
            )
            return None

        # Post audit trail comment before applying label
        comment_body = (
            "\u26a0\ufe0f **Auto-approved (infrastructure bypass)**\n\n"
            f"The judge phase failed due to infrastructure issues: {failure_reason}.\n"
            "Auto-approving because:\n"
            "- \u2705 CI checks pass\n"
            "- \u2705 Merge state is clean\n\n"
            "This PR was **NOT** code-reviewed. "
            "Consider manual review if the changes are significant.\n\n"
            "<!-- loom:infrastructure-bypass -->"
        )

        subprocess.run(
            ["gh", "pr", "comment", str(ctx.pr_number), "--body", comment_body],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        # Apply loom:pr label
        result = subprocess.run(
            [
                "gh", "pr", "edit", str(ctx.pr_number),
                "--add-label", "loom:pr",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            log_warning(
                "[force-mode] Infrastructure bypass: "
                "failed to apply loom:pr label"
            )
            return None

        ctx.label_cache.invalidate_pr(ctx.pr_number)

        log_warning(
            f"[force-mode] Infrastructure bypass applied to PR #{ctx.pr_number} "
            f"(reason: {failure_reason})"
        )

        return PhaseResult(
            status=PhaseStatus.SUCCESS,
            message=(
                f"[force-mode] Infrastructure bypass: PR #{ctx.pr_number} "
                f"auto-approved (judge failed: {failure_reason})"
            ),
            phase_name="judge",
            data={
                "approved": True,
                "infrastructure_bypass": True,
                "bypass_reason": failure_reason,
            },
        )

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

    def _get_log_path(self, ctx: ShepherdContext) -> Path:
        """Return the most recent judge worker log file path.

        When unique session names are used per retry attempt (issue #2639),
        multiple log files may exist (e.g., ``loom-judge-issue-42.log``,
        ``loom-judge-issue-42-a1.log``).  Returns the most recently
        modified one so that diagnostics reflect the latest attempt.
        """
        log_dir = LoomPaths(ctx.repo_root).logs_dir
        base = f"loom-judge-issue-{ctx.config.issue}"
        # Match base name and any attempt suffixes, excluding rotated
        # timestamp logs (e.g., .20260217-095910.log)
        candidates = [
            p for p in log_dir.glob(f"{base}*.log")
            if not any(c.isdigit() and len(c) > 4 for c in p.stem.split(".")[-1:])
        ]
        if candidates:
            # Return the most recently modified log
            return max(candidates, key=lambda p: p.stat().st_mtime)
        # Fallback to the expected base name
        return LoomPaths(ctx.repo_root).worker_log_file("judge", ctx.config.issue)

    def _has_meaningful_output(self, log_path: Path) -> bool:
        """Check if log file contains substantive content beyond control sequences.

        When the judge agent spawns but fails to produce meaningful output,
        the log file may contain only terminal control sequences like
        "\\x1b[?2026l" (disable bracketed paste). This helper detects that
        failure mode by stripping ANSI escape sequences and checking if
        at least MINIMUM_MEANINGFUL_OUTPUT_CHARS of content remain.

        Args:
            log_path: Path to the judge worker log file.

        Returns:
            True if the log contains meaningful content (>= threshold chars
            after stripping ANSI sequences), False otherwise.

        See issue #1978 for details on the failure mode this detects.
        """
        if not log_path.is_file():
            return False
        try:
            content = log_path.read_text()
            # Strip ANSI escape sequences to get actual content
            stripped = strip_ansi(content)
            # Check if remaining content meets minimum threshold
            return len(stripped.strip()) >= MINIMUM_MEANINGFUL_OUTPUT_CHARS
        except OSError:
            return False

    def _gather_diagnostics(
        self, ctx: ShepherdContext, phase_start_time: float = 0.0
    ) -> dict[str, Any]:
        """Collect diagnostic info when judge validation fails.

        Inspects the judge worker log file, PR judging state, and comments
        to provide actionable context about why the judge phase failed.

        Distinguishes between (issue #1960):
        - Agent didn't run at all (no log, no activity)
        - Agent started but didn't complete (loom:reviewing but no outcome)
        - Agent completed judging but label failed (comment exists, no label)
        - Agent left no signals (timeout without work)

        Args:
            ctx: Shepherd context.
            phase_start_time: Unix timestamp of when the current judge phase
                attempt started.  Used to detect stale log files from previous
                runs (issue #2327).

        All commands are best-effort; failures are recorded but never raised.
        """
        diag: dict[str, Any] = {}

        # -- Log file ----------------------------------------------------------
        log_path = self._get_log_path(ctx)
        diag["log_file"] = str(log_path)
        diag["log_exists"] = log_path.is_file()
        # Check for meaningful output (issue #1978) - detects agents that spawn
        # but produce only terminal escape sequences like "\x1b[?2026l"
        diag["has_meaningful_output"] = self._has_meaningful_output(log_path)
        diag["log_is_stale"] = False
        if log_path.is_file():
            try:
                lines = log_path.read_text().splitlines()
                diag["log_tail"] = lines[-20:] if len(lines) > 20 else lines
                # Add timing info (issue #1960)
                stat = log_path.stat()
                diag["log_mtime"] = stat.st_mtime
                diag["log_size_bytes"] = stat.st_size
                # Detect stale log from a previous run (issue #2327).
                # If the log file was created before this phase attempt
                # started, the content belongs to an earlier session and
                # the session duration would be meaningless.
                ctime = stat.st_ctime  # Creation time (when session started)
                mtime = stat.st_mtime  # Modification time (last output)
                if phase_start_time > 0 and ctime < phase_start_time:
                    diag["log_is_stale"] = True
                    log_warning(
                        f"Stale judge log detected: log created at "
                        f"{ctime:.0f}, phase started at "
                        f"{phase_start_time:.0f} (issue #2327)"
                    )
                else:
                    # Only report session duration for current-session logs
                    diag["session_duration_seconds"] = int(mtime - ctime)
            except OSError:
                diag["log_tail"] = []
        else:
            diag["log_tail"] = []

        # -- PR state (labels and any native GitHub reviews) --------------------
        assert ctx.pr_number is not None
        review_result = subprocess.run(
            [
                "gh",
                "pr",
                "view",
                str(ctx.pr_number),
                "--json",
                "reviews,labels",
                "--jq",
                '{reviews: [.reviews[-3:][] | {state: .state, author: .author.login}], labels: [.labels[].name]}',
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        review_data = parse_command_output(review_result)
        if isinstance(review_data, dict):
            diag["pr_reviews"] = review_data.get("reviews", [])
            diag["pr_labels"] = review_data.get("labels", [])
        else:
            diag["pr_reviews"] = []
            diag["pr_labels"] = []

        # -- Determine failure mode (issue #1960, #1998) -------------------------
        # Check for comment signals using the existing helper methods.
        has_reviewing_label = "loom:reviewing" in diag["pr_labels"]
        has_approval_comment = self._has_approval_comment(ctx)
        has_rejection_comment = self._has_rejection_comment(ctx)
        has_outcome_label = any(
            lbl in diag["pr_labels"]
            for lbl in ("loom:pr", "loom:changes-requested")
        )
        # Issue #1998: Check for Doctor-fixed intermediate state
        has_review_requested = "loom:review-requested" in diag["pr_labels"]

        # Categorize the failure to help with debugging
        if not diag["log_exists"]:
            diag["failure_mode"] = "agent_never_ran"
        elif diag["log_is_stale"]:
            # Issue #2327: Log file belongs to a previous run; current
            # session output is unavailable.  This typically happens when
            # MCP retry creates a new tmux session but the old log content
            # persists from a much earlier run.
            diag["failure_mode"] = "stale_log_from_previous_run"
        elif has_review_requested and not has_outcome_label:
            # Issue #1998: Doctor applied fixes, PR has loom:review-requested,
            # but judge hasn't applied outcome label yet. This takes precedence
            # over log-based heuristics since the label is a reliable signal.
            diag["failure_mode"] = "doctor_fixed_awaiting_outcome"
        elif diag["log_exists"] and not diag["has_meaningful_output"]:
            # Issue #1978: Agent spawned but only produced terminal escape sequences
            diag["failure_mode"] = "agent_started_no_meaningful_output"
        elif not has_reviewing_label and not has_outcome_label:
            diag["failure_mode"] = "agent_started_no_work"
        elif has_reviewing_label and not has_outcome_label:
            if has_approval_comment or has_rejection_comment:
                diag["failure_mode"] = "comment_exists_label_missing"
            else:
                diag["failure_mode"] = "started_reviewing_incomplete"
        else:
            diag["failure_mode"] = "unknown"

        diag["has_approval_comment"] = has_approval_comment
        diag["has_rejection_comment"] = has_rejection_comment

        # -- Human-readable summary --------------------------------------------
        parts: list[str] = []

        # Judging state (comment + label signals)
        comment_signals: list[str] = []
        if has_approval_comment:
            comment_signals.append("approval")
        if has_rejection_comment:
            comment_signals.append("rejection")
        if comment_signals:
            parts.append(f"judge comments=[{', '.join(comment_signals)}]")
        else:
            parts.append("no judge comments detected")

        # Labels
        loom_labels = [
            lbl for lbl in diag["pr_labels"] if lbl.startswith("loom:")
        ]
        if loom_labels:
            parts.append(f"labels=[{', '.join(loom_labels)}]")
        else:
            parts.append("no loom labels on PR")

        # Log tail — flag stale output so operators don't misinterpret it
        if diag["log_is_stale"]:
            parts.append("log file is STALE (from a previous run)")
        elif diag["log_tail"]:
            last_line = diag["log_tail"][-1].strip()
            parts.append(f"last output: {last_line!r}")
        elif diag["log_exists"]:
            parts.append("log file empty")
        else:
            parts.append(f"log file not found ({diag['log_file']})")

        # Session duration (if available; omitted for stale logs)
        if "session_duration_seconds" in diag:
            duration = diag["session_duration_seconds"]
            parts.append(f"session duration: {duration}s")

        # Add failure mode explanation
        failure_mode = diag.get("failure_mode", "unknown")
        mode_explanations = {
            "agent_never_ran": "Judge agent did not start (no log file created)",
            "agent_started_no_meaningful_output": "Judge agent spawned but produced only terminal escape sequences (likely timeout or prompt failure)",
            "agent_started_no_work": "Judge started but did not claim the PR (no loom:reviewing label)",
            "comment_exists_label_missing": "Judge left a comment but failed to apply the outcome label (API failure?)",
            "started_reviewing_incomplete": "Judge claimed PR (loom:reviewing) but did not complete judging (timeout?)",
            # Issue #1998: Add explanation for Doctor-fixed intermediate state
            "doctor_fixed_awaiting_outcome": (
                "PR has loom:review-requested (Doctor applied fixes) but judge "
                "did not apply outcome label (loom:pr or loom:changes-requested)"
            ),
            # Issue #2327: Log file is from a previous run
            "stale_log_from_previous_run": (
                "Log file predates current phase attempt — content is from a "
                "previous run (MCP retry likely created a new session but the "
                "old log was not rotated)"
            ),
            "unknown": "Unable to determine failure mode",
        }
        diag["failure_explanation"] = mode_explanations.get(failure_mode, "Unknown failure mode")

        diag["summary"] = "; ".join(parts)
        return diag

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
                "**Shepherd blocked**: Judge agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)
