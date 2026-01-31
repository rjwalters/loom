"""Builder phase implementation."""

from __future__ import annotations

import json
import logging
import subprocess
import time
from pathlib import Path
from typing import Any

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.state import parse_command_output, read_json_file
from loom_tools.common.worktree_safety import is_worktree_safe_to_remove
from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.issue_quality import (
    Severity,
    validate_issue_quality,
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

        # Pre-flight issue quality validation (informational only)
        self._run_quality_validation(ctx)

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

        # Run test verification in worktree
        test_result = self._run_test_verification(ctx)
        if test_result is not None and test_result.status == PhaseStatus.FAILED:
            # Clean up worktree on test failure to prevent blocking future attempts
            self._cleanup_on_failure(ctx)
            return test_result

        # Validate phase
        if not self.validate(ctx):
            # Cleanup stale worktree
            diag = self._gather_diagnostics(ctx)
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder phase validation failed: {diag['summary']}"
                ),
                phase_name="builder",
                data={"diagnostics": diag},
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

    def _run_quality_validation(self, ctx: ShepherdContext) -> None:
        """Run pre-flight quality validation on the issue body.

        Logs warnings for quality issues but never blocks the builder.
        """
        body = self._fetch_issue_body(ctx)
        if body is None:
            return

        result = validate_issue_quality(body)

        if not result.findings:
            log_info(f"Issue #{ctx.config.issue} passed pre-flight quality checks")
            return

        for finding in result.findings:
            if finding.severity == Severity.WARNING:
                log_warning(f"Issue #{ctx.config.issue} quality: {finding.message}")
            else:
                log_info(f"Issue #{ctx.config.issue} quality: {finding.message}")

        ctx.report_milestone(
            "heartbeat",
            action=f"issue quality: {len(result.warnings)} warning(s), {len(result.infos)} info(s)",
        )

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

    def _extract_error_lines(self, output: str) -> list[str]:
        """Extract error-indicator lines from test output for comparison.

        Returns a list of normalized lines that indicate failures (FAIL, ERROR,
        error messages, etc.). Used to diff baseline vs worktree output to
        detect new regressions.
        """
        error_indicators = ("fail", "error", "✗", "✕", "×", "not ok")
        lines = []
        for raw_line in output.splitlines():
            stripped = raw_line.strip()
            if not stripped:
                continue
            lower = stripped.lower()
            if any(indicator in lower for indicator in error_indicators):
                lines.append(stripped)
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
            # Baseline also fails. Compare error output to determine if
            # the worktree introduced new failures.
            baseline_errors = set(
                self._extract_error_lines(
                    baseline_result.stdout + "\n" + baseline_result.stderr
                )
            )
            worktree_errors = set(
                self._extract_error_lines(
                    result.stdout + "\n" + result.stderr
                )
            )
            new_errors = worktree_errors - baseline_errors

            if not new_errors:
                # All failures are pre-existing on main
                summary = self._parse_test_summary(
                    result.stdout + "\n" + result.stderr
                )
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

            # New errors introduced by the worktree
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

        return PhaseResult(
            status=PhaseStatus.FAILED,
            message=f"test verification failed ({display_name}, exit code {result.returncode})",
            phase_name="builder",
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

        This method is called when the builder phase fails (e.g., test verification
        failure) to ensure the worktree is removed so subsequent attempts don't
        encounter branch conflicts.

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
