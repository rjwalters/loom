"""Builder phase implementation."""

from __future__ import annotations

import json
import logging
import re
import subprocess
import time
from pathlib import Path
from typing import Any

from loom_tools.common.git import get_changed_files, get_commit_count
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.state import parse_command_output, read_json_file
from loom_tools.common.worktree_safety import is_worktree_safe_to_remove
from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.issue_quality import (
    Severity,
    validate_issue_quality,
    validate_issue_quality_with_gates,
)
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

logger = logging.getLogger(__name__)

# Default timeout for test verification (seconds)
_TEST_VERIFY_TIMEOUT = 300


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

    def run(
        self, ctx: ShepherdContext, *, skip_test_verification: bool = False
    ) -> PhaseResult:
        """Run builder phase.

        Args:
            ctx: Shepherd context
            skip_test_verification: If True, skip running test verification
                after the builder completes. Use this when Phase 3c re-runs
                the builder after Doctor handles pre-existing test failures,
                since re-running test verification would fail again on the
                same pre-existing errors.
        """
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

        # Pre-flight issue quality validation (may block with configured gates)
        quality_result = self._run_quality_validation(ctx)
        if quality_result is not None:
            # Quality validation blocked - revert claim
            remove_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
            add_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
            ctx.label_cache.invalidate_issue(ctx.config.issue)
            return quality_result

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
            except subprocess.CalledProcessError as exc:
                detail = (exc.stderr or exc.stdout or "").strip()
                msg = "failed to create worktree"
                if detail:
                    msg = f"{msg}: {detail}"
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=msg,
                    phase_name="builder",
                    data={"error_detail": detail},
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
                data={"log_file": str(self._get_log_path(ctx))},
            )

        if exit_code not in (0, 3, 4):
            # Unexpected non-zero exit from builder subprocess
            diag = self._gather_diagnostics(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder subprocess exited with code {exit_code}: "
                    f"{diag['summary']}"
                ),
                phase_name="builder",
                data={"exit_code": exit_code, "diagnostics": diag},
            )

        # Run test verification in worktree (unless explicitly skipped)
        if skip_test_verification:
            log_info("Skipping test verification (pre-existing failures already handled)")
        else:
            test_result = self._run_test_verification(ctx)
            if test_result is not None and test_result.status == PhaseStatus.FAILED:
                # Preserve worktree and push branch so Doctor/Builder can fix tests
                self._preserve_on_test_failure(ctx, test_result)
                return test_result

        # Validate phase with completion retry
        # If builder made changes but didn't complete commit/push/PR workflow,
        # spawn a focused completion phase to finish the work.
        completion_attempts = 0
        max_completion_attempts = ctx.config.builder_completion_retries

        while True:
            if self.validate(ctx):
                break  # Validation passed

            diag = self._gather_diagnostics(ctx)

            # Check if this is incomplete work that could be completed
            if (
                completion_attempts < max_completion_attempts
                and self._has_incomplete_work(diag)
            ):
                completion_attempts += 1
                log_warning(
                    f"Builder left incomplete work (attempt {completion_attempts}/{max_completion_attempts}): "
                    f"{diag['summary']}"
                )

                # Run completion phase
                completion_exit = self._run_completion_phase(ctx, diag)

                if completion_exit == 3:
                    # Shutdown during completion
                    return PhaseResult(
                        status=PhaseStatus.SHUTDOWN,
                        message="shutdown signal detected during completion phase",
                        phase_name="builder",
                    )

                if completion_exit == 0:
                    log_info("Completion phase finished, re-validating")
                    continue  # Re-validate

                log_warning(f"Completion phase failed with exit code {completion_exit}")
                # Fall through to failure

            # No incomplete work pattern or retries exhausted
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder phase validation failed: {diag['summary']}"
                ),
                phase_name="builder",
                data={"diagnostics": diag, "completion_attempts": completion_attempts},
            )

        # Get PR number
        pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if pr is None:
            diag = self._gather_diagnostics(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"could not find PR for issue #{ctx.config.issue}: "
                    f"{diag['summary']}"
                ),
                phase_name="builder",
                data={"diagnostics": diag},
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

        Calls the Python validate_phase module directly for comprehensive
        validation with recovery.
        """
        from loom_tools.validate_phase import validate_phase

        result = validate_phase(
            phase="builder",
            issue=ctx.config.issue,
            repo_root=ctx.repo_root,
            worktree=str(ctx.worktree_path) if ctx.worktree_path else None,
            task_id=ctx.config.task_id,
        )
        return result.satisfied

    def _fetch_issue_body(self, ctx: ShepherdContext) -> str | None:
        """Fetch the issue body from GitHub.

        Returns the issue body text, or None if the fetch fails.
        """
        try:
            result = subprocess.run(
                [
                    "gh",
                    "issue",
                    "view",
                    str(ctx.config.issue),
                    "--json",
                    "body",
                    "--jq",
                    ".body",
                ],
                cwd=ctx.repo_root,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode == 0:
                return result.stdout
        except OSError:
            pass
        return None

    def _run_quality_validation(self, ctx: ShepherdContext) -> PhaseResult | None:
        """Run pre-flight quality validation on the issue body.

        Logs findings at configured severity levels. Returns PhaseResult.FAILED
        if any BLOCK-level findings exist, otherwise returns None to continue.

        Returns:
            None if validation passes (or cannot run), PhaseResult if blocked.
        """
        body = self._fetch_issue_body(ctx)
        if body is None:
            return None

        # Use quality gates from config for configurable severity
        result = validate_issue_quality_with_gates(body, ctx.config.quality_gates)

        if not result.findings:
            log_info(f"Issue #{ctx.config.issue} passed pre-flight quality checks")
            return None

        # Log findings at appropriate levels
        for finding in result.findings:
            if finding.severity == Severity.BLOCK:
                log_error(f"Issue #{ctx.config.issue} quality: {finding.message}")
            elif finding.severity == Severity.WARNING:
                log_warning(f"Issue #{ctx.config.issue} quality: {finding.message}")
            else:
                log_info(f"Issue #{ctx.config.issue} quality: {finding.message}")

        ctx.report_milestone(
            "heartbeat",
            action=f"issue quality: {len(result.blocks)} block(s), {len(result.warnings)} warning(s), {len(result.infos)} info(s)",
        )

        # Block if any BLOCK-level findings exist
        if result.has_blocking_findings:
            block_messages = [f.message for f in result.blocks]
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"issue quality check failed: {'; '.join(block_messages)}",
                phase_name="builder",
                data={
                    "quality_blocked": True,
                    "block_findings": block_messages,
                },
            )

        return None

    def _ensure_dependencies(self, worktree: Path) -> bool:
        """Ensure project dependencies are installed in the worktree.

        Checks for package.json with missing node_modules and runs
        ``pnpm install --frozen-lockfile`` when needed.

        Returns True if dependencies are ready (already present or
        successfully installed), False if installation failed.
        Installation failure is non-fatal -- callers should continue
        gracefully.
        """
        pkg_json = worktree / "package.json"
        node_modules = worktree / "node_modules"

        if not pkg_json.is_file() or node_modules.is_dir():
            return True

        log_info("node_modules missing, running pnpm install --frozen-lockfile")
        try:
            result = subprocess.run(
                ["pnpm", "install", "--frozen-lockfile"],
                cwd=worktree,
                text=True,
                capture_output=True,
                timeout=120,
                check=False,
            )
            if result.returncode == 0:
                log_success("Dependencies installed successfully")
                return True
            log_warning(
                f"pnpm install failed (exit code {result.returncode}): "
                f"{(result.stderr or result.stdout or '').strip()[:200]}"
            )
            return False
        except subprocess.TimeoutExpired:
            log_warning("pnpm install timed out after 120s")
            return False
        except OSError as e:
            log_warning(f"Could not run pnpm install: {e}")
            return False

    def _detect_test_command(self, worktree: Path) -> tuple[list[str], str] | None:
        """Detect the appropriate test command for the project in the worktree.

        Returns a tuple of (command_args, display_name) or None if no test runner
        is detected.
        """
        if (worktree / "package.json").is_file():
            pkg = read_json_file(worktree / "package.json")
            if isinstance(pkg, dict):
                scripts = pkg.get("scripts", {})
                # Prefer check:ci:lite > check:ci > test > check
                if "check:ci:lite" in scripts:
                    return (["pnpm", "check:ci:lite"], "pnpm check:ci:lite")
                if "check:ci" in scripts:
                    return (["pnpm", "check:ci"], "pnpm check:ci")
                if "test" in scripts:
                    return (["pnpm", "test"], "pnpm test")
                if "check" in scripts:
                    return (["pnpm", "check"], "pnpm check")

        if (worktree / "Cargo.toml").is_file():
            return (["cargo", "test", "--workspace"], "cargo test --workspace")

        if (worktree / "pyproject.toml").is_file():
            return (["python", "-m", "pytest"], "pytest")

        return None

    def _parse_test_summary(self, output: str) -> str | None:
        """Extract a brief test summary from command output.

        Looks for common test result patterns and returns a compact summary.
        Returns None if no recognizable pattern is found.
        """
        lines = output.strip().splitlines()

        # Search from the end for summary lines (most test runners put summary last)
        for line in reversed(lines):
            stripped = line.strip()
            # Strip leading/trailing decoration characters (pytest uses = borders)
            cleaned = stripped.strip("= ").strip()

            # vitest/jest: "Tests  N passed" or "Test Suites: N passed"
            if "passed" in stripped.lower() and "test" in stripped.lower():
                return stripped

            # cargo test: "test result: ok. N passed; 0 failed"
            if stripped.startswith("test result:"):
                return stripped

            # pytest: "N passed in Xs" or "N passed, N failed" (with = border)
            if cleaned and "passed" in cleaned and ("in " in cleaned or "failed" in cleaned):
                return cleaned

        return None

    def _parse_failure_count(self, output: str) -> int | None:
        """Extract the number of test failures from command output.

        Parses structured summary lines from pytest, cargo test, and
        vitest/jest to extract the failure count. Returns 0 when a
        test summary indicates all tests passed with no failures.
        Returns None if no recognizable pattern is found.

        For multi-command pipeline output (e.g., cargo test followed by
        vitest), scans ALL lines and returns the worst result (highest
        failure count) when multiple test summaries are present.
        """
        lines = output.strip().splitlines()

        failure_counts: list[int] = []
        found_any_summary = False

        for line in lines:
            stripped = line.strip()
            cleaned = stripped.strip("= ").strip()

            # cargo multi-target: "error: 1 target failed:"
            # This appears when one cargo test binary fails but others pass.
            # Treat target-level failures as 1 failure for comparison purposes.
            m = re.match(r"error:\s+(\d+)\s+targets?\s+failed", stripped)
            if m:
                failure_counts.append(int(m.group(1)))
                found_any_summary = True
                continue

            # pytest: "1 failed, 12 passed in 0.03s" or "1 failed"
            m = re.search(r"(\d+)\s+failed", cleaned)
            if m and ("passed" in cleaned or "failed" in cleaned):
                failure_counts.append(int(m.group(1)))
                found_any_summary = True
                continue

            # cargo test: "test result: ok. 14 passed; 0 failed; 0 ignored"
            # or "test result: FAILED. 0 passed; 1 failed; 0 ignored"
            if stripped.startswith("test result:"):
                m = re.search(r"(\d+)\s+failed", stripped)
                if m:
                    failure_counts.append(int(m.group(1)))
                    found_any_summary = True
                    continue

            # vitest/jest: "Tests  2 failed, 3 passed"
            if "test" in stripped.lower() and "failed" in stripped.lower():
                m = re.search(r"(\d+)\s+failed", stripped)
                if m:
                    failure_counts.append(int(m.group(1)))
                    found_any_summary = True
                    continue

            # vitest/jest all-pass: "Tests  N passed" (no "failed" keyword)
            # pytest all-pass: "N passed in Xs"
            # These indicate 0 failures when no "failed" appears in the line.
            if re.search(r"\d+\s+passed", cleaned) and "failed" not in cleaned.lower():
                failure_counts.append(0)
                found_any_summary = True
                continue

        if not found_any_summary:
            return None

        # Return worst result (highest failure count)
        return max(failure_counts)

    def _extract_failing_test_names(self, output: str) -> set[str]:
        """Extract individual failing test names from test output.

        Parses output from common test runners to extract the names of
        failing tests. Used as a secondary comparison when failure counts
        are equal to detect whether different tests are failing.

        Supported formats:
        - pytest: "FAILED tests/test_foo.py::test_bar - AssertionError"
        - cargo test: "test some::path ... FAILED"
        - vitest/jest: "FAIL src/foo.test.ts" at line start
        """
        names: set[str] = set()

        for raw_line in output.splitlines():
            stripped = raw_line.strip()

            # pytest short summary: "FAILED tests/test_foo.py::test_bar - ..."
            m = re.match(r"FAILED\s+(\S+)", stripped)
            if m:
                names.add(m.group(1))
                continue

            # cargo test: "test some::module::test_name ... FAILED"
            m = re.match(r"test\s+(\S+)\s+\.\.\.\s+FAILED", stripped)
            if m:
                names.add(m.group(1))
                continue

            # vitest/jest: "FAIL src/foo.test.ts" (at start of line)
            # but not summary lines like "Tests  2 failed"
            if stripped.startswith("FAIL ") and "/" in stripped:
                name = stripped[5:].strip()
                if name:
                    names.add(name)
                continue

        return names

    def _compare_test_results(
        self, baseline_output: str, worktree_output: str
    ) -> bool | None:
        """Compare baseline and worktree test results using structured parsing.

        Returns:
            None  — structured comparison succeeded, no new failures detected
            True  — structured comparison succeeded, new failures detected
            False — structured parsing failed, caller should use fallback
        """
        baseline_count = self._parse_failure_count(baseline_output)
        worktree_count = self._parse_failure_count(worktree_output)

        # If we can parse both counts, use count-based comparison
        if baseline_count is not None and worktree_count is not None:
            if worktree_count <= baseline_count:
                # Worktree has same or fewer failures — pre-existing
                return None

            # Stage 1.5: Refine with test name comparison
            # When worktree has more failures, check if they're genuinely new
            # tests or just flaky count discrepancies
            baseline_names = self._extract_failing_test_names(baseline_output)
            worktree_names = self._extract_failing_test_names(worktree_output)
            if baseline_names and worktree_names:
                new_failures = worktree_names - baseline_names
                if not new_failures:
                    # Same tests failing — count discrepancy is noise
                    return None
                # Genuinely new test failures detected
                return True

            # Name extraction failed for at least one side — trust counts
            return True

        # If only one side parsed, we can't do structured comparison
        if baseline_count is None and worktree_count is None:
            # Neither parsed — signal fallback
            return False

        # One side parsed, other didn't — can't reliably compare
        return False

    # Patterns for non-deterministic content that should be normalized
    # before line-based comparison to avoid false positives.
    _NORMALIZE_PATTERNS: list[tuple[re.Pattern[str], str]] = [
        # ISO-8601 timestamps: 2026-01-25T10:15:00.123Z or similar
        (re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[\.\d]*Z?"), "<TIMESTAMP>"),
        # Error IDs like ERR-ml3akrjz-zq26l (alphanumeric with dashes)
        (re.compile(r"ERR-[a-z0-9]+-[a-z0-9]+", re.IGNORECASE), "<ERR-ID>"),
        # Generic UUIDs (before hex to avoid partial matches)
        (
            re.compile(
                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                re.IGNORECASE,
            ),
            "<UUID>",
        ),
        # Hex hashes (8+ chars, e.g. git SHAs, 0x-prefixed memory addresses)
        (re.compile(r"(?:0x)?[0-9a-f]{8,}\b", re.IGNORECASE), "<HEX>"),
        # Stack trace line/column numbers: :123:45 or (line 42)
        (re.compile(r":\d+:\d+"), ":<L>:<C>"),
        (re.compile(r"\(line \d+\)"), "(line <N>)"),
        # Timing values: 1.23s, 123ms, (45s), in 2.45s
        (re.compile(r"\b\d+(\.\d+)?\s*(ms|s)\b"), "<TIME>"),
        # Coverage percentages: 85.2%, 100%
        (re.compile(r"\b\d+(\.\d+)?%"), "<PCT>"),
    ]

    def _normalize_error_line(self, line: str) -> str:
        """Normalize non-deterministic content in an error line.

        Replaces timestamps, error IDs, hex hashes, line numbers, timing
        values, coverage percentages, and UUIDs with stable placeholders
        so that the same logical error produces the same normalized string
        across different runs.
        """
        result = line
        for pattern, replacement in self._NORMALIZE_PATTERNS:
            result = pattern.sub(replacement, result)
        return result

    # Patterns for coverage threshold output lines that should be excluded
    # from error-line extraction. These lines contain "ERROR" or "fail" but
    # represent coverage threshold violations, not actual test failures.
    _COVERAGE_EXCLUSION_PATTERNS: list[re.Pattern[str]] = [
        # vitest/istanbul: "ERROR: Coverage for functions (56.83%) does not meet global threshold (75%)"
        re.compile(r"coverage for .+ does not meet", re.IGNORECASE),
        # Generic: "Coverage threshold not met" or "coverage below minimum"
        re.compile(r"coverage\s+threshold", re.IGNORECASE),
        # Jest: "coverage below minimum" or "coverage not met"
        re.compile(r"coverage\s+(below|not met)", re.IGNORECASE),
    ]

    def _is_coverage_line(self, line: str) -> bool:
        """Check if a line is a coverage threshold output line.

        Coverage threshold violations look like errors (contain "ERROR", "fail")
        but represent coverage shortfalls, not actual test failures. Filtering
        these prevents false positives in the error-line diff comparison.
        """
        return any(
            pattern.search(line) for pattern in self._COVERAGE_EXCLUSION_PATTERNS
        )

    def _extract_error_lines(self, output: str) -> list[str]:
        """Extract error-indicator lines from test output for comparison.

        Returns a list of normalized lines that indicate failures (FAIL, ERROR,
        error messages, etc.). Used to diff baseline vs worktree output to
        detect new regressions.

        Lines are normalized to replace non-deterministic content (timestamps,
        error IDs, hex hashes, etc.) with stable placeholders so that the same
        logical error matches across runs.

        Coverage threshold violation lines are excluded since they represent
        coverage shortfalls rather than actual test failures.
        """
        error_indicators = ("fail", "error", "✗", "✕", "×", "not ok")
        lines = []
        for raw_line in output.splitlines():
            stripped = raw_line.strip()
            if not stripped:
                continue
            lower = stripped.lower()
            if any(indicator in lower for indicator in error_indicators):
                if self._is_coverage_line(stripped):
                    continue
                lines.append(self._normalize_error_line(stripped))
        return lines

    def _run_baseline_tests(
        self,
        ctx: ShepherdContext,
        test_cmd: list[str],
        display_name: str,
    ) -> subprocess.CompletedProcess[str] | None:
        """Run tests against main branch (repo root) to establish a baseline.

        Returns the CompletedProcess result from running tests at the repo root,
        or None if the baseline cannot be obtained (timeout, OSError, or no
        repo root available).
        """
        if not ctx.repo_root or not ctx.repo_root.is_dir():
            return None

        # Detect test command at repo root (may differ from worktree)
        baseline_test_info = self._detect_test_command(ctx.repo_root)
        if baseline_test_info is None:
            return None

        baseline_cmd, _ = baseline_test_info
        log_info(f"Running baseline tests on main: {display_name}")

        try:
            return subprocess.run(
                baseline_cmd,
                cwd=ctx.repo_root,
                text=True,
                capture_output=True,
                timeout=_TEST_VERIFY_TIMEOUT,
                check=False,
            )
        except subprocess.TimeoutExpired:
            log_warning("Baseline test run timed out, skipping comparison")
            return None
        except OSError as e:
            log_warning(f"Could not run baseline tests: {e}")
            return None

    def _run_test_verification(self, ctx: ShepherdContext) -> PhaseResult | None:
        """Run test verification in the worktree after builder completes.

        Uses baseline comparison: runs tests against main first, then against
        the worktree. Only fails if the worktree introduces NEW errors not
        present in the baseline.

        Returns None if tests pass or cannot be run (no test runner detected).
        Returns a PhaseResult with FAILED status if tests introduce new failures.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return None

        # Ensure dependencies are installed before running tests
        self._ensure_dependencies(ctx.worktree_path)

        test_info = self._detect_test_command(ctx.worktree_path)
        if test_info is None:
            log_info("No test runner detected in worktree, skipping test verification")
            return None

        test_cmd, display_name = test_info

        # Run baseline tests on main to detect pre-existing failures
        baseline_result = self._run_baseline_tests(ctx, test_cmd, display_name)

        log_info(f"Running tests: {display_name}")
        ctx.report_milestone("heartbeat", action=f"verifying tests: {display_name}")

        test_start = time.time()
        try:
            result = subprocess.run(
                test_cmd,
                cwd=ctx.worktree_path,
                text=True,
                capture_output=True,
                timeout=_TEST_VERIFY_TIMEOUT,
                check=False,
            )
        except subprocess.TimeoutExpired:
            elapsed = int(time.time() - test_start)
            log_error(f"Tests timed out after {elapsed}s ({display_name})")
            ctx.report_milestone(
                "heartbeat",
                action=f"test verification timed out after {elapsed}s",
            )
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"test verification timed out after {elapsed}s ({display_name})",
                phase_name="builder",
                data={"test_failure": True},
            )
        except OSError as e:
            log_warning(f"Could not run tests ({display_name}): {e}")
            return None

        elapsed = int(time.time() - test_start)

        if result.returncode == 0:
            summary = self._parse_test_summary(
                result.stdout + "\n" + result.stderr
            )
            if summary:
                log_success(f"Tests passed ({summary}, {elapsed}s)")
            else:
                log_success(f"Tests passed ({elapsed}s)")
            ctx.report_milestone(
                "heartbeat",
                action=f"tests passed ({elapsed}s)",
            )
            return None

        # Tests failed — check if these are pre-existing failures
        if baseline_result is not None and baseline_result.returncode != 0:
            baseline_output = baseline_result.stdout + "\n" + baseline_result.stderr
            worktree_output = result.stdout + "\n" + result.stderr

            # Primary: structured comparison using parsed failure counts
            structured = self._compare_test_results(baseline_output, worktree_output)

            if structured is None:
                # Structured comparison says no new failures
                worktree_count = self._parse_failure_count(worktree_output)
                summary = self._parse_test_summary(worktree_output)

                if worktree_count == 0:
                    # Exit code non-zero but no test failures — likely coverage/lint issue
                    msg = f"exit code {result.returncode} but 0 test failures"
                    if summary:
                        msg = f"{summary}, exit code {result.returncode}"
                    log_warning(
                        f"Tests passed but process exited non-zero: {msg}"
                    )
                    ctx.report_milestone(
                        "heartbeat",
                        action=f"tests passed with non-zero exit ({elapsed}s)",
                    )
                else:
                    # Actual failures exist but they're pre-existing
                    msg = f"pre-existing on main (exit code {result.returncode})"
                    if summary:
                        msg = f"pre-existing on main ({summary})"
                    log_warning(
                        f"Tests failed but all failures are pre-existing: {msg}"
                    )
                    ctx.report_milestone(
                        "heartbeat",
                        action=f"tests have pre-existing failures ({elapsed}s)",
                    )
                return None

            if structured is True:
                # Structured comparison detected new failures
                log_warning(
                    "Baseline also fails but worktree introduces "
                    "new test failures (higher failure count)"
                )
            else:
                # Structured parsing failed — fall back to line-based comparison
                baseline_errors = set(
                    self._extract_error_lines(baseline_output)
                )
                worktree_errors = set(
                    self._extract_error_lines(worktree_output)
                )
                new_errors = worktree_errors - baseline_errors

                if not new_errors:
                    summary = self._parse_test_summary(worktree_output)

                    if len(worktree_errors) == 0:
                        # No error lines at all — likely coverage/lint issue
                        msg = f"exit code {result.returncode} but no error lines"
                        if summary:
                            msg = f"{summary}, exit code {result.returncode}"
                        log_warning(
                            f"Tests passed but process exited non-zero: {msg}"
                        )
                        ctx.report_milestone(
                            "heartbeat",
                            action=f"tests passed with non-zero exit ({elapsed}s)",
                        )
                    else:
                        # Error lines exist but they're pre-existing
                        msg = f"pre-existing on main (exit code {result.returncode})"
                        if summary:
                            msg = f"pre-existing on main ({summary})"
                        log_warning(
                            f"Tests failed but all failures are pre-existing: {msg}"
                        )
                        ctx.report_milestone(
                            "heartbeat",
                            action=f"tests have pre-existing failures ({elapsed}s)",
                        )
                    return None

                # Exit-code heuristic: when both sides have the same
                # exit code AND the same number of error lines, the
                # "new" lines in the diff are likely false positives
                # from non-deterministic output (timestamps, error IDs,
                # coverage fluctuations, etc.).  Different error-line
                # counts suggest genuinely new errors even with the
                # same exit code.
                if (
                    result.returncode == baseline_result.returncode
                    and len(worktree_errors) == len(baseline_errors)
                ):
                    summary = self._parse_test_summary(worktree_output)

                    if len(worktree_errors) == 0:
                        # No error lines at all — likely coverage/lint issue
                        msg = f"exit code {result.returncode} but no error lines"
                        if summary:
                            msg = f"{summary}, exit code {result.returncode}"
                        log_warning(
                            f"Tests passed but process exited non-zero: {msg}"
                        )
                        ctx.report_milestone(
                            "heartbeat",
                            action=f"tests passed with non-zero exit ({elapsed}s)",
                        )
                    else:
                        msg = f"pre-existing on main (exit code {result.returncode})"
                        if summary:
                            msg = f"pre-existing on main ({summary})"
                        log_warning(
                            f"Tests failed but line diff is likely non-deterministic "
                            f"(same exit code {result.returncode}, "
                            f"same error count {len(worktree_errors)}, "
                            f"{len(new_errors)} diff lines): {msg}"
                        )
                        ctx.report_milestone(
                            "heartbeat",
                            action=f"tests have pre-existing failures ({elapsed}s)",
                        )
                    return None

                log_warning(
                    f"Baseline also fails but worktree introduces "
                    f"{len(new_errors)} new error(s)"
                )

        # Tests failed with new errors (or no baseline available)
        summary = self._parse_test_summary(
            result.stdout + "\n" + result.stderr
        )
        combined = (result.stdout + "\n" + result.stderr).strip()
        tail_lines = combined.splitlines()[-10:]
        tail_text = "\n".join(tail_lines)

        if summary:
            log_error(f"Tests failed ({summary}, {elapsed}s)")
        else:
            log_error(f"Tests failed (exit code {result.returncode}, {elapsed}s)")

        log_info(f"Test output (last 10 lines):\n{tail_text}")
        ctx.report_milestone(
            "heartbeat",
            action=f"test verification failed ({elapsed}s)",
        )

        # Collect changed files for doctor context
        # Use get_changed_files helper which uses origin/main...HEAD to detect
        # both committed and uncommitted changes
        changed_files: list[str] = []
        if ctx.worktree_path:
            changed_files = get_changed_files(cwd=ctx.worktree_path)

        return PhaseResult(
            status=PhaseStatus.FAILED,
            message=f"test verification failed ({display_name}, exit code {result.returncode})",
            phase_name="builder",
            data={
                "test_failure": True,
                "test_output_tail": tail_text,
                "test_summary": summary or "",
                "test_command": display_name,
                "changed_files": changed_files,
            },
        )

    def _is_rate_limited(self, ctx: ShepherdContext) -> bool:
        """Check if Claude API usage is too high."""
        script = ctx.scripts_dir / "check-usage.sh"
        if not script.is_file():
            return False

        try:
            result = ctx.run_script("check-usage.sh", [], check=False)
            data = parse_command_output(result)
            if not isinstance(data, dict):
                return False

            session_pct = data.get("session_percent", 0)
            if session_pct is None:
                return False

            return float(session_pct) >= ctx.config.rate_limit_threshold
        except ValueError:
            return False

    def _get_log_path(self, ctx: ShepherdContext) -> Path:
        """Return the expected builder log file path."""
        paths = LoomPaths(ctx.repo_root)
        return paths.builder_log_file(ctx.config.issue)

    def _gather_diagnostics(self, ctx: ShepherdContext) -> dict[str, Any]:
        """Collect diagnostic info about the builder environment.

        Inspects worktree state, remote branch, issue labels, and the
        builder log file.  The returned dict is safe to include in
        ``PhaseResult.data`` and its ``"summary"`` key provides a
        human-readable string for error messages.

        All git/gh commands are best-effort; failures are recorded but
        never raised.
        """
        diag: dict[str, Any] = {}

        # -- Log file -------------------------------------------------------
        log_path = self._get_log_path(ctx)
        diag["log_file"] = str(log_path)
        diag["log_exists"] = log_path.is_file()
        if log_path.is_file():
            try:
                lines = log_path.read_text().splitlines()
                diag["log_tail"] = lines[-20:] if len(lines) > 20 else lines
            except OSError:
                diag["log_tail"] = []
        else:
            diag["log_tail"] = []

        # -- Worktree state --------------------------------------------------
        wt = ctx.worktree_path
        diag["worktree_exists"] = bool(wt and wt.is_dir())
        if wt and wt.is_dir():
            # Current branch
            branch_res = subprocess.run(
                ["git", "-C", str(wt), "rev-parse", "--abbrev-ref", "HEAD"],
                capture_output=True,
                text=True,
                check=False,
            )
            diag["branch"] = (
                branch_res.stdout.strip() if branch_res.returncode == 0 else None
            )

            # Commits ahead of main
            log_res = subprocess.run(
                ["git", "-C", str(wt), "log", "--oneline", "main..HEAD"],
                capture_output=True,
                text=True,
                check=False,
            )
            commits = (
                log_res.stdout.strip().splitlines()
                if log_res.returncode == 0 and log_res.stdout.strip()
                else []
            )
            diag["commits_ahead"] = len(commits)

            # Uncommitted changes
            status_res = subprocess.run(
                ["git", "-C", str(wt), "status", "--porcelain"],
                capture_output=True,
                text=True,
                check=False,
            )
            diag["has_uncommitted_changes"] = bool(
                status_res.returncode == 0 and status_res.stdout.strip()
            )
        else:
            diag["branch"] = None
            diag["commits_ahead"] = 0
            diag["has_uncommitted_changes"] = False

        # -- Remote branch ---------------------------------------------------
        branch_name = NamingConventions.branch_name(ctx.config.issue)
        ls_res = subprocess.run(
            ["git", "ls-remote", "--heads", "origin", branch_name],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        diag["remote_branch_exists"] = bool(
            ls_res.returncode == 0 and ls_res.stdout.strip()
        )

        # -- Issue labels ----------------------------------------------------
        label_res = subprocess.run(
            [
                "gh",
                "issue",
                "view",
                str(ctx.config.issue),
                "--json",
                "labels",
                "--jq",
                "[.labels[].name] | join(\", \")",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        diag["issue_labels"] = (
            label_res.stdout.strip() if label_res.returncode == 0 else "unknown"
        )

        # -- Human-readable summary -----------------------------------------
        parts: list[str] = []
        if diag["worktree_exists"]:
            parts.append(
                f"worktree exists (branch={diag['branch']}, "
                f"commits_ahead={diag['commits_ahead']}, "
                f"uncommitted={diag['has_uncommitted_changes']})"
            )
        else:
            parts.append("worktree does not exist")
        parts.append(
            f"remote branch {'exists' if diag['remote_branch_exists'] else 'missing'}"
        )
        parts.append(f"labels=[{diag['issue_labels']}]")
        parts.append(f"log={diag['log_file']}")
        diag["summary"] = "; ".join(parts)

        return diag

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

        # Record blocked reason and update systematic failure tracking
        from loom_tools.common.systematic_failure import (
            detect_systematic_failure,
            record_blocked_reason,
        )

        record_blocked_reason(
            ctx.repo_root,
            ctx.config.issue,
            error_class=error_class,
            phase="builder",
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
                f"**Shepherd blocked**: Builder agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)

    def _has_incomplete_work(self, diag: dict[str, Any]) -> bool:
        """Check if diagnostics indicate incomplete work that could be completed.

        Returns True if:
        - Worktree exists
        - Has uncommitted changes OR commits ahead of main
        - Remote branch doesn't exist (work not pushed)

        This pattern suggests the builder made progress but didn't complete
        the commit/push/PR workflow.
        """
        if not diag.get("worktree_exists"):
            return False

        has_work = (
            diag.get("has_uncommitted_changes", False)
            or diag.get("commits_ahead", 0) > 0
        )

        if not has_work:
            return False

        # If remote branch exists, work was pushed - may just need PR
        # If no remote, definitely incomplete
        return True

    def _run_completion_phase(
        self, ctx: ShepherdContext, diag: dict[str, Any]
    ) -> int:
        """Run a focused completion phase to finish incomplete work.

        Spawns a builder session with explicit instructions to complete
        the commit/push/PR workflow based on current worktree state.

        Args:
            ctx: Shepherd context
            diag: Diagnostics from _gather_diagnostics

        Returns:
            Exit code from the completion worker (0=success)
        """
        from loom_tools.shepherd.phases.base import run_worker_phase

        # Build completion instructions based on current state
        instructions: list[str] = []

        if diag.get("has_uncommitted_changes"):
            instructions.append("- Stage and commit all changes")

        if not diag.get("remote_branch_exists"):
            branch = diag.get("branch", f"feature/issue-{ctx.config.issue}")
            instructions.append(f"- Push branch to remote: git push -u origin {branch}")

        instructions.append(
            f"- Create PR with loom:review-requested label using 'Closes #{ctx.config.issue}' in body"
        )
        instructions.append("- Verify PR was created successfully with gh pr view")

        instruction_text = "\n".join(instructions)

        log_info(f"Running completion phase for issue #{ctx.config.issue}")
        log_info(f"Instructions:\n{instruction_text}")

        # Use a special completion prompt as args
        # IMPORTANT: Args must be single-line because they're passed through tmux send-keys.
        # Newlines break shell command parsing (causes "dquote>" prompts).
        # Join instructions with semicolons instead of newlines.
        instruction_oneline = "; ".join(instructions)
        completion_args = (
            f"COMPLETION_MODE: Your previous session ended before completing the workflow. "
            f"You are in worktree .loom/worktrees/issue-{ctx.config.issue} with changes ready. "
            f"Complete these steps: {instruction_oneline}. "
            f"Do NOT implement anything new - just complete the git/PR workflow."
        )

        exit_code = run_worker_phase(
            ctx,
            role="builder",
            name=f"builder-complete-{ctx.config.issue}",
            timeout=300,  # 5 minutes should be enough to commit/push/PR
            phase="builder",
            worktree=ctx.worktree_path,
            args=completion_args,
        )

        return exit_code

    def _cleanup_stale_worktree(self, ctx: ShepherdContext) -> None:
        """Clean up worktree if it has no commits or changes.

        Performs safety checks before removal to prevent destroying active sessions:
        - Checks for .loom-in-use marker (shepherd is actively using worktree)
        - Checks for active processes with CWD in the worktree
        - Checks if worktree is within grace period (default 5 minutes)
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return

        # Safety check: ensure worktree is safe to remove
        safety = is_worktree_safe_to_remove(ctx.worktree_path)
        if not safety.safe_to_remove:
            log_info(f"Worktree cannot be removed: {safety.reason}")
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

    def _cleanup_on_failure(self, ctx: ShepherdContext) -> None:
        """Clean up worktree and revert labels when builder fails.

        This method is called when the builder phase fails in ways other than
        test verification failure (e.g., validation failure without commits).
        For test failures, use ``_preserve_on_test_failure`` instead.

        The cleanup:
        1. Removes the worktree marker
        2. Removes the worktree (if safe)
        3. Deletes the local branch (if empty)
        4. Reverts issue labels: loom:building -> loom:issue
        """
        # Remove worktree marker first
        self._remove_worktree_marker(ctx)

        # Clean up the worktree
        self._cleanup_stale_worktree(ctx)

        # Revert issue label so it can be picked up again
        remove_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
        add_issue_label(ctx.config.issue, "loom:issue", ctx.repo_root)
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        log_info(f"Cleaned up worktree and reverted labels for issue #{ctx.config.issue}")

    def _push_branch(self, ctx: ShepherdContext) -> bool:
        """Push the current branch to remote.

        Returns True if the push succeeded or the branch was already pushed.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return False

        branch_name = NamingConventions.branch_name(ctx.config.issue)

        # Push branch to remote (create upstream tracking)
        result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "push", "-u", "origin", branch_name],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0:
            log_info(f"Pushed branch {branch_name} to remote")
            return True

        # May already be pushed or have no commits
        log_warning(
            f"Could not push branch {branch_name}: "
            f"{(result.stderr or result.stdout or '').strip()[:200]}"
        )
        return False

    def _preserve_on_test_failure(
        self, ctx: ShepherdContext, test_result: PhaseResult
    ) -> None:
        """Preserve worktree and branch when builder tests fail.

        Instead of cleaning up, this method:
        1. Keeps the worktree intact (with marker for protection)
        2. Pushes existing commits to remote
        3. Labels the issue ``loom:needs-fix`` so Doctor/Builder can continue
        4. Writes test failure context to a file for Doctor to read
        5. Adds a comment with test failure context
        """
        # Push whatever commits exist to remote
        self._push_branch(ctx)

        # Transition label: loom:building -> loom:needs-fix
        remove_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
        add_issue_label(ctx.config.issue, "loom:needs-fix", ctx.repo_root)
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        # Write test failure context file for Doctor phase
        failure_msg = test_result.message or "test verification failed"
        if ctx.worktree_path:
            context_data = {
                "issue": ctx.config.issue,
                "failure_message": failure_msg,
                "test_command": test_result.data.get("test_command", ""),
                "test_output_tail": test_result.data.get("test_output_tail", ""),
                "test_summary": test_result.data.get("test_summary", ""),
                "changed_files": test_result.data.get("changed_files", []),
            }
            context_file = ctx.worktree_path / ".loom-test-failure-context.json"
            try:
                context_file.write_text(json.dumps(context_data, indent=2))
                log_info(f"Wrote test failure context to {context_file}")
            except OSError as e:
                log_warning(f"Could not write test failure context: {e}")

        # Add comment with failure context
        branch_name = NamingConventions.branch_name(ctx.config.issue)
        worktree_rel = (
            f".loom/worktrees/issue-{ctx.config.issue}"
            if ctx.worktree_path
            else "unknown"
        )

        comment = (
            f"**Shepherd**: Builder test verification failed. "
            f"Worktree and branch preserved for Doctor/Builder to fix.\n\n"
            f"- **Failure**: {failure_msg}\n"
            f"- **Branch**: `{branch_name}`\n"
            f"- **Worktree**: `{worktree_rel}`\n\n"
            f"The Doctor or a subsequent Builder can pick this up and fix "
            f"the failing tests without starting from scratch."
        )
        subprocess.run(
            [
                "gh",
                "issue",
                "comment",
                str(ctx.config.issue),
                "--body",
                comment,
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.report_milestone(
            "blocked",
            reason="test_failure",
            details=failure_msg,
        )

        log_info(
            f"Preserved worktree for issue #{ctx.config.issue} "
            f"(test failure, labeled loom:needs-fix)"
        )

    # Maps file extension → set of test ecosystems that extension can affect.
    # Used by _should_skip_doctor_recovery() to determine if builder changes
    # could plausibly cause failures in the failing test ecosystem.
    _EXT_TO_ECOSYSTEM: dict[str, set[str]] = {
        # Rust
        ".rs": {"cargo"},
        ".toml": {"cargo", "pnpm"},  # Cargo.toml + possible build scripts
        # TypeScript/JavaScript
        ".ts": {"pnpm"},
        ".tsx": {"pnpm"},
        ".js": {"pnpm"},
        ".jsx": {"pnpm"},
        ".mjs": {"pnpm"},
        ".cjs": {"pnpm"},
        ".css": {"pnpm"},
        ".scss": {"pnpm"},
        ".svelte": {"pnpm"},
        ".vue": {"pnpm"},
        ".json": {"pnpm", "cargo"},  # package.json, tsconfig, etc.
        # Python
        ".py": {"pytest"},
        ".pyi": {"pytest"},
        # Config files affect all ecosystems
        ".yml": {"pnpm", "cargo", "pytest"},
        ".yaml": {"pnpm", "cargo", "pytest"},
        ".sh": {"pnpm", "cargo", "pytest"},
        # Lock files
        ".lock": {"pnpm", "cargo"},
        # Documentation never affects tests
        ".md": set(),
        ".txt": set(),
        ".rst": set(),
    }

    def _detect_test_ecosystem(self, test_cmd: list[str]) -> str | None:
        """Determine the test ecosystem from the test command.

        Returns one of "cargo", "pnpm", "pytest", or None if unknown.
        """
        cmd_str = " ".join(test_cmd)
        if "cargo" in cmd_str:
            return "cargo"
        if any(kw in cmd_str for kw in ("pnpm", "npm", "vitest", "jest")):
            return "pnpm"
        if any(kw in cmd_str for kw in ("pytest", "python")):
            return "pytest"
        return None

    def should_skip_doctor_recovery(
        self, ctx: ShepherdContext, test_cmd: list[str]
    ) -> bool:
        """Check if Doctor recovery should be skipped for test failures.

        Compares the builder's changed files against the failing test
        ecosystem. If none of the changed files could plausibly affect
        the failing tests, the failures are pre-existing and Doctor
        recovery should be skipped.

        Returns True if Doctor should be skipped (no overlap).
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return False  # Can't determine — conservatively try Doctor

        # Get files changed by the builder
        changed = get_changed_files(cwd=ctx.worktree_path)
        if not changed:
            log_info(
                "Skipping Doctor recovery: no changed files in worktree "
                "(test failures are pre-existing)"
            )
            return True

        # Determine which ecosystem the failing tests belong to
        test_ecosystem = self._detect_test_ecosystem(test_cmd)
        if test_ecosystem is None:
            return False  # Unknown ecosystem — conservatively try Doctor

        # Check if any changed file could affect the failing test ecosystem
        for filepath in changed:
            ext = Path(filepath).suffix.lower()
            ecosystems = self._EXT_TO_ECOSYSTEM.get(ext)

            if ecosystems is None:
                # Unknown extension — conservatively assume it could affect anything
                return False

            if test_ecosystem in ecosystems:
                # At least one changed file overlaps with the failing ecosystem
                return False

        # No overlap found — failures are unrelated to builder's changes
        log_info(
            f"Skipping Doctor recovery: {len(changed)} changed file(s) "
            f"do not affect {test_ecosystem} tests"
        )
        return True
