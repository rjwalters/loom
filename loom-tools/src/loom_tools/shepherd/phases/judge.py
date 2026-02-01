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
            #
            # Issue #1998: After Doctor applies fixes, the PR may have
            # loom:review-requested but neither loom:pr nor loom:changes-requested.
            # This is an expected intermediate state - the judge worker just ran
            # but may not have applied its outcome label yet.
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
                diag = self._gather_diagnostics(ctx)
                # Add context about loom:review-requested state (issue #1998)
                if ctx.has_pr_label("loom:review-requested"):
                    diag["intermediate_state"] = "doctor_fixed_awaiting_review"
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

    def _get_log_path(self, ctx: ShepherdContext) -> Path:
        """Return the expected judge worker log file path."""
        paths = LoomPaths(ctx.repo_root)
        return paths.worker_log_file("judge", ctx.config.issue)

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

    def _gather_diagnostics(self, ctx: ShepherdContext) -> dict[str, Any]:
        """Collect diagnostic info when judge validation fails.

        Inspects the judge worker log file, PR review state, and comments
        to provide actionable context about why the judge phase failed.

        Distinguishes between (issue #1960):
        - Agent didn't run at all (no log, no activity)
        - Agent started but didn't complete (loom:reviewing but no outcome)
        - Agent completed review but label failed (comment exists, no label)
        - Agent left no signals (timeout without work)

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
        if log_path.is_file():
            try:
                lines = log_path.read_text().splitlines()
                diag["log_tail"] = lines[-20:] if len(lines) > 20 else lines
                # Add timing info (issue #1960)
                stat = log_path.stat()
                diag["log_mtime"] = stat.st_mtime
                diag["log_size_bytes"] = stat.st_size
                # Calculate approximate session duration from file timestamps
                ctime = stat.st_ctime  # Creation time (when session started)
                mtime = stat.st_mtime  # Modification time (last output)
                diag["session_duration_seconds"] = int(mtime - ctime)
            except OSError:
                diag["log_tail"] = []
        else:
            diag["log_tail"] = []

        # -- PR review state ---------------------------------------------------
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

        # Review state
        if diag["pr_reviews"]:
            review_states = [
                f"{r.get('state', 'unknown')}" for r in diag["pr_reviews"]
            ]
            parts.append(f"reviews=[{', '.join(review_states)}]")
        else:
            parts.append("no GitHub reviews on PR (Loom uses comment + label workflow instead)")

        # Labels
        loom_labels = [
            lbl for lbl in diag["pr_labels"] if lbl.startswith("loom:")
        ]
        if loom_labels:
            parts.append(f"labels=[{', '.join(loom_labels)}]")
        else:
            parts.append("no loom labels on PR")

        # Log tail
        if diag["log_tail"]:
            last_line = diag["log_tail"][-1].strip()
            parts.append(f"last output: {last_line!r}")
        elif diag["log_exists"]:
            parts.append("log file empty")
        else:
            parts.append(f"log file not found ({diag['log_file']})")

        # Session duration (if available)
        if "session_duration_seconds" in diag:
            duration = diag["session_duration_seconds"]
            parts.append(f"session duration: {duration}s")

        # Add failure mode explanation
        failure_mode = diag.get("failure_mode", "unknown")
        mode_explanations = {
            "agent_never_ran": "Judge agent did not start (no log file created)",
            "agent_started_no_meaningful_output": "Judge agent spawned but produced only terminal escape sequences (likely timeout or prompt failure)",
            "agent_started_no_work": "Judge started but did not claim the PR (no loom:reviewing label)",
            "comment_exists_label_missing": "Judge left a comment but failed to apply the label (API failure?)",
            "started_reviewing_incomplete": "Judge claimed PR (loom:reviewing) but did not complete (timeout?)",
            # Issue #1998: Add explanation for Doctor-fixed intermediate state
            "doctor_fixed_awaiting_outcome": (
                "PR has loom:review-requested (Doctor applied fixes) but judge "
                "did not apply outcome label (loom:pr or loom:changes-requested)"
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
