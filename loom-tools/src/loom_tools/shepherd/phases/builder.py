"""Builder phase implementation."""

from __future__ import annotations

import json
import logging
import os
import re
import shlex
import subprocess
import time
from pathlib import Path
from typing import Any

from loom_tools.checkpoints import (
    Checkpoint,
    get_recovery_recommendation,
    read_checkpoint,
    write_checkpoint,
)
from loom_tools.common.git import get_changed_files, parse_porcelain_path
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.state import parse_command_output, read_json_file
from loom_tools.common.worktree_safety import is_worktree_safe_to_remove
from loom_tools.shepherd.config import Phase
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.issue_quality import (
    Severity,
    validate_issue_quality_with_gates,
)
from loom_tools.shepherd.labels import (
    get_pr_for_issue,
    transition_issue_labels,
)
from loom_tools.shepherd.phases.base import (
    DEGRADED_CRYSTALLIZING_THRESHOLD,
    DEGRADED_SCAN_TAIL_LINES,
    DEGRADED_SESSION_PATTERNS,
    MCP_FAILURE_PATTERNS,
    PhaseResult,
    PhaseStatus,
    _get_cli_output,
    extract_log_errors,
    run_phase_with_retry,
)

logger = logging.getLogger(__name__)

# Pre-implementation reproducibility check settings
_REPRODUCIBILITY_RUNS = 3  # Number of times to run each test for flakiness
_REPRODUCIBILITY_TIMEOUT = 120  # seconds per run

# Regex to extract fenced code blocks and inline code from markdown
_CODE_BLOCK_RE = re.compile(r"```(?:\w+)?\n(.*?)```", re.DOTALL)
_INLINE_CODE_RE = re.compile(r"`([^`\n]+)`")

# Test command prefixes recognized for reproducibility checking
_TEST_CMD_PREFIXES = [
    "python -m pytest",
    "pytest",
    "cargo test",
    "pnpm check:ci:lite",
    "pnpm check:ci",
    "pnpm check",
    "pnpm test",
    "npm test",
]

# Patterns in CLI output that indicate the builder was actively implementing
# (not just reading/analyzing). If a builder session contains these patterns
# but produced no git artifacts, it likely crashed/timed out rather than
# concluding "no changes needed."
_IMPLEMENTATION_TOOL_PATTERNS = [
    r"⠋|⠙|⠹|⠸|⠼|⠴|⠦|⠧|⠇|⠏",  # Claude Code spinner chars (tool execution)
    r"✓ Edit\b",  # Successful Edit tool call
    r"✓ Write\b",  # Successful Write tool call
    r"Wrote to ",  # Write tool output
]
_IMPLEMENTATION_TOOL_RE = re.compile("|".join(_IMPLEMENTATION_TOOL_PATTERNS))

# Minimum CLI output length (chars) to consider a session "substantive."
# Below this, even with tool patterns, the session may not have done real work.
_SUBSTANTIVE_OUTPUT_MIN_CHARS = 2000

# Minimum CLI output (chars) to believe the builder actually analyzed an issue
# and intentionally decided "no changes needed."  A builder that exits with
# near-zero output was degraded/failed, not making an analytical conclusion.
# Set lower than _SUBSTANTIVE_OUTPUT_MIN_CHARS because analysis (Read/Grep)
# produces less output than implementation (Edit/Write).
_MIN_ANALYSIS_OUTPUT_CHARS = 500

# Patterns that indicate MCP server or plugin failures in the CLI output.
# Unlike _is_mcp_failure() in base.py which gates on output volume (allowing
# sessions with substantial output to pass), these markers are checked
# unconditionally in _is_no_changes_needed() because thinking spinners can
# inflate output volume without any real tool calls.  See issue #2464.
_MCP_FAILURE_MARKER_PATTERNS = [
    *MCP_FAILURE_PATTERNS,
    r"plugins?\s+failed\s+to\s+install",
]
_MCP_FAILURE_MARKER_RE = re.compile(
    "|".join(_MCP_FAILURE_MARKER_PATTERNS), re.IGNORECASE
)

# Marker file name the builder writes to explicitly signal "no changes needed."
# The builder agent creates this file in the worktree root when it deliberately
# determines no code changes are required (e.g. bug already fixed on main).
# Without this marker, an empty worktree is treated as a builder failure
# (crash, timeout, OOM kill) rather than an intentional "no changes" decision.
# See issue #2403.
NO_CHANGES_MARKER = ".no-changes-needed"

# File extensions mapped to language categories for scoped test verification
_LANGUAGE_EXTENSIONS: dict[str, str] = {
    # Python
    ".py": "python",
    ".pyi": "python",
    # Rust
    ".rs": "rust",
    # TypeScript/JavaScript
    ".ts": "typescript",
    ".tsx": "typescript",
    ".js": "javascript",
    ".jsx": "javascript",
    ".mjs": "javascript",
    ".cjs": "javascript",
    # Configuration files that affect all languages
    ".json": "config",
    ".toml": "config",
    ".yaml": "config",
    ".yml": "config",
}

# Paths that indicate which language is affected (takes precedence over extension)
_LANGUAGE_PATH_PATTERNS: list[tuple[str, str]] = [
    ("loom-tools/", "python"),
    ("src-tauri/", "rust"),
    ("loom-daemon/", "rust"),
    ("loom-api/", "rust"),
    ("src/", "typescript"),
    ("e2e/", "typescript"),
    ("mcp-loom/", "javascript"),
]

# Patterns for build artifacts that should not be counted as meaningful builder work.
# These files are generated by dependency installs, builds, or loom infrastructure
# and don't represent actual code changes from the builder.
_BUILD_ARTIFACT_PATTERNS: list[str] = [
    "node_modules",
    "Cargo.lock",
    "target/",
    ".loom-checkpoint",
    ".loom-in-use",
    ".loom-interrupted-context.json",
    "pnpm-lock.yaml",
    ".venv",
    NO_CHANGES_MARKER,
]


def _build_worktree_env(worktree_path: Path | None) -> dict[str, str] | None:
    """Build environment with PYTHONPATH set for worktree imports.

    When running pytest in a worktree, the editable install's .pth file
    causes Python to resolve imports from the main repo instead of the
    worktree's modified source. Prepending the worktree's loom-tools/src
    to PYTHONPATH ensures imports resolve from the worktree first.

    Returns None if no override is needed (non-worktree or no loom-tools/src).
    """
    if worktree_path is None:
        return None
    worktree_src = worktree_path / "loom-tools" / "src"
    if not worktree_src.is_dir():
        return None
    env = os.environ.copy()
    env["PYTHONPATH"] = f"{worktree_src}:{env.get('PYTHONPATH', '')}"
    return env


class BuilderPhase:
    """Phase 3: Builder - Create worktree, implement, create PR."""

    def __init__(self) -> None:
        # Baseline snapshot of main's dirty files before builder spawns.
        # Used to distinguish pre-existing dirt from worktree escapes.
        self._main_dirty_baseline: set[str] | None = None
        # Cache baseline test results keyed by the actual command tuple.
        # This prevents re-running baselines on main between Doctor
        # iterations, which eliminates flaky-test false positives caused
        # by non-deterministic baseline results across runs.
        self._baseline_cache: dict[
            tuple[str, ...], subprocess.CompletedProcess[str] | None
        ] = {}

    def should_skip(self, ctx: ShepherdContext) -> tuple[bool, str]:
        """Check if builder phase should be skipped.

        Skip if:
        - --pr <N> specifies an existing PR number directly
        - --skip-builder flag with auto-detected PR
        - --from argument skips this phase (requires existing PR)
        - PR already exists for this issue
        """
        # Check --pr <N> override (skip builder, use specified PR)
        if ctx.config.pr_number_override is not None:
            ctx.pr_number = ctx.config.pr_number_override
            return True, f"skipped via --pr {ctx.config.pr_number_override}"

        # Check --skip-builder (skip builder, auto-detect PR)
        if ctx.config.skip_builder:
            pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
            if pr is None:
                return False, ""
            ctx.pr_number = pr
            return True, f"skipped via --skip-builder (PR #{pr})"

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
            # Ensure issue has loom:building label (atomic transition)
            if not ctx.has_issue_label("loom:building"):
                transition_issue_labels(
                    ctx.config.issue,
                    add=["loom:building"],
                    remove=["loom:issue"],
                    repo_root=ctx.repo_root,
                )
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

        # Claim the issue (atomic transition: loom:issue -> loom:building)
        transition_issue_labels(
            ctx.config.issue,
            add=["loom:building"],
            remove=["loom:issue"],
            repo_root=ctx.repo_root,
        )
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        # Pre-flight issue quality validation (may block with configured gates)
        quality_result = self._run_quality_validation(ctx)
        if quality_result is not None:
            # Quality validation blocked - revert claim (atomic transition)
            transition_issue_labels(
                ctx.config.issue,
                add=["loom:issue"],
                remove=["loom:building"],
                repo_root=ctx.repo_root,
            )
            ctx.label_cache.invalidate_issue(ctx.config.issue)
            return quality_result

        # Pre-implementation reproducibility check (issue #2316)
        # Before creating a worktree and running the builder, verify that
        # the bug described in the issue is still reproducible on main.
        repro_result = self._run_reproducibility_check(ctx)
        if repro_result is not None:
            return repro_result

        # Check for and recover from stale worktree (issue #1995)
        # A stale worktree is one left behind by a previous builder that crashed
        # or timed out before making any commits.
        if ctx.worktree_path and ctx.worktree_path.is_dir():
            if self._is_stale_worktree(ctx.worktree_path):
                log_warning(
                    f"Stale worktree detected at {ctx.worktree_path} "
                    "(no commits, no uncommitted changes) - cleaning up"
                )
                self._reset_stale_worktree(ctx)

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
            except FileNotFoundError as exc:
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=str(exc),
                    phase_name="builder",
                )
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

        # Pre-flight worktree anchor verification (issue #2630):
        # Confirm the worktree is a valid git directory before spawning the
        # builder.  This provides a clear audit trail.  We warn instead of
        # failing because the builder would fail naturally on git operations
        # if the worktree is invalid.
        if ctx.worktree_path and ctx.worktree_path.is_dir():
            anchor_check = subprocess.run(
                ["git", "-C", str(ctx.worktree_path), "rev-parse", "--git-dir"],
                capture_output=True, text=True, check=False,
            )
            if anchor_check.returncode != 0:
                log_warning(
                    f"Worktree at {ctx.worktree_path} may not be a valid "
                    f"git directory — builder may fail"
                )
            else:
                log_info(
                    f"Worktree anchor verified: builder will work in "
                    f"{ctx.worktree_path} (branch={ctx.worktree_path.name})"
                )

        # Snapshot main's dirty state before spawning the builder so we can
        # distinguish pre-existing dirt from actual worktree escapes later.
        self._main_dirty_baseline = self._snapshot_main_dirty(ctx)

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
            planning_timeout=ctx.config.planning_timeout,
        )

        if exit_code == 3:
            # Revert claim on shutdown (atomic transition)
            transition_issue_labels(
                ctx.config.issue,
                add=["loom:issue"],
                remove=["loom:building"],
                repo_root=ctx.repo_root,
            )
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

        if exit_code == 8:
            # Planning stall: builder stayed in "planning" checkpoint beyond
            # the planning_timeout without progressing to implementation.
            # This is distinct from "stuck" (exit 4) which is general idle
            # detection.  See issue #2443.
            log_warning(
                f"Builder stalled in planning checkpoint for issue "
                f"#{ctx.config.issue} (planning_timeout="
                f"{ctx.config.planning_timeout}s)"
            )
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder stalled in planning checkpoint without "
                    f"progressing to implementation "
                    f"(timeout={ctx.config.planning_timeout}s)"
                ),
                phase_name="builder",
                data={
                    "planning_stall": True,
                    "planning_timeout": ctx.config.planning_timeout,
                    "log_file": str(self._get_log_path(ctx)),
                },
            )

        if exit_code == 9:
            # Auth pre-flight failure: the wrapper's `claude auth status`
            # timed out, likely because a parent Claude session holds the
            # config lock.  Not retryable — fail fast instead of wasting
            # ~5 minutes on futile retries.  See issue #2508.
            log_error(
                f"Auth pre-flight failure for issue #{ctx.config.issue}: "
                f"authentication check timed out (not retryable)"
            )
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    "builder auth pre-flight failed: authentication check "
                    "timed out (parent session may hold config lock)"
                ),
                phase_name="builder",
                data={
                    "auth_failure": True,
                    "exit_code": exit_code,
                    "log_file": str(self._get_log_path(ctx)),
                },
            )

        if exit_code == 11:
            # Degraded session: builder ran under rate limits and entered a
            # Crystallizing loop, producing no useful work.  Not retryable —
            # the rate limit won't resolve until it resets.  See issue #2631.
            log_warning(
                f"Degraded session for issue #{ctx.config.issue}: "
                f"builder was rate-limited and entered Crystallizing loop"
            )
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    "builder session degraded: rate limit warnings and "
                    "Crystallizing loop detected (not retryable until "
                    "rate limit resets)"
                ),
                phase_name="builder",
                data={
                    "degraded_session": True,
                    "exit_code": exit_code,
                    "log_file": str(self._get_log_path(ctx)),
                },
            )

        if exit_code not in (0, 3, 4):
            # Unexpected non-zero exit from builder subprocess
            diag = self._gather_diagnostics(ctx)

            # Check if the builder actually completed its work despite
            # the non-zero exit.  This happens when e.g. MCP retries
            # cause a duplicate-worktree error (exit code 7) after the
            # PR was already created successfully, or when the builder
            # creates a PR but fails before updating its checkpoint
            # (e.g., MCP server failure after gh pr create).
            # The PR's existence is a stronger signal than checkpoint
            # stage — recover whenever diagnostics show a PR exists.
            if diag.get("pr_number") is not None:
                pr = diag["pr_number"]
                checkpoint = diag.get("checkpoint_stage", "unknown")
                log_warning(
                    f"Builder exited with code {exit_code} but PR #{pr} "
                    f"exists (checkpoint={checkpoint}) — treating as success"
                )
                ctx.pr_number = pr
                ctx.report_milestone("pr_created", pr_number=pr)
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=(
                        f"builder phase complete - PR #{pr} created "
                        f"(recovered from exit code {exit_code})"
                    ),
                    phase_name="builder",
                    data={
                        "pr_number": pr,
                        "exit_code": exit_code,
                        "recovered_from_checkpoint": True,
                        "checkpoint_stage": checkpoint,
                    },
                )

            # Recover from low-output (code 6) / MCP failure (code 7)
            # when the worktree has incomplete work from a previous builder
            # run.  The current builder CLI couldn't start (auth timeout,
            # nesting protection, etc.) but the worktree may have commits
            # or changes that can be completed mechanically.  See issue #2507.
            if exit_code in (6, 7) and diag.get("worktree_exists"):
                recovery = self._recover_from_existing_worktree(
                    ctx, diag, exit_code
                )
                if recovery is not None:
                    return recovery
                # Recovery wasn't possible — fall through to generic handling

            # If there are uncommitted changes, preserve them as a WIP commit
            if diag.get("has_uncommitted_changes"):
                reason = f"builder exited with code {exit_code}"
                if self._commit_interrupted_work(ctx, reason):
                    # Work was preserved - return failure but work is recoverable
                    return PhaseResult(
                        status=PhaseStatus.FAILED,
                        message=(
                            f"builder subprocess exited with code {exit_code}, "
                            f"uncommitted work preserved as WIP commit"
                        ),
                        phase_name="builder",
                        data={
                            "exit_code": exit_code,
                            "diagnostics": diag,
                            "work_preserved": True,
                        },
                    )

            # Clean up stale worktree to avoid leaving orphans
            if exit_code in (6, 7) and diag.get("worktree_exists"):
                if self._is_stale_worktree(ctx.worktree_path):
                    self._cleanup_stale_worktree(ctx)

            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder subprocess exited with code {exit_code}: "
                    f"{diag['summary']}"
                ),
                phase_name="builder",
                data={"exit_code": exit_code, "diagnostics": diag},
            )

        # Early worktree escape detection (issue #2630):
        # Check for dirty main immediately after builder exits — before
        # entering the validation/completion loop.  When the builder
        # escapes the worktree and modifies main instead, completion
        # retries are futile (they operate on the empty worktree).
        escape = self._detect_worktree_escape(ctx)
        if escape is not None:
            return escape

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
        # Use quiet=True to suppress intermediate diagnostic comments that
        # would persist even if a later retry succeeds (issue #2609).
        completion_attempts = 0
        max_completion_attempts = ctx.config.builder_completion_retries

        while True:
            if self.validate(ctx, quiet=True):
                break  # Validation passed

            # Re-diagnose on each iteration to detect partial progress
            diag = self._gather_diagnostics(ctx)

            # Check if this is incomplete work that could be completed
            if not self._has_incomplete_work(diag):
                # Check if this is the "no changes needed" pattern
                if self._is_no_changes_needed(diag):
                    log_info(
                        f"Builder analyzed issue #{ctx.config.issue} and determined "
                        "no changes are needed"
                    )
                    self._cleanup_stale_worktree(ctx)
                    return PhaseResult(
                        status=PhaseStatus.SKIPPED,
                        message="no changes needed - problem already resolved on main",
                        phase_name="builder",
                        data={
                            "no_changes_needed": True,
                            "reason": "already_resolved",
                            "diagnostics": diag,
                        },
                    )

                # Detect worktree escape: clean worktree but dirty main
                if diag.get("main_branch_dirty", False):
                    log_warning(
                        f"Builder may have escaped worktree for issue "
                        f"#{ctx.config.issue} — main branch has "
                        f"{diag.get('main_dirty_file_count', 0)} uncommitted "
                        f"file(s)"
                    )

                # No incomplete work pattern — nothing to retry
                self._cleanup_stale_worktree(ctx)
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"builder phase validation failed: {diag['summary']}"
                    ),
                    phase_name="builder",
                    data={"diagnostics": diag, "completion_attempts": completion_attempts},
                )

            # Try direct completion first — avoids spawning an LLM agent
            # when only mechanical steps (push, create_pr, add label) remain.
            if self._direct_completion(ctx, diag):
                log_success("Direct completion succeeded")
                continue  # Re-validate

            if completion_attempts >= max_completion_attempts:
                # Retries exhausted and direct completion couldn't handle it
                self._cleanup_stale_worktree(ctx)
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"builder phase validation failed: {diag['summary']}"
                    ),
                    phase_name="builder",
                    data={"diagnostics": diag, "completion_attempts": completion_attempts},
                )

            completion_attempts += 1
            log_warning(
                f"Builder left incomplete work (attempt {completion_attempts}/{max_completion_attempts}): "
                f"{diag['summary']}"
            )

            # Brief delay before retry to allow transient GitHub API issues to resolve
            if completion_attempts > 1:
                log_info("Waiting 5 seconds before retry to allow transient issues to resolve")
                time.sleep(5)

            # Run completion phase with attempt number for progressive simplification
            completion_exit = self._run_completion_phase(
                ctx, diag, attempt=completion_attempts
            )

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
            # Loop back to re-diagnose and potentially retry

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

    def validate(self, ctx: ShepherdContext, *, quiet: bool = False) -> bool:
        """Validate builder phase contract.

        Calls the Python validate_phase module directly for comprehensive
        validation with recovery.

        Args:
            quiet: If True, attempt recovery but suppress diagnostic comments
                   and label changes on failure.  Used by retry loops to avoid
                   posting noisy intermediate-failure comments (issue #2609).
        """
        from loom_tools.validate_phase import validate_phase

        result = validate_phase(
            phase="builder",
            issue=ctx.config.issue,
            repo_root=ctx.repo_root,
            worktree=str(ctx.worktree_path) if ctx.worktree_path else None,
            task_id=ctx.config.task_id,
            quiet=quiet,
        )
        return result.satisfied

    def run_test_verification_only(self, ctx: ShepherdContext) -> PhaseResult | None:
        """Run only the test verification step.

        This is used by the orchestrator after Doctor fixes to verify that
        tests now pass. Unlike the full run() method, this does not spawn
        the builder worker or modify issue labels.

        Returns:
            None if tests pass or cannot be run.
            PhaseResult with FAILED status if tests still fail.
        """
        return self._run_test_verification(ctx)

    def validate_and_complete(self, ctx: ShepherdContext) -> PhaseResult:
        """Validate builder phase and run completion if needed.

        This method is used after the doctor test-fix loop succeeds to ensure
        that a PR actually exists. If the builder made changes but didn't
        complete the commit/push/PR workflow, this will spawn a focused
        completion phase to finish the work.

        This is the same validation and completion logic that runs at the end
        of the normal builder.run() method, extracted for reuse.

        Returns:
            PhaseResult with SUCCESS status if PR exists or was created.
            PhaseResult with FAILED status if completion could not be achieved.
        """
        # Run validation with completion retry
        # Use quiet=True to suppress intermediate diagnostic comments (issue #2609).
        completion_attempts = 0
        max_completion_attempts = ctx.config.builder_completion_retries

        while True:
            if self.validate(ctx, quiet=True):
                break  # Validation passed

            # Re-diagnose on each iteration to detect partial progress
            diag = self._gather_diagnostics(ctx)

            # Check if this is incomplete work that could be completed
            if not self._has_incomplete_work(diag):
                # Check if this is the "no changes needed" pattern
                if self._is_no_changes_needed(diag):
                    log_info(
                        f"Builder analyzed issue #{ctx.config.issue} after doctor fixes "
                        "and determined no changes are needed"
                    )
                    return PhaseResult(
                        status=PhaseStatus.SKIPPED,
                        message="no changes needed - problem already resolved on main",
                        phase_name="builder",
                        data={
                            "no_changes_needed": True,
                            "reason": "already_resolved",
                            "diagnostics": diag,
                        },
                    )

                # Detect worktree escape: clean worktree but dirty main
                if diag.get("main_branch_dirty", False):
                    log_warning(
                        f"Builder may have escaped worktree for issue "
                        f"#{ctx.config.issue} — main branch has "
                        f"{diag.get('main_dirty_file_count', 0)} uncommitted "
                        f"file(s)"
                    )

                # No incomplete work pattern — nothing to retry
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"builder phase validation failed after doctor fixes: "
                        f"{diag['summary']}"
                    ),
                    phase_name="builder",
                    data={"diagnostics": diag, "completion_attempts": completion_attempts},
                )

            if completion_attempts >= max_completion_attempts:
                # Retries exhausted — try direct fallback for mechanical ops
                if self._direct_completion(ctx, diag):
                    log_success("Direct completion succeeded after doctor fixes")
                    continue  # Re-validate

                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"builder phase validation failed after doctor fixes: "
                        f"{diag['summary']}"
                    ),
                    phase_name="builder",
                    data={"diagnostics": diag, "completion_attempts": completion_attempts},
                )

            completion_attempts += 1
            log_warning(
                f"Builder left incomplete work after doctor fixes "
                f"(attempt {completion_attempts}/{max_completion_attempts}): "
                f"{diag['summary']}"
            )

            # Brief delay before retry to allow transient GitHub API issues to resolve
            if completion_attempts > 1:
                log_info("Waiting 5 seconds before retry to allow transient issues to resolve")
                time.sleep(5)

            # Run completion phase with attempt number for progressive simplification
            completion_exit = self._run_completion_phase(
                ctx, diag, attempt=completion_attempts
            )

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
            # Loop back to re-diagnose and potentially retry

        # Get PR number
        pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if pr is None:
            diag = self._gather_diagnostics(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"could not find PR for issue #{ctx.config.issue} after doctor fixes: "
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
            message=f"builder phase complete after doctor fixes - PR #{pr} created",
            phase_name="builder",
            data={"pr_number": pr},
        )

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

    def _fetch_issue_comments(self, ctx: ShepherdContext) -> list[str]:
        """Fetch issue comment bodies from GitHub.

        Returns a list of comment body strings, or an empty list if the
        fetch fails or there are no comments.
        """
        try:
            result = subprocess.run(
                [
                    "gh",
                    "issue",
                    "view",
                    str(ctx.config.issue),
                    "--json",
                    "comments",
                ],
                cwd=ctx.repo_root,
                text=True,
                capture_output=True,
                check=False,
            )
            if result.returncode == 0:
                data = json.loads(result.stdout)
                return [
                    c["body"]
                    for c in data.get("comments", [])
                    if c.get("body")
                ]
        except (OSError, json.JSONDecodeError, KeyError):
            pass
        return []

    def _parse_test_command(self, line: str) -> list[str] | None:
        """Parse a single line to check if it's a recognized test command.

        Strips common shell prompt prefixes (``$``) and checks against
        ``_TEST_CMD_PREFIXES``.  Returns the parsed command as a list of
        arguments, or ``None`` if the line is not a test command.
        """
        stripped = line.lstrip("$ ").strip()
        if not stripped or stripped.startswith("#"):
            return None

        for prefix in _TEST_CMD_PREFIXES:
            if stripped.startswith(prefix) and (
                len(stripped) == len(prefix)
                or stripped[len(prefix)] in (" ", "\t")
            ):
                try:
                    return shlex.split(stripped)
                except ValueError:
                    return None

        return None

    def _extract_test_commands(self, text: str) -> list[tuple[list[str], str]]:
        """Extract runnable test commands from issue markdown text.

        Searches fenced code blocks and inline code spans for recognized
        test invocations (``pytest``, ``cargo test``, ``pnpm test``, etc.).

        Returns a deduplicated list of ``(command_args, display_name)``
        tuples.
        """
        commands: list[tuple[list[str], str]] = []
        seen: set[str] = set()

        # Extract from fenced code blocks
        for match in _CODE_BLOCK_RE.finditer(text):
            block = match.group(1)
            for line in block.strip().splitlines():
                cmd = self._parse_test_command(line.strip())
                if cmd is not None:
                    key = " ".join(cmd)
                    if key not in seen:
                        seen.add(key)
                        commands.append((cmd, key))

        # Extract from inline code spans
        for match in _INLINE_CODE_RE.finditer(text):
            code = match.group(1).strip()
            cmd = self._parse_test_command(code)
            if cmd is not None:
                key = " ".join(cmd)
                if key not in seen:
                    seen.add(key)
                    commands.append((cmd, key))

        return commands

    def _run_reproducibility_check(
        self, ctx: ShepherdContext
    ) -> PhaseResult | None:
        """Check if the bug described in the issue is still reproducible.

        Extracts test commands from the issue body and comments, then runs
        them on the repo root (main branch) multiple times.  If every
        command passes on all runs, the bug is considered already fixed and
        a ``PhaseResult`` with ``no_changes_needed`` is returned.

        Returns:
            ``None`` if any test still fails or no commands were found
            (the caller should proceed with the normal builder workflow).
            A ``PhaseResult`` with ``SKIPPED`` status when all tests pass
            reliably, indicating no implementation is needed.
        """
        body = self._fetch_issue_body(ctx)
        if body is None:
            return None

        comments = self._fetch_issue_comments(ctx)
        full_text = body + "\n" + "\n".join(comments)

        commands = self._extract_test_commands(full_text)
        if not commands:
            log_info(
                f"No test commands found in issue #{ctx.config.issue}, "
                "skipping reproducibility check"
            )
            return None

        log_info(
            f"Reproducibility check: found {len(commands)} test command(s) "
            f"in issue #{ctx.config.issue}"
        )
        ctx.report_milestone(
            "heartbeat",
            action="running pre-implementation reproducibility check",
        )

        for cmd_args, display_name in commands:
            for run_num in range(1, _REPRODUCIBILITY_RUNS + 1):
                log_info(
                    f"Reproducibility run {run_num}/{_REPRODUCIBILITY_RUNS}: "
                    f"{display_name}"
                )
                pre_dirty = self._get_dirty_files(ctx.repo_root)
                try:
                    result = subprocess.run(
                        cmd_args,
                        cwd=ctx.repo_root,
                        text=True,
                        capture_output=True,
                        timeout=_REPRODUCIBILITY_TIMEOUT,
                        check=False,
                    )
                except subprocess.TimeoutExpired:
                    self._cleanup_new_artifacts(ctx.repo_root, pre_dirty)
                    log_info(
                        f"Test timed out on main (run {run_num}): "
                        f"{display_name}"
                    )
                    return None
                except OSError as e:
                    self._cleanup_new_artifacts(ctx.repo_root, pre_dirty)
                    log_warning(
                        f"Could not run test command: {display_name}: {e}"
                    )
                    return None
                self._cleanup_new_artifacts(ctx.repo_root, pre_dirty)
                if result.returncode != 0:
                    log_info(
                        f"Test still fails on main (run {run_num}): "
                        f"{display_name} (exit code {result.returncode})"
                    )
                    # Bug is still reproducible — proceed normally
                    return None

        # All commands passed on every run
        log_info(
            f"All test commands pass on main "
            f"({_REPRODUCIBILITY_RUNS} runs each) for issue "
            f"#{ctx.config.issue} — bug appears already fixed"
        )

        return PhaseResult(
            status=PhaseStatus.SKIPPED,
            message=(
                "no changes needed - bug already fixed on main "
                "(pre-implementation verification)"
            ),
            phase_name="builder",
            data={
                "no_changes_needed": True,
                "reason": "already_resolved",
                "pre_implementation_check": True,
                "test_commands": [display for _, display in commands],
                "runs_per_command": _REPRODUCIBILITY_RUNS,
            },
        )

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
        ``pnpm install --frozen-lockfile`` when needed.  Also checks for
        pyproject.toml with missing .venv and runs ``uv sync`` when needed.

        Returns True if dependencies are ready (already present or
        successfully installed), False if installation failed.
        Installation failure is non-fatal -- callers should continue
        gracefully.
        """
        ok = True
        ok = self._ensure_node_deps(worktree) and ok
        ok = self._ensure_python_deps(worktree) and ok
        return ok

    def _ensure_node_deps(self, worktree: Path) -> bool:
        """Install Node dependencies if package.json exists but node_modules is missing."""
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

    def _ensure_python_deps(self, worktree: Path) -> bool:
        """Sync Python venv if pyproject.toml exists but .venv is missing."""
        pyproject = worktree / "pyproject.toml"
        venv = worktree / ".venv"

        if not pyproject.is_file() or venv.is_dir():
            return True

        log_info(".venv missing, running uv sync")
        try:
            result = subprocess.run(
                ["uv", "sync"],
                cwd=worktree,
                text=True,
                capture_output=True,
                timeout=120,
                check=False,
            )
            if result.returncode == 0:
                log_success("Python dependencies synced successfully")
                return True
            log_warning(
                f"uv sync failed (exit code {result.returncode}): "
                f"{(result.stderr or result.stdout or '').strip()[:200]}"
            )
            return False
        except subprocess.TimeoutExpired:
            log_warning("uv sync timed out after 120s")
            return False
        except OSError as e:
            log_warning(f"Could not run uv sync: {e}")
            return False

    def _classify_changed_files(self, files: list[str]) -> set[str]:
        """Classify changed files by language/type.

        Returns a set of language categories: "python", "rust", "typescript",
        "javascript", "config", "other".

        Config changes are treated as affecting ALL languages since they may
        impact the build/test configuration.
        """
        languages: set[str] = set()

        for filepath in files:
            # Check path patterns first (more specific)
            matched = False
            for pattern, lang in _LANGUAGE_PATH_PATTERNS:
                if filepath.startswith(pattern):
                    languages.add(lang)
                    matched = True
                    break

            # If no path pattern matched, check extension
            if not matched:
                ext = Path(filepath).suffix.lower()
                if ext in _LANGUAGE_EXTENSIONS:
                    languages.add(_LANGUAGE_EXTENSIONS[ext])
                else:
                    languages.add("other")

        return languages

    def _find_python_test_root(self, worktree: Path) -> Path | None:
        """Find the directory containing pyproject.toml for Python tests.

        Checks the worktree root first, then known Python package directories.
        Returns None if no pyproject.toml is found.
        """
        # Check root first
        if (worktree / "pyproject.toml").is_file():
            return worktree
        # Check known Python package directories
        for subdir in ["loom-tools"]:
            if (worktree / subdir / "pyproject.toml").is_file():
                return worktree / subdir
        return None

    def _get_python_test_files(
        self, worktree: Path, changed_files: list[str], python_root: Path
    ) -> list[str]:
        """Extract Python test file paths from changed files.

        For each changed Python file, determines the relevant test file:
        - If the file is already a test file (test_*.py or *_test.py), include it
        - If the file is a source file, look for a corresponding test file

        Returns paths relative to python_root suitable for passing to pytest.
        Returns an empty list if no specific test files can be identified
        (caller should fall back to running the full pytest suite).
        """
        test_files: list[str] = []

        for filepath in changed_files:
            if not filepath.endswith(".py") and not filepath.endswith(".pyi"):
                continue

            filename = Path(filepath).name
            # Check if this is already a test file
            if filename.startswith("test_") or filename.endswith("_test.py"):
                # Resolve the path relative to python_root
                abs_path = worktree / filepath
                if abs_path.is_file():
                    try:
                        rel = str(abs_path.relative_to(python_root))
                        if rel not in test_files:
                            test_files.append(rel)
                    except ValueError:
                        # File is outside python_root, skip
                        pass
                continue

            # Source file: try to find corresponding test file
            stem = Path(filepath).stem
            # Common patterns: module.py -> test_module.py
            test_name = f"test_{stem}.py"
            # Search for test file in the python_root's test directories
            abs_path = worktree / filepath
            parent = abs_path.parent

            # Look for test file in common locations relative to the source
            candidates = [
                parent / test_name,                          # Same directory
                worktree / "tests" / test_name,              # Root tests/
                python_root / "tests" / test_name,           # Python root tests/
            ]
            # Also check tests/ relative to parent path structure
            try:
                rel_to_root = abs_path.relative_to(python_root)
                # e.g. src/foo/bar.py -> tests/foo/test_bar.py
                test_in_mirror = python_root / "tests" / rel_to_root.parent / test_name
                candidates.append(test_in_mirror)
            except ValueError:
                pass

            for candidate in candidates:
                if candidate.is_file():
                    try:
                        rel = str(candidate.relative_to(python_root))
                        if rel not in test_files:
                            test_files.append(rel)
                    except ValueError:
                        pass
                    break

        return test_files

    def _get_scoped_test_commands(
        self,
        worktree: Path,
        languages: set[str],
        changed_files: list[str] | None = None,
    ) -> list[tuple[list[str], str]]:
        """Get the test commands scoped to the changed languages.

        Returns a list of (command_args, display_name) tuples for the relevant
        test suites. If "config" is in languages, returns all test commands
        (config changes affect everything).

        When changed_files is provided, pytest commands are scoped to specific
        test files rather than running the entire suite.

        Returns an empty list if no relevant tests are detected.
        """
        commands: list[tuple[list[str], str]] = []

        # Config changes affect everything - run all tests.
        # Prefer decomposing &&-chained pipelines to avoid short-circuit
        # masking failures in later stages (see issue #2610).
        if "config" in languages:
            for script_name in ("check:ci:lite", "check:ci"):
                decomposed = self._decompose_pipeline_script(worktree, script_name)
                if decomposed:
                    return decomposed
            full_cmd = self._detect_test_command(worktree)
            if full_cmd:
                return [full_cmd]
            return []

        # Check which test runners are available
        has_package_json = (worktree / "package.json").is_file()
        has_cargo_toml = (worktree / "Cargo.toml").is_file()
        python_root = self._find_python_test_root(worktree)

        # Python changes -> Python tests only
        if "python" in languages and python_root:
            # Try to scope to specific test files
            test_files: list[str] = []
            if changed_files:
                test_files = self._get_python_test_files(
                    worktree, changed_files, python_root
                )

            if test_files:
                # Scope pytest to specific test files
                if python_root == worktree:
                    cmd = ["uv", "run", "pytest", "-x", "-q"] + test_files
                else:
                    cmd = [
                        "uv", "run", "--directory", str(python_root),
                        "pytest", "-x", "-q",
                    ] + test_files
                display = f"pytest {' '.join(test_files)}"
                commands.append((cmd, display))
            elif python_root == worktree:
                commands.append((["uv", "run", "pytest", "-x", "-q"], "pytest"))
            else:
                commands.append(
                    (
                        ["uv", "run", "--directory", str(python_root), "pytest", "-x", "-q"],
                        "pytest",
                    )
                )

        # Rust changes -> Rust clippy and tests
        if "rust" in languages and has_cargo_toml:
            # Clippy for linting
            commands.append(
                (
                    [
                        "cargo",
                        "clippy",
                        "--workspace",
                        "--exclude",
                        "app",
                        "--all-targets",
                        "--all-features",
                        "--locked",
                        "--",
                        "-D",
                        "warnings",
                    ],
                    "cargo clippy",
                )
            )
            # Cargo test
            commands.append(
                (
                    [
                        "cargo",
                        "test",
                        "--workspace",
                        "--exclude",
                        "app",
                        "--locked",
                        "--all-features",
                        "--no-fail-fast",
                        "--",
                        "--nocapture",
                    ],
                    "cargo test",
                )
            )

        # TypeScript/JavaScript changes -> vitest + biome lint
        if ("typescript" in languages or "javascript" in languages) and has_package_json:
            pkg = read_json_file(worktree / "package.json")
            if isinstance(pkg, dict):
                scripts = pkg.get("scripts", {})
                # Lint check (biome or eslint)
                if "lint" in scripts:
                    commands.append((["pnpm", "lint"], "pnpm lint"))
                # Typecheck
                if "typecheck" in scripts:
                    commands.append((["pnpm", "typecheck"], "pnpm typecheck"))
                # Unit tests
                if "test:unit:coverage" in scripts:
                    commands.append(
                        (["pnpm", "test:unit:coverage"], "pnpm test:unit:coverage")
                    )
                elif "test:unit" in scripts:
                    commands.append((["pnpm", "test:unit"], "pnpm test:unit"))

        # If "other" category or no specific tests detected, fall back to
        # full test suite (decomposing pipelines to avoid short-circuit issues)
        if not commands and ("other" in languages or not languages):
            for script_name in ("check:ci:lite", "check:ci"):
                decomposed = self._decompose_pipeline_script(worktree, script_name)
                if decomposed:
                    commands = decomposed
                    break
            if not commands:
                full_cmd = self._detect_test_command(worktree)
                if full_cmd:
                    commands.append(full_cmd)

        return commands

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

        python_root = self._find_python_test_root(worktree)
        if python_root:
            if python_root == worktree:
                return (["python", "-m", "pytest"], "pytest")
            else:
                return (
                    ["python", "-m", "pytest", "--rootdir", str(python_root)],
                    "pytest",
                )

        return None

    def _decompose_pipeline_script(
        self, worktree: Path, script_name: str
    ) -> list[tuple[list[str], str]] | None:
        """Decompose a &&-chained package.json script into individual commands.

        When a script like ``check:ci:lite`` contains multiple commands joined
        with ``&&``, running it as a single command allows early failures to
        short-circuit the pipeline, preventing later test suites from executing.
        This makes baseline comparison unreliable because both sides may fail
        at the same early step while the worktree has regressions in later steps.

        This method reads the script value from ``package.json``, splits on
        ``&&``, and returns individual ``(command_args, display_name)`` tuples
        so each step can be run and compared independently.

        Returns None if the script is not a pipeline (single command) or if
        the script is not found in package.json.
        """
        try:
            if not (worktree / "package.json").is_file():
                return None

            pkg = read_json_file(worktree / "package.json")
            if not isinstance(pkg, dict):
                return None

            scripts = pkg.get("scripts", {})
            script_value = scripts.get(script_name, "")

            if not isinstance(script_value, str) or " && " not in script_value:
                return None

            parts = [p.strip() for p in script_value.split(" && ") if p.strip()]
            if len(parts) <= 1:
                return None

            commands: list[tuple[list[str], str]] = []
            for part in parts:
                args = shlex.split(part)
                if not args:
                    continue
                commands.append((args, part))

            return commands if commands else None
        except Exception:
            # Decomposition is best-effort; fall back to single-command path
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

            # biome: "Found 1 error." or "Found 3 errors."
            m = re.match(r"Found\s+(\d+)\s+errors?\.?$", stripped)
            if m:
                failure_counts.append(int(m.group(1)))
                found_any_summary = True
                continue

            # clippy: "could not compile `foo` due to 23 previous errors"
            m = re.search(r"due to (\d+) previous errors?", stripped)
            if m:
                failure_counts.append(int(m.group(1)))
                found_any_summary = True
                continue

        if not found_any_summary:
            return None

        # Return worst result (highest failure count)
        return max(failure_counts)

    def _identify_failure_tool(self, output: str) -> str | None:
        """Identify which tool produced the failure output.

        Scans the output for tool-specific markers to determine which
        pipeline stage failed. Returns the tool name or None if
        unrecognizable. Used by _compare_test_results to avoid
        cross-tool count comparisons.
        """
        for line in output.splitlines():
            stripped = line.strip()

            # biome: "Found N error(s)." or biome check header
            if re.match(r"Found\s+\d+\s+errors?\.?$", stripped):
                return "biome"

            # clippy: "could not compile ... due to N previous errors"
            if "due to" in stripped and "previous error" in stripped:
                return "clippy"

            # cargo test: "test result:" lines
            if stripped.startswith("test result:"):
                return "cargo_test"

            # cargo multi-target: "error: N target(s) failed:"
            if re.match(r"error:\s+\d+\s+targets?\s+failed", stripped):
                return "cargo_test"

            # pytest: "N failed, M passed in Xs" or "N passed in Xs"
            if re.search(r"\d+\s+(failed|passed)\s+in\s+\d", stripped):
                return "pytest"

            # vitest/jest: "Tests  N failed" or "Tests  N passed"
            if re.match(r"\s*Tests?\s+\d+\s+(failed|passed)", stripped):
                return "vitest"

        return None

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

    def _has_pytest_output(self, output: str) -> bool:
        """Check if test output contains pytest results.

        Detects whether the combined output from a pipeline command includes
        pytest execution output. Used to determine if Python tests actually
        ran when the primary command is a ``&&``-chained pipeline like
        ``check:ci:lite`` that may short-circuit before reaching pytest.
        """
        for line in output.splitlines():
            stripped = line.strip()
            # pytest session header: "== test session starts =="
            if "test session starts" in stripped:
                return True
            # pytest summary with = borders: "== 1 failed, 14 passed in 2.45s =="
            # or "== 15 passed in 0.50s =="
            # Only match lines that start with "=" to avoid false positives
            # from vitest/jest ("Tests  5 passed in 1.23s").
            if stripped.startswith("=") and "passed" in stripped:
                return True
        return False

    def _get_supplemental_test_commands(
        self, ctx: ShepherdContext, primary_output: str
    ) -> list[tuple[list[str], str]]:
        """Determine supplemental test commands for ecosystems not covered by primary output.

        When the primary test command is a ``&&``-chained pipeline (like
        ``check:ci:lite``), earlier failures can short-circuit and prevent
        later test suites from running. This method checks which ecosystems
        the builder's changed files affect and returns test commands for any
        ecosystem whose output is missing from the primary run.

        Currently supports supplemental Python test detection. Can be
        extended for other ecosystems as needed.

        Returns a list of (command_args, display_name) tuples.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return []

        changed_files = get_changed_files(cwd=ctx.worktree_path)
        if not changed_files:
            return []

        # Check if any changed files are Python files
        has_python_changes = any(
            f.endswith(".py") or f.endswith(".pyi") for f in changed_files
        )
        if not has_python_changes:
            return []

        # Check if pytest already ran in the primary output
        if self._has_pytest_output(primary_output):
            return []

        # Python files changed but pytest didn't run — need supplemental check.
        # Look for the test:python script in package.json, fall back to pytest.
        worktree = ctx.worktree_path
        if (worktree / "package.json").is_file():
            pkg = read_json_file(worktree / "package.json")
            if isinstance(pkg, dict):
                scripts = pkg.get("scripts", {})
                if "test:python" in scripts:
                    return [(["pnpm", "test:python"], "pnpm test:python (supplemental)")]

        python_root = self._find_python_test_root(worktree)
        if python_root:
            if python_root == worktree:
                return [(["python", "-m", "pytest"], "pytest (supplemental)")]
            else:
                return [
                    (
                        ["python", "-m", "pytest", "--rootdir", str(python_root)],
                        "pytest (supplemental)",
                    )
                ]

        return []

    def _run_supplemental_verification(
        self, ctx: ShepherdContext, primary_output: str
    ) -> PhaseResult | None:
        """Run supplemental tests for ecosystems missed by the primary pipeline.

        When the primary test command (e.g., ``check:ci:lite``) short-circuits
        via ``&&`` chaining, test suites later in the pipeline may not execute.
        This method detects which ecosystems the builder's changes affect but
        whose tests didn't run, and executes those test suites separately.

        Uses the same baseline comparison logic as the primary verification
        to distinguish new regressions from pre-existing failures.

        Returns None if all supplemental tests pass (or none are needed).
        Returns a PhaseResult with FAILED status if new failures are detected.
        """
        supplemental_cmds = self._get_supplemental_test_commands(ctx, primary_output)
        if not supplemental_cmds:
            return None

        for test_cmd, display_name in supplemental_cmds:
            log_info(f"Running supplemental test: {display_name}")
            ctx.report_milestone("heartbeat", action=f"supplemental: {display_name}")

            # Run baseline for this specific test command (not the full pipeline)
            baseline_result = self._run_baseline_tests(
                ctx, test_cmd, display_name, use_provided_cmd=True
            )

            test_start = time.time()
            try:
                result = subprocess.run(
                    test_cmd,
                    cwd=ctx.worktree_path,
                    text=True,
                    capture_output=True,
                    timeout=ctx.config.test_verify_timeout,
                    check=False,
                    env=_build_worktree_env(ctx.worktree_path),
                )
            except subprocess.TimeoutExpired:
                elapsed = int(time.time() - test_start)
                log_error(f"Supplemental test timed out after {elapsed}s ({display_name})")
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=f"supplemental test timed out after {elapsed}s ({display_name})",
                    phase_name="builder",
                    data={"test_failure": True, "test_timeout": True},
                )
            except OSError as e:
                log_warning(f"Could not run supplemental test ({display_name}): {e}")
                continue

            elapsed = int(time.time() - test_start)

            if result.returncode == 0:
                log_success(f"Supplemental test passed ({display_name}, {elapsed}s)")
                continue

            # Supplemental test failed — apply baseline comparison
            if baseline_result is not None and baseline_result.returncode != 0:
                baseline_output = baseline_result.stdout + "\n" + baseline_result.stderr
                worktree_output = result.stdout + "\n" + result.stderr

                structured = self._compare_test_results(baseline_output, worktree_output)
                if structured is None:
                    summary = self._parse_test_summary(worktree_output)
                    msg = f"pre-existing on main ({summary})" if summary else "pre-existing on main"
                    log_warning(
                        f"Supplemental test failed but pre-existing: {msg}"
                    )
                    continue

            # New failure from supplemental test
            combined = (result.stdout + "\n" + result.stderr).strip()
            tail_lines = combined.splitlines()[-10:]
            summary = self._parse_test_summary(combined)
            changed_files = get_changed_files(cwd=ctx.worktree_path) if ctx.worktree_path else []

            if summary:
                log_error(f"Supplemental test failed ({summary}, {elapsed}s)")
            else:
                log_error(f"Supplemental test failed ({display_name}, exit code {result.returncode}, {elapsed}s)")

            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=f"supplemental test failed ({display_name}, exit code {result.returncode})",
                phase_name="builder",
                data={
                    "test_failure": True,
                    "test_output_tail": "\n".join(tail_lines),
                    "test_summary": summary or "",
                    "test_command": display_name,
                    "changed_files": changed_files,
                },
            )

        return None

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
            # Check if both outputs are from the same tool.  When the
            # pipeline fails at different stages (e.g. biome vs clippy),
            # comparing counts across tools is meaningless.
            baseline_tool = self._identify_failure_tool(baseline_output)
            worktree_tool = self._identify_failure_tool(worktree_output)
            if (
                baseline_tool is not None
                and worktree_tool is not None
                and baseline_tool != worktree_tool
            ):
                # Different tools failed — worktree likely regressed at
                # an earlier (or different) pipeline stage.
                return True

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

    def _compute_new_error_count(
        self, worktree_output: str, baseline_output: str | None
    ) -> int | None:
        """Compute the number of new errors introduced by the worktree.

        Uses structured failure count parsing to determine how many errors
        the worktree introduces beyond baseline. Used by the orchestrator
        to detect regressions across doctor test-fix iterations.

        Returns:
            The number of new errors (worktree - baseline), or None if
            failure counts could not be parsed.
        """
        worktree_count = self._parse_failure_count(worktree_output)
        if worktree_count is None:
            # Fall back to error line counting
            worktree_lines = set(self._extract_error_lines(worktree_output))
            if not worktree_lines:
                return None
            if baseline_output is None:
                return len(worktree_lines)
            baseline_lines = set(self._extract_error_lines(baseline_output))
            return len(worktree_lines - baseline_lines)

        if baseline_output is None:
            return worktree_count

        baseline_count = self._parse_failure_count(baseline_output)
        if baseline_count is None:
            return worktree_count

        return max(0, worktree_count - baseline_count)

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
        *,
        use_provided_cmd: bool = False,
    ) -> subprocess.CompletedProcess[str] | None:
        """Run tests against main branch (repo root) to establish a baseline.

        Args:
            ctx: Shepherd context.
            test_cmd: Test command (used when *use_provided_cmd* is True).
            display_name: Human-readable name for logging.
            use_provided_cmd: When True, run *test_cmd* directly at the repo
                root instead of auto-detecting the test command.  Used by
                supplemental verification to run specific ecosystem tests.

        Returns the CompletedProcess result from running tests at the repo root,
        or None if the baseline cannot be obtained (timeout, OSError, or no
        repo root available).
        """
        if not ctx.repo_root or not ctx.repo_root.is_dir():
            return None

        if use_provided_cmd:
            baseline_cmd = test_cmd
        else:
            # Detect test command at repo root (may differ from worktree)
            baseline_test_info = self._detect_test_command(ctx.repo_root)
            if baseline_test_info is None:
                return None
            baseline_cmd, _ = baseline_test_info

        # Return cached baseline if available (avoids flaky-test false
        # positives when baselines are re-run between Doctor iterations).
        cache_key = tuple(baseline_cmd)
        if cache_key in self._baseline_cache:
            log_info(f"Using cached baseline for: {display_name}")
            return self._baseline_cache[cache_key]

        log_info(f"Running baseline tests on main: {display_name}")

        # Snapshot dirty state before running tests so we can clean up artifacts
        pre_dirty = self._get_dirty_files(ctx.repo_root)

        try:
            result = subprocess.run(
                baseline_cmd,
                cwd=ctx.repo_root,
                text=True,
                capture_output=True,
                timeout=ctx.config.test_verify_timeout,
                check=False,
            )
        except subprocess.TimeoutExpired:
            log_warning("Baseline test run timed out, skipping comparison")
            self._cleanup_new_artifacts(ctx.repo_root, pre_dirty)
            self._baseline_cache[cache_key] = None
            return None
        except OSError as e:
            log_warning(f"Could not run baseline tests: {e}")
            self._baseline_cache[cache_key] = None
            return None

        self._cleanup_new_artifacts(ctx.repo_root, pre_dirty)
        self._baseline_cache[cache_key] = result
        return result

    @staticmethod
    def _get_dirty_files(repo_root: Path) -> set[str]:
        """Return the set of dirty file paths in a git working tree."""
        try:
            result = subprocess.run(
                ["git", "status", "--porcelain"],
                cwd=repo_root,
                text=True,
                capture_output=True,
                timeout=30,
                check=False,
            )
            if result.returncode != 0:
                return set()
            entries = set()
            for line in result.stdout.splitlines():
                if len(line) > 3:
                    entries.add(parse_porcelain_path(line))
            return entries
        except (subprocess.TimeoutExpired, OSError):
            return set()

    @staticmethod
    def _cleanup_new_artifacts(repo_root: Path, pre_dirty: set[str]) -> None:
        """Remove files dirtied by baseline test run that weren't dirty before."""
        try:
            result = subprocess.run(
                ["git", "status", "--porcelain"],
                cwd=repo_root,
                text=True,
                capture_output=True,
                timeout=30,
                check=False,
            )
            if result.returncode != 0:
                return
        except (subprocess.TimeoutExpired, OSError):
            return

        new_entries: list[tuple[str, str]] = []  # (status, path)
        for line in result.stdout.splitlines():
            if len(line) > 3:
                path = parse_porcelain_path(line)
                if path not in pre_dirty:
                    status = line[:2].strip()
                    new_entries.append((status, path))

        if not new_entries:
            return

        # Restore tracked files that were modified
        tracked = [p for s, p in new_entries if s != "??"]
        if tracked:
            try:
                subprocess.run(
                    ["git", "checkout", "--"] + tracked,
                    cwd=repo_root,
                    capture_output=True,
                    timeout=30,
                    check=False,
                )
            except (subprocess.TimeoutExpired, OSError):
                pass

        # Remove untracked files
        untracked = [p for s, p in new_entries if s == "??"]
        if untracked:
            try:
                subprocess.run(
                    ["git", "clean", "-f", "--"] + untracked,
                    cwd=repo_root,
                    capture_output=True,
                    timeout=30,
                    check=False,
                )
            except (subprocess.TimeoutExpired, OSError):
                pass

        cleaned = len(tracked) + len(untracked)
        if cleaned:
            log_info(
                f"Cleaned {cleaned} artifact(s) from main after baseline test run"
            )


    def _run_single_test_with_baseline(
        self,
        ctx: ShepherdContext,
        test_cmd: list[str],
        display_name: str,
    ) -> PhaseResult | None:
        """Run a single test command with baseline comparison.

        This is a helper for scoped test verification. It runs the test command
        once on the worktree and compares against baseline (main branch) to
        determine if failures are new or pre-existing.

        Returns None if tests pass or failures are pre-existing.
        Returns a PhaseResult with FAILED status if new test failures are found.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return None

        # Run baseline tests on main using the same command as the worktree
        baseline_result = self._run_baseline_tests(
            ctx, test_cmd, display_name, use_provided_cmd=True
        )

        log_info(f"Running tests: {display_name}")
        ctx.report_milestone("heartbeat", action=f"verifying tests: {display_name}")

        test_start = time.time()
        try:
            result = subprocess.run(
                test_cmd,
                cwd=ctx.worktree_path,
                text=True,
                capture_output=True,
                timeout=ctx.config.test_verify_timeout,
                check=False,
                env=_build_worktree_env(ctx.worktree_path),
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
                data={"test_failure": True, "test_timeout": True},
            )
        except OSError as e:
            log_warning(f"Could not run tests ({display_name}): {e}")
            return None

        elapsed = int(time.time() - test_start)
        output = result.stdout + "\n" + result.stderr

        if result.returncode == 0:
            summary = self._parse_test_summary(output)
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
            worktree_output = output

            # Structured comparison using parsed failure counts
            structured = self._compare_test_results(baseline_output, worktree_output)

            if structured is None:
                # No new failures detected
                summary = self._parse_test_summary(worktree_output)
                msg = f"pre-existing on main ({summary})" if summary else "pre-existing on main"
                log_warning(f"Tests failed but all failures are pre-existing: {msg}")
                ctx.report_milestone(
                    "heartbeat",
                    action=f"tests have pre-existing failures ({elapsed}s)",
                )
                return None

            if structured is True:
                log_warning(
                    f"Baseline also fails but worktree introduces "
                    f"new test failures (higher failure count) for {display_name}"
                )
            # Fall through to return failure

        # Tests failed with new errors
        summary = self._parse_test_summary(output)
        tail_lines = output.strip().splitlines()[-10:]
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
        changed_files: list[str] = []
        if ctx.worktree_path:
            changed_files = get_changed_files(cwd=ctx.worktree_path)

        # Compute new error count for regression detection across doctor iterations
        new_error_count = self._compute_new_error_count(
            output,
            baseline_result.stdout + "\n" + baseline_result.stderr
            if baseline_result is not None and baseline_result.returncode != 0
            else None,
        )

        data: dict[str, object] = {
            "test_failure": True,
            "test_output_tail": tail_text,
            "test_summary": summary or "",
            "test_command": display_name,
            "changed_files": changed_files,
        }
        if new_error_count is not None:
            data["new_error_count"] = new_error_count

        return PhaseResult(
            status=PhaseStatus.FAILED,
            message=f"test verification failed ({display_name}, exit code {result.returncode})",
            phase_name="builder",
            data=data,
        )

    def _run_test_verification(self, ctx: ShepherdContext) -> PhaseResult | None:
        """Run test verification in the worktree after builder completes.

        Uses scoped test verification: determines which files changed between
        main and HEAD, then runs only the test suites relevant to those changes.
        For example, Python-only changes skip Rust tests entirely.

        Falls back to full test suite when:
        - Changed files cannot be determined (git diff fails)
        - Config files are changed (affect all languages)
        - No scoped test commands could be determined

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

        # Get changed files and determine which test suites to run
        changed_files = get_changed_files(cwd=ctx.worktree_path)

        if changed_files:
            languages = self._classify_changed_files(changed_files)
            log_info(
                f"Scoped test verification: {len(changed_files)} changed files, "
                f"languages: {sorted(languages) if languages else 'none detected'}"
            )

            # Get scoped test commands based on changed languages and files
            test_commands = self._get_scoped_test_commands(
                ctx.worktree_path, languages, changed_files
            )

            if test_commands:
                # Run scoped tests instead of full suite
                cmd_names = [name for _, name in test_commands]
                log_info(f"Running scoped tests: {', '.join(cmd_names)}")

                # Run each scoped test command
                for test_cmd, display_name in test_commands:
                    result = self._run_single_test_with_baseline(
                        ctx, test_cmd, display_name
                    )
                    if result is not None:
                        # Test failed, return early
                        return result

                # All scoped tests passed - check supplemental verification
                # using the last test command's output (stored in ctx)
                return None

        # Fall back to full test suite
        log_info("Using full test suite (no scoping or config files changed)")
        test_info = self._detect_test_command(ctx.worktree_path)
        if test_info is None:
            log_info("No test runner detected in worktree, skipping test verification")
            return None

        test_cmd, display_name = test_info

        # If the detected command is a &&-chained pipeline (e.g. check:ci:lite),
        # decompose it into individual steps to avoid short-circuit masking
        # failures in later stages (see issue #2610).
        if display_name.startswith("pnpm "):
            script_name = display_name[len("pnpm "):]
            pipeline_cmds = self._decompose_pipeline_script(
                ctx.worktree_path, script_name
            )
            if pipeline_cmds:
                log_info(
                    f"Decomposing pipeline '{display_name}' into "
                    f"{len(pipeline_cmds)} steps"
                )
                for step_cmd, step_name in pipeline_cmds:
                    result = self._run_single_test_with_baseline(
                        ctx, step_cmd, step_name
                    )
                    if result is not None:
                        return result
                return None

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
                timeout=ctx.config.test_verify_timeout,
                check=False,
                env=_build_worktree_env(ctx.worktree_path),
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
                data={"test_failure": True, "test_timeout": True},
            )
        except OSError as e:
            log_warning(f"Could not run tests ({display_name}): {e}")
            return None

        elapsed = int(time.time() - test_start)

        primary_output = result.stdout + "\n" + result.stderr

        if result.returncode == 0:
            summary = self._parse_test_summary(primary_output)
            if summary:
                log_success(f"Tests passed ({summary}, {elapsed}s)")
            else:
                log_success(f"Tests passed ({elapsed}s)")
            ctx.report_milestone(
                "heartbeat",
                action=f"tests passed ({elapsed}s)",
            )
            # Check supplemental tests for ecosystems missed by the pipeline
            return self._run_supplemental_verification(ctx, primary_output)

        # Tests failed — check if these are pre-existing failures
        if baseline_result is not None and baseline_result.returncode != 0:
            baseline_output = baseline_result.stdout + "\n" + baseline_result.stderr
            worktree_output = primary_output

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
                # Check supplemental tests for ecosystems missed by the pipeline
                return self._run_supplemental_verification(ctx, primary_output)

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
                    # Check supplemental tests for ecosystems missed by the pipeline
                    return self._run_supplemental_verification(ctx, primary_output)

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
                        # Before concluding pre-existing, try name-based comparison
                        # to detect different tests failing with same error count
                        baseline_names = self._extract_failing_test_names(
                            baseline_output
                        )
                        worktree_names = self._extract_failing_test_names(
                            worktree_output
                        )
                        if baseline_names and worktree_names:
                            new_test_names = worktree_names - baseline_names
                            if new_test_names:
                                # Different tests are failing — genuine regression
                                log_warning(
                                    f"Baseline also fails but worktree has different "
                                    f"failing tests: {new_test_names}"
                                )
                                # Fall through to failure path below
                            else:
                                # Same tests failing — pre-existing
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
                                # Check supplemental tests for ecosystems missed by the pipeline
                                return self._run_supplemental_verification(
                                    ctx, primary_output
                                )
                        else:
                            # Name extraction failed — fall back to line heuristic
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
                            # Check supplemental tests for ecosystems missed by the pipeline
                            return self._run_supplemental_verification(
                                ctx, primary_output
                            )

                # Prefer structured failure count for clearer messaging.
                # _compare_test_results returned False (structured
                # comparison failed), but one side may still parse.
                worktree_fc = self._parse_failure_count(worktree_output)
                if worktree_fc is not None:
                    log_warning(
                        f"Baseline also fails but worktree has "
                        f"{worktree_fc} test failure(s) "
                        f"({len(new_errors)} error-indicator lines in diff)"
                    )
                else:
                    log_warning(
                        f"Baseline also fails but worktree introduces "
                        f"{len(new_errors)} new error-indicator line(s)"
                    )

        # Tests failed with new errors (or no baseline available)
        summary = self._parse_test_summary(primary_output)
        combined = primary_output.strip()
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

        # Compute new error count for regression detection across doctor iterations
        baseline_output_for_count = (
            baseline_result.stdout + "\n" + baseline_result.stderr
            if baseline_result is not None and baseline_result.returncode != 0
            else None
        )
        new_error_count = self._compute_new_error_count(
            primary_output, baseline_output_for_count
        )

        data: dict[str, object] = {
            "test_failure": True,
            "test_output_tail": tail_text,
            "test_summary": summary or "",
            "test_command": display_name,
            "changed_files": changed_files,
        }
        if new_error_count is not None:
            data["new_error_count"] = new_error_count

        return PhaseResult(
            status=PhaseStatus.FAILED,
            message=f"test verification failed ({display_name}, exit code {result.returncode})",
            phase_name="builder",
            data=data,
        )

    def _is_rate_limited(self, ctx: ShepherdContext) -> bool:
        """Check if Claude API usage is too high."""
        from loom_tools.common.usage import get_usage

        try:
            data = get_usage(ctx.repo_root)
            if not isinstance(data, dict) or "error" in data:
                return False
            session_pct = data.get("session_percent", 0)
            if session_pct is None:
                return False
            return float(session_pct) >= ctx.config.rate_limit_threshold
        except Exception:
            return False

    def _filter_build_artifacts(
        self, porcelain_lines: list[str]
    ) -> tuple[list[str], list[str]]:
        """Separate meaningful changes from build artifacts in git status output.

        Returns (meaningful_lines, artifact_lines).
        """
        meaningful: list[str] = []
        artifacts: list[str] = []
        for line in porcelain_lines:
            path = parse_porcelain_path(line)
            if any(
                path == pat or path.startswith(pat)
                for pat in _BUILD_ARTIFACT_PATTERNS
            ):
                artifacts.append(line)
            else:
                meaningful.append(line)
        return meaningful, artifacts

    def _get_log_path(self, ctx: ShepherdContext) -> Path:
        """Return the expected builder log file path."""
        paths = LoomPaths(ctx.repo_root)
        return paths.builder_log_file(ctx.config.issue)

    @staticmethod
    def _snapshot_main_dirty(ctx: ShepherdContext) -> set[str]:
        """Snapshot main's dirty files (git status --porcelain lines).

        Called before the builder spawns so that _gather_diagnostics can
        distinguish pre-existing dirt from new files added by a worktree escape.
        """
        result = subprocess.run(
            ["git", "-C", str(ctx.repo_root), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            return set(
                line for line in result.stdout.splitlines() if line
            )
        return set()

    def _detect_worktree_escape(
        self, ctx: ShepherdContext
    ) -> PhaseResult | None:
        """Detect worktree escape early — before the validation/completion loop.

        When the builder escapes the worktree and modifies main instead,
        two signals are present simultaneously:
          1. Main has NEW dirty files (compared to the pre-builder baseline).
          2. The worktree is clean (0 commits ahead, no uncommitted changes).

        When both hold, completion retries are futile — they operate on the
        empty worktree — so we short-circuit to FAILED immediately.

        Also detects wrong-issue confusion: if the worktree has commits but
        they reference a different issue number than the one assigned.

        Returns:
            PhaseResult if escape or wrong-issue detected, else None.

        See issue #2630.
        """
        wt = ctx.worktree_path
        if not wt or not wt.is_dir():
            return None

        # --- Check for dirty main (worktree escape) --------------------------
        new_dirty = self._get_new_main_dirty_files(ctx)
        if new_dirty:
            # Check if worktree is empty (no real work there)
            wt_status = subprocess.run(
                ["git", "-C", str(wt), "status", "--porcelain"],
                capture_output=True, text=True, check=False,
            )
            wt_uncommitted = bool(
                wt_status.returncode == 0 and wt_status.stdout.strip()
            )
            log_res = subprocess.run(
                ["git", "-C", str(wt), "log", "--oneline", "main..HEAD"],
                capture_output=True, text=True, check=False,
            )
            commits_ahead = (
                len([l for l in log_res.stdout.splitlines() if l])
                if log_res.returncode == 0 and log_res.stdout.strip()
                else 0
            )

            if commits_ahead == 0 and not wt_uncommitted:
                # Classic escape: builder worked on main, worktree is untouched
                dirty_preview = new_dirty[:5]
                log_error(
                    f"Builder escaped worktree for issue #{ctx.config.issue} — "
                    f"main branch has {len(new_dirty)} new dirty file(s): "
                    f"{dirty_preview}"
                )
                self._cleanup_stale_worktree(ctx)
                return PhaseResult(
                    status=PhaseStatus.FAILED,
                    message=(
                        f"builder escaped worktree: main branch has "
                        f"{len(new_dirty)} new dirty file(s) but worktree "
                        f"is clean (0 commits, no uncommitted changes)"
                    ),
                    phase_name="builder",
                    data={
                        "worktree_escape": True,
                        "main_dirty_files": new_dirty[:10],
                    },
                )

        # --- Check for wrong-issue confusion ----------------------------------
        wrong_issue = self._detect_wrong_issue(ctx)
        if wrong_issue is not None:
            issue_refs, commit_messages = wrong_issue
            log_error(
                f"Builder confused: commits in worktree for issue "
                f"#{ctx.config.issue} reference other issue(s): "
                f"{issue_refs}"
            )
            self._cleanup_stale_worktree(ctx)
            return PhaseResult(
                status=PhaseStatus.FAILED,
                message=(
                    f"builder confused session: commits reference "
                    f"issue(s) {issue_refs} instead of #{ctx.config.issue}"
                ),
                phase_name="builder",
                data={
                    "wrong_issue": True,
                    "referenced_issues": sorted(issue_refs),
                    "commit_messages": commit_messages,
                },
            )

        return None

    def _get_new_main_dirty_files(self, ctx: ShepherdContext) -> list[str]:
        """Return list of NEW dirty files on main since baseline snapshot."""
        main_status = subprocess.run(
            ["git", "-C", str(ctx.repo_root), "status", "--porcelain"],
            capture_output=True, text=True, check=False,
        )
        if not (main_status.returncode == 0 and main_status.stdout.strip()):
            return []

        current = [line for line in main_status.stdout.splitlines() if line]
        if self._main_dirty_baseline is not None:
            return [f for f in current if f not in self._main_dirty_baseline]
        return current

    def _detect_wrong_issue(
        self, ctx: ShepherdContext
    ) -> tuple[set[int], list[str]] | None:
        """Check if worktree commits reference a different issue number.

        Returns (wrong_issue_numbers, commit_messages) if wrong-issue
        detected, else None.
        """
        wt = ctx.worktree_path
        if not wt or not wt.is_dir():
            return None

        log_res = subprocess.run(
            ["git", "-C", str(wt), "log", "--format=%s", "main..HEAD"],
            capture_output=True, text=True, check=False,
        )
        if log_res.returncode != 0 or not log_res.stdout.strip():
            return None

        commit_messages = [
            line for line in log_res.stdout.splitlines() if line
        ]
        if not commit_messages:
            return None

        # Find all issue references (#NNN) in commit messages
        assigned = ctx.config.issue
        other_issues: set[int] = set()
        for msg in commit_messages:
            refs = re.findall(r"#(\d+)", msg)
            for ref in refs:
                num = int(ref)
                if num != assigned and num > 0:
                    other_issues.add(num)

        # Only flag when commits reference OTHER issues but NOT the assigned one
        references_assigned = any(
            f"#{assigned}" in msg for msg in commit_messages
        )
        if other_issues and not references_assigned:
            return other_issues, commit_messages

        return None

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
        # Extract last [ERROR] lines from the log for root-cause surfacing.
        # These are included in the diagnostic summary for exit codes 6/7
        # so the shepherd output is self-contained.  See issue #2513.
        diag["log_errors"] = extract_log_errors(log_path)
        if log_path.is_file():
            try:
                log_content = log_path.read_text()
                lines = log_content.splitlines()
                diag["log_tail"] = lines[-20:] if len(lines) > 20 else lines

                # Analyze CLI output for implementation activity.
                # This distinguishes a builder that crashed mid-work from one
                # that legitimately concluded "no changes needed."
                from loom_tools.common.logging import strip_ansi

                stripped = strip_ansi(log_content)
                cli_output = _get_cli_output(stripped)
                diag["log_cli_output_length"] = len(cli_output.strip())
                diag["log_has_implementation_activity"] = bool(
                    len(cli_output.strip()) >= _SUBSTANTIVE_OUTPUT_MIN_CHARS
                    and _IMPLEMENTATION_TOOL_RE.search(cli_output)
                )
                # Check for MCP/plugin failure markers regardless of output
                # volume.  Thinking spinners can inflate output past the
                # MCP_FAILURE_MIN_OUTPUT_CHARS threshold without any real
                # tool calls, causing _is_mcp_failure() to miss it.
                diag["log_has_mcp_failure_markers"] = bool(
                    _MCP_FAILURE_MARKER_RE.search(cli_output)
                )
                # Check for degradation patterns (rate limits + Crystallizing).
                # See issue #2631.
                cli_lines = cli_output.splitlines()
                tail = cli_lines[-DEGRADED_SCAN_TAIL_LINES:]
                diag["log_has_rate_limit_warning"] = bool(
                    DEGRADED_SESSION_PATTERNS[0].search(cli_output)
                )
                diag["log_crystallizing_count"] = sum(
                    1 for line in tail
                    if DEGRADED_SESSION_PATTERNS[1].search(line)
                )
                diag["log_has_degradation_patterns"] = (
                    diag["log_has_rate_limit_warning"]
                    and diag["log_crystallizing_count"]
                    >= DEGRADED_CRYSTALLIZING_THRESHOLD
                )
            except OSError:
                diag["log_tail"] = []
                diag["log_cli_output_length"] = 0
                diag["log_has_implementation_activity"] = False
                diag["log_has_mcp_failure_markers"] = False
                diag["log_has_rate_limit_warning"] = False
                diag["log_crystallizing_count"] = 0
                diag["log_has_degradation_patterns"] = False
        else:
            diag["log_tail"] = []
            diag["log_cli_output_length"] = 0
            diag["log_has_implementation_activity"] = False
            diag["log_has_mcp_failure_markers"] = False
            diag["log_has_rate_limit_warning"] = False
            diag["log_crystallizing_count"] = 0
            diag["log_has_degradation_patterns"] = False

        # -- Low-output cause classification (set by run_phase_with_retry) ---
        # Surfaced here so the diagnostic summary is self-contained
        # without having to cross-reference the retry log.  See issue #2562.
        diag["low_output_cause"] = ctx.last_low_output_cause

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
                [line for line in log_res.stdout.splitlines() if line]
                if log_res.returncode == 0 and log_res.stdout.strip()
                else []
            )
            diag["commits_ahead"] = len(commits)

            # Files changed in commits ahead of main (for fallback detection
            # when builder accidentally commits the no-changes marker).
            # See issue #2605.
            if commits:
                diff_res = subprocess.run(
                    ["git", "-C", str(wt), "diff", "--name-only", "main..HEAD"],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                diag["committed_files"] = (
                    [f for f in diff_res.stdout.splitlines() if f]
                    if diff_res.returncode == 0 and diff_res.stdout.strip()
                    else []
                )
            else:
                diag["committed_files"] = []

            # Uncommitted changes (with file count for diagnostic prompts)
            status_res = subprocess.run(
                ["git", "-C", str(wt), "status", "--porcelain"],
                capture_output=True,
                text=True,
                check=False,
            )
            uncommitted_files = (
                [line for line in status_res.stdout.splitlines() if line]
                if status_res.returncode == 0 and status_res.stdout.strip()
                else []
            )
            meaningful_files, artifact_files = self._filter_build_artifacts(
                uncommitted_files
            )
            diag["has_uncommitted_changes"] = bool(meaningful_files)
            diag["uncommitted_file_count"] = len(meaningful_files)
            diag["artifact_file_count"] = len(artifact_files)
            diag["total_uncommitted_file_count"] = len(uncommitted_files)

            # Check for explicit "no changes needed" marker (issue #2403)
            marker_path = wt / NO_CHANGES_MARKER
            diag["no_changes_marker_exists"] = marker_path.is_file()
        else:
            diag["branch"] = None
            diag["commits_ahead"] = 0
            diag["committed_files"] = []
            diag["has_uncommitted_changes"] = False
            diag["uncommitted_file_count"] = 0
            diag["artifact_file_count"] = 0
            diag["total_uncommitted_file_count"] = 0
            diag["no_changes_marker_exists"] = False

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

        # -- Existing PR -----------------------------------------------------
        branch_name_for_pr = NamingConventions.branch_name(ctx.config.issue)
        pr_res = subprocess.run(
            [
                "gh",
                "pr",
                "list",
                "--head",
                branch_name_for_pr,
                "--state",
                "open",
                "--json",
                "number,labels",
                "--jq",
                ".[0] // empty",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            text=True,
            check=False,
        )
        diag["pr_number"] = None
        diag["pr_has_review_label"] = False
        if pr_res.returncode == 0 and pr_res.stdout.strip():
            try:
                pr_data = json.loads(pr_res.stdout.strip())
                diag["pr_number"] = pr_data.get("number")
                pr_labels = [
                    lbl.get("name", "") for lbl in pr_data.get("labels", [])
                ]
                diag["pr_has_review_label"] = "loom:review-requested" in pr_labels
            except (json.JSONDecodeError, TypeError):
                pass

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

        # -- Builder checkpoint ----------------------------------------------
        checkpoint = None
        if wt and wt.is_dir():
            checkpoint = read_checkpoint(wt)
        if checkpoint is not None:
            recommendation = get_recovery_recommendation(checkpoint)
            diag["checkpoint"] = checkpoint.to_dict()
            diag["checkpoint_stage"] = checkpoint.stage
            diag["checkpoint_recovery_path"] = recommendation["recovery_path"]
            diag["checkpoint_skip_stages"] = recommendation["skip_stages"]
        else:
            diag["checkpoint"] = None
            diag["checkpoint_stage"] = None
            diag["checkpoint_recovery_path"] = "retry_from_scratch"
            diag["checkpoint_skip_stages"] = []

        # -- Main branch state (detect worktree escape) -----------------------
        # Check if the builder accidentally modified files on main instead
        # of in the worktree.  Only flag files that are NEW since the builder
        # started — pre-existing dirty files are not evidence of escape.
        main_status = subprocess.run(
            ["git", "-C", str(ctx.repo_root), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        main_dirty_files: list[str] = []
        if main_status.returncode == 0 and main_status.stdout.strip():
            main_dirty_files = [
                line for line in main_status.stdout.splitlines() if line
            ]

        # Filter out pre-existing dirty files using the baseline snapshot
        if self._main_dirty_baseline is not None:
            new_dirty_files = [
                f for f in main_dirty_files
                if f not in self._main_dirty_baseline
            ]
        else:
            new_dirty_files = main_dirty_files

        diag["main_branch_dirty"] = bool(new_dirty_files)
        diag["main_dirty_file_count"] = len(new_dirty_files)
        diag["main_dirty_files"] = new_dirty_files[:10]  # Cap for readability

        # -- Wrong-issue detection (issue #2630) ----------------------------
        wrong = self._detect_wrong_issue(ctx)
        if wrong is not None:
            wrong_issues, wrong_msgs = wrong
            diag["wrong_issue_refs"] = sorted(wrong_issues)
            diag["wrong_issue_commit_messages"] = wrong_msgs
        else:
            diag["wrong_issue_refs"] = []
            diag["wrong_issue_commit_messages"] = []

        # -- Human-readable summary -----------------------------------------
        parts: list[str] = []
        # Surface root cause errors from the log at the front of the
        # summary so they're immediately visible in shepherd output
        # instead of buried in the log file.  See issue #2513.
        if diag["log_errors"]:
            last_error = diag["log_errors"][-1]
            parts.append(last_error)
        if diag["low_output_cause"]:
            parts.append(f"low_output_cause={diag['low_output_cause']}")
        if diag["worktree_exists"]:
            if diag["has_uncommitted_changes"]:
                uncommitted_note = f"{diag['uncommitted_file_count']} files"
            elif diag.get("artifact_file_count", 0) > 0:
                uncommitted_note = (
                    f"only build artifacts ({diag['artifact_file_count']} files)"
                )
            else:
                uncommitted_note = "none"
            parts.append(
                f"worktree exists (branch={diag['branch']}, "
                f"commits_ahead={diag['commits_ahead']}, "
                f"uncommitted={uncommitted_note})"
            )
        else:
            parts.append("worktree does not exist")
        parts.append(
            f"remote branch {'exists' if diag['remote_branch_exists'] else 'missing'}"
        )
        if diag["pr_number"] is not None:
            label_note = (
                "with loom:review-requested"
                if diag["pr_has_review_label"]
                else "missing loom:review-requested"
            )
            parts.append(f"PR #{diag['pr_number']} ({label_note})")
        else:
            parts.append("no PR")
        parts.append(f"labels=[{diag['issue_labels']}]")
        if diag.get("checkpoint_stage"):
            parts.append(f"checkpoint={diag['checkpoint_stage']}")
        if diag["main_branch_dirty"]:
            parts.append(
                f"WARNING: main branch dirty ({diag['main_dirty_file_count']} NEW files)"
            )
        elif main_dirty_files and not new_dirty_files:
            parts.append(
                f"main branch dirty ({len(main_dirty_files)} pre-existing files, ignored)"
            )
        if diag.get("wrong_issue_refs"):
            parts.append(
                f"WARNING: commits reference wrong issue(s): "
                f"{diag['wrong_issue_refs']}"
            )
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
                "**Shepherd blocked**: Builder agent was stuck and did not recover after retry. Diagnostics saved to `.loom/diagnostics/`.",
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )

        ctx.label_cache.invalidate_issue(ctx.config.issue)

    def _has_incomplete_work(self, diag: dict[str, Any]) -> bool:
        """Check if diagnostics indicate incomplete work that could be completed.

        Returns True if the worktree exists and any of these conditions hold:
        - Has commits ahead of main (needs: push, PR)
        - Has uncommitted changes AND a checkpoint exists (builder made progress)
        - Has uncommitted changes with evidence of implementation activity
          (log shows Edit/Write tool calls) or multiple substantive files
        - Remote branch exists but no PR (needs: PR creation)
        - PR exists but missing loom:review-requested label (needs: label)
        - Checkpoint indicates recoverable stage (tested, committed, pushed)
        """
        if not diag.get("worktree_exists"):
            return False

        commits_ahead = diag.get("commits_ahead", 0)

        if commits_ahead > 0:
            # Real builder progress — always treat as incomplete
            return True

        if diag.get("has_uncommitted_changes", False):
            # Uncommitted changes exist but no commits ahead.
            # Treat as incomplete if any of these prove meaningful work:
            # 1. A checkpoint exists (builder explicitly reported progress)
            checkpoint_stage = diag.get("checkpoint_stage")
            if checkpoint_stage is not None:
                return True
            # 2. Builder log shows implementation activity (Edit/Write tool calls)
            #    — the builder was actively coding, not just initialising
            if diag.get("log_has_implementation_activity", False):
                return True
            # 3. Multiple substantive uncommitted files suggest real work
            #    (a single file could be a marker or artifact, but ≥2 is
            #    strong evidence of implementation)
            if diag.get("uncommitted_file_count", 0) >= 2:
                return True
            # No evidence of meaningful work → full retry is more appropriate
            return False

        # Remote branch exists but no PR — only incomplete if there are
        # commits to PR.  A remote branch with 0 commits ahead of main
        # (e.g. branch pushed during worktree setup) has nothing to PR.
        if (
            diag.get("remote_branch_exists")
            and diag.get("pr_number") is None
            and commits_ahead > 0
        ):
            return True

        # PR exists but missing the review label
        if (
            diag.get("pr_number") is not None
            and not diag.get("pr_has_review_label", False)
        ):
            return True

        # Check checkpoint for recoverable stages
        # If checkpoint shows progress past 'implementing', there may be work to salvage
        checkpoint_stage = diag.get("checkpoint_stage")
        if checkpoint_stage in ("tested", "committed", "pushed"):
            # Checkpoint indicates work was done that may be recoverable
            # even if git state doesn't show it (edge case: changes were stashed, etc.)
            return True

        return False

    def _recover_from_existing_worktree(
        self,
        ctx: ShepherdContext,
        diag: dict[str, Any],
        exit_code: int,
    ) -> PhaseResult | None:
        """Attempt to recover from builder low-output/MCP failure using worktree state.

        When the builder CLI can't start (auth timeout, nesting protection, MCP
        failure) but a previous builder run left meaningful work in the worktree,
        try to complete the commit/push/PR workflow mechanically.

        Returns a PhaseResult if recovery succeeded or failed definitively,
        or None if no recovery was possible (caller should fall through to
        generic failure handling).

        See issue #2507.
        """
        if not self._has_incomplete_work(diag):
            return None

        log_info(
            f"Builder exited with code {exit_code} but worktree has "
            f"incomplete work for issue #{ctx.config.issue} — "
            f"attempting recovery"
        )

        # Try direct completion first (push, PR creation, label addition)
        # — avoids spawning an LLM agent for purely mechanical steps.
        if self._direct_completion(ctx, diag):
            log_success(
                f"Recovered incomplete work for issue #{ctx.config.issue} "
                f"via direct completion after builder exit code {exit_code}"
            )
            diag = self._gather_diagnostics(ctx)
            if diag.get("pr_number") is not None:
                ctx.pr_number = diag["pr_number"]
                ctx.report_milestone("pr_created", pr_number=diag["pr_number"])
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=(
                        f"builder phase complete - PR #{diag['pr_number']} "
                        f"created (recovered from exit code {exit_code})"
                    ),
                    phase_name="builder",
                    data={
                        "pr_number": diag["pr_number"],
                        "exit_code": exit_code,
                        "recovered_from_worktree": True,
                    },
                )

        # Direct completion couldn't handle it (non-mechanical steps remain).
        # Try a focused completion phase agent.
        completion_exit = self._run_completion_phase(ctx, diag)
        if completion_exit == 0:
            diag = self._gather_diagnostics(ctx)
            if diag.get("pr_number") is not None:
                ctx.pr_number = diag["pr_number"]
                ctx.report_milestone("pr_created", pr_number=diag["pr_number"])
                return PhaseResult(
                    status=PhaseStatus.SUCCESS,
                    message=(
                        f"builder phase complete - PR #{diag['pr_number']} "
                        f"created (completion phase recovered from "
                        f"exit code {exit_code})"
                    ),
                    phase_name="builder",
                    data={
                        "pr_number": diag["pr_number"],
                        "exit_code": exit_code,
                        "recovered_from_worktree": True,
                    },
                )

        # Neither recovery path succeeded
        log_warning(
            f"Worktree recovery failed for issue #{ctx.config.issue} "
            f"(direct_completion=False, completion_phase={completion_exit})"
        )
        return None

    def _is_no_changes_needed(self, diag: dict[str, Any]) -> bool:
        """Check if diagnostics indicate "no changes needed" condition.

        Requires an **explicit positive signal** — the builder must have
        written a ``.no-changes-needed`` marker file in the worktree root
        to confirm it deliberately decided no code changes are required.

        Without the marker file, an empty worktree is treated as a builder
        failure (crash, timeout, OOM kill) rather than an intentional "no
        changes" decision.  See issue #2403.

        If the builder log shows substantive implementation activity (Edit/Write
        tool calls with significant output), the builder was actively working
        but crashed or timed out before committing.  This is a builder failure,
        not a "no changes needed" determination.  See issue #2425.

        If main has NEW uncommitted changes (compared to pre-builder baseline),
        the builder may have escaped the worktree and modified files on main
        instead.  This is NOT a "no changes needed" situation — it's a
        worktree escape bug.  Pre-existing dirty files are excluded from this
        check (see issue #2457).
        """
        if not diag.get("worktree_exists"):
            return False

        # If main branch has NEW dirty files (not pre-existing), the builder
        # may have escaped the worktree and made changes on main instead.
        # Never treat this as "no changes needed".
        if diag.get("main_branch_dirty", False):
            return False

        # All indicators of work must be absent — but if the builder
        # accidentally committed *only* the marker file, treat that as
        # equivalent to no work done (defense-in-depth for issue #2605).
        commits_ahead = diag.get("commits_ahead", 0)
        committed_files = diag.get("committed_files", [])
        only_marker_committed = (
            commits_ahead > 0
            and committed_files == [NO_CHANGES_MARKER]
        )

        has_any_work = (
            diag.get("has_uncommitted_changes", False)
            or (commits_ahead > 0 and not only_marker_committed)
            or diag.get("remote_branch_exists", False)
            or diag.get("pr_number") is not None
        )

        if has_any_work:
            return False

        if only_marker_committed:
            log_warning(
                "Builder accidentally committed the .no-changes-needed marker "
                "file — treating as 'no changes needed' anyway (issue #2605)"
            )

        # Session quality gate: if the builder log shows too little output,
        # the session was degraded/failed and never actually analyzed the issue.
        # Don't mistake a failed session for "no changes needed."  (Issue #2436)
        cli_output_len = diag.get("log_cli_output_length", 0)
        if cli_output_len < _MIN_ANALYSIS_OUTPUT_CHARS:
            log_warning(
                f"Builder session too short to conclude 'no changes needed' "
                f"({cli_output_len} chars of output, minimum "
                f"{_MIN_ANALYSIS_OUTPUT_CHARS}) — treating as builder failure"
            )
            return False

        # MCP/plugin failure markers in the CLI output indicate the builder
        # session was broken by infrastructure issues, not a legitimate
        # analysis.  Thinking spinners can inflate output past the volume
        # threshold used by _is_mcp_failure() in base.py, so we check
        # markers unconditionally here.  See issue #2464.
        if diag.get("log_has_mcp_failure_markers", False):
            log_warning(
                "Builder log shows MCP/plugin failure markers but no git "
                "artifacts — treating as builder failure, not 'no changes "
                "needed'"
            )
            return False

        # No git artifacts — but check the builder log for signs of
        # implementation activity.  A builder that made Edit/Write tool calls
        # with substantial output was actively implementing, not concluding
        # "no changes needed."  It likely crashed or timed out before
        # committing.  Treat this as a builder failure instead.
        if diag.get("log_has_implementation_activity", False):
            log_warning(
                "Builder log shows implementation activity (Edit/Write tool "
                "calls) but no git artifacts — treating as builder failure, "
                "not 'no changes needed'"
            )
            return False

        # Require explicit marker file from the builder (issue #2403).
        # Without this positive signal, an empty worktree is
        # indistinguishable from a builder that was killed/crashed before
        # producing any work.
        if not diag.get("no_changes_marker_exists", False):
            log_warning(
                "No .no-changes-needed marker file found in worktree — "
                "treating empty worktree as builder failure, not "
                "'no changes needed' (issue #2403)"
            )
            return False

        return True

    def _diagnose_remaining_steps(self, diag: dict[str, Any], issue: int) -> list[str]:
        """Determine exactly which steps remain to complete the workflow.

        Returns a list of step identifiers like "stage_and_commit",
        "push_branch", "create_pr", or "add_review_label".
        """
        steps: list[str] = []

        if diag.get("has_uncommitted_changes"):
            steps.append("stage_and_commit")

        if diag.get("commits_ahead", 0) > 0 and not diag.get("remote_branch_exists"):
            steps.append("push_branch")
        elif not diag.get("remote_branch_exists") and diag.get("has_uncommitted_changes"):
            # Will need push after committing
            steps.append("push_branch")

        if diag.get("pr_number") is None:
            # Only create PR if there are (or will be) commits ahead of main.
            # "push_branch" in steps means commits exist or will exist after
            # stage_and_commit.  remote_branch_exists alone is not enough —
            # the branch may have 0 commits ahead (e.g. pushed during worktree
            # setup), and creating a PR with an empty diff is useless.
            has_or_will_have_commits = (
                "push_branch" in steps
                or (
                    diag.get("remote_branch_exists")
                    and diag.get("commits_ahead", 0) > 0
                )
            )
            if has_or_will_have_commits:
                steps.append("create_pr")
        elif not diag.get("pr_has_review_label", False):
            steps.append("add_review_label")

        return steps

    def _direct_completion(
        self, ctx: ShepherdContext, diag: dict[str, Any]
    ) -> bool:
        """Attempt to complete mechanical operations directly in Python.

        Handles simple operations (stage/commit, push branch, create PR, add
        label) without spawning a full agent. Returns True if all remaining
        steps were completed.
        """
        steps = self._diagnose_remaining_steps(diag, ctx.config.issue)

        # Only handle purely mechanical steps directly
        mechanical_steps = {
            "stage_and_commit", "push_branch", "add_review_label", "create_pr",
        }
        if not steps or not set(steps).issubset(mechanical_steps):
            return False

        # Safety guard: refuse to create a PR when there are 0 commits
        # ahead of main AND we're not about to create one via stage_and_commit.
        if (
            "create_pr" in steps
            and diag.get("commits_ahead", 0) == 0
            and "stage_and_commit" not in steps
        ):
            log_warning(
                f"Direct completion: refusing to create PR for issue "
                f"#{ctx.config.issue} with 0 commits ahead of main"
            )
            return False

        log_info(
            f"Attempting direct completion for issue #{ctx.config.issue}: "
            f"{steps}"
        )

        for step in steps:
            if step == "stage_and_commit":
                if not self._stage_and_commit(ctx):
                    log_warning("Direct completion: stage_and_commit failed")
                    return False
                log_success("Direct completion: changes committed")
                if ctx.worktree_path:
                    write_checkpoint(
                        ctx.worktree_path, "committed",
                        issue=ctx.config.issue, quiet=True,
                    )

            elif step == "push_branch":
                if not self._push_branch(ctx):
                    log_warning("Direct completion: push failed")
                    return False
                log_success("Direct completion: branch pushed")
                if ctx.worktree_path:
                    write_checkpoint(
                        ctx.worktree_path, "pushed",
                        issue=ctx.config.issue, quiet=True,
                    )

            elif step == "create_pr":
                branch = diag.get(
                    "branch",
                    NamingConventions.branch_name(ctx.config.issue),
                )
                # Guard against stale diagnostics: re-check if a PR already
                # exists for this branch before creating a new one.
                check_result = subprocess.run(
                    [
                        "gh", "pr", "list",
                        "--head", branch,
                        "--state", "open",
                        "--json", "number",
                        "--jq", ".[0].number // empty",
                    ],
                    cwd=ctx.repo_root,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                existing_pr = check_result.stdout.strip()
                if existing_pr:
                    log_info(
                        f"Direct completion: PR #{existing_pr} already exists "
                        f"for branch {branch}, skipping creation"
                    )
                    continue
                result = subprocess.run(
                    [
                        "gh",
                        "pr",
                        "create",
                        "--head",
                        branch,
                        "--title",
                        NamingConventions.pr_title(ctx.issue_title, ctx.config.issue),
                        "--label",
                        "loom:review-requested",
                        "--body",
                        f"Closes #{ctx.config.issue}",
                    ],
                    cwd=ctx.repo_root,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if result.returncode != 0:
                    log_warning(
                        f"Direct completion: gh pr create failed: "
                        f"{result.stderr.strip()[:200]}"
                    )
                    return False
                log_success(
                    f"Direct completion: PR created for issue #{ctx.config.issue}"
                )
                if ctx.worktree_path:
                    write_checkpoint(
                        ctx.worktree_path, "pr_created",
                        issue=ctx.config.issue, quiet=True,
                    )

            elif step == "add_review_label":
                pr_num = diag.get("pr_number")
                if pr_num is None:
                    return False
                result = subprocess.run(
                    [
                        "gh",
                        "pr",
                        "edit",
                        str(pr_num),
                        "--add-label",
                        "loom:review-requested",
                    ],
                    cwd=ctx.repo_root,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if result.returncode != 0:
                    log_warning(
                        f"Direct completion: failed to add label to PR #{pr_num}"
                    )
                    return False
                log_success(f"Direct completion: added loom:review-requested to PR #{pr_num}")

        return True

    def _run_completion_phase(
        self,
        ctx: ShepherdContext,
        diag: dict[str, Any],
        attempt: int = 1,
    ) -> int:
        """Run a focused completion phase to finish incomplete work.

        Spawns a builder session with explicit instructions to complete
        the commit/push/PR workflow based on current worktree state.

        Args:
            ctx: Shepherd context
            diag: Diagnostics from _gather_diagnostics
            attempt: Current attempt number (1-based), used for
                progressive instruction simplification

        Returns:
            Exit code from the completion worker (0=success)
        """
        from loom_tools.shepherd.phases.base import run_phase_with_retry

        steps = self._diagnose_remaining_steps(diag, ctx.config.issue)

        # Build targeted completion instructions based on remaining steps
        instructions: list[str] = []

        if "stage_and_commit" in steps:
            if attempt >= 2:
                instructions.append(
                    "- Run: git add -A && git commit -m 'Implement changes for issue "
                    f"#{ctx.config.issue}'"
                )
            else:
                instructions.append("- Stage and commit all changes")

        if "push_branch" in steps:
            branch = diag.get("branch", f"feature/issue-{ctx.config.issue}")
            instructions.append(
                f"- Push branch to remote: git push -u origin {branch}"
            )

        if "create_pr" in steps:
            branch = diag.get("branch", f"feature/issue-{ctx.config.issue}")
            instructions.append(
                f"- First check if a PR already exists: "
                f"gh pr list --head {branch} --state open"
            )
            if attempt >= 2:
                instructions.append(
                    f"- Only if no PR exists, run: gh pr create "
                    f"--title {shlex.quote(NamingConventions.pr_title(ctx.issue_title, ctx.config.issue))} "
                    f"--label loom:review-requested "
                    f"--body 'Closes #{ctx.config.issue}'"
                )
            else:
                instructions.append(
                    f"- Only if no PR exists, create PR with loom:review-requested label "
                    f"using 'Closes #{ctx.config.issue}' in body"
                )

        if "add_review_label" in steps:
            pr_num = diag.get("pr_number")
            instructions.append(
                f"- Run: gh pr edit {pr_num} --add-label loom:review-requested"
            )

        if not instructions:
            instructions.append("- Verify PR was created successfully with gh pr view")

        instruction_text = "\n".join(instructions)

        log_info(f"Running completion phase for issue #{ctx.config.issue} (attempt {attempt})")
        log_info(f"Remaining steps: {steps}")
        log_info(f"Instructions:\n{instruction_text}")

        # Use a special completion prompt as args
        # IMPORTANT: Args must be single-line because they're passed through tmux send-keys.
        # Newlines break shell command parsing (causes "dquote>" prompts).
        # Join instructions with semicolons instead of newlines.
        instruction_oneline = "; ".join(instructions)

        # Build diagnostic context for retry attempts
        # Include checkpoint info and state details to help the agent understand
        # the exact situation
        diag_context = ""

        # Always include checkpoint info if available (more reliable than git state inference)
        checkpoint_stage = diag.get("checkpoint_stage")
        if checkpoint_stage:
            checkpoint_info = f"Builder checkpoint indicates progress through '{checkpoint_stage}' stage."
            checkpoint_details = diag.get("checkpoint", {}).get("details", {})
            if checkpoint_details.get("test_result"):
                checkpoint_info += f" Tests: {checkpoint_details['test_result']}."
            diag_context = f" {checkpoint_info}"

        if attempt >= 2:
            diag_parts = []
            if diag.get("uncommitted_file_count", 0) > 0:
                diag_parts.append(
                    f"there are {diag['uncommitted_file_count']} uncommitted files"
                )
            if diag.get("commits_ahead", 0) > 0:
                diag_parts.append(
                    f"the branch has {diag['commits_ahead']} commits ahead of main"
                )
            elif diag.get("remote_branch_exists"):
                diag_parts.append(
                    "the remote branch exists but has no commits ahead of main"
                )
            else:
                diag_parts.append("the remote branch does not exist yet")
            if diag.get("pr_number"):
                if diag.get("pr_has_review_label"):
                    diag_parts.append(
                        f"PR #{diag['pr_number']} exists with loom:review-requested"
                    )
                else:
                    diag_parts.append(
                        f"PR #{diag['pr_number']} exists but is missing loom:review-requested label"
                    )
            if diag_parts:
                diag_context = f" Current state: {', '.join(diag_parts)}."

        completion_args = (
            f"COMPLETION_MODE: Your previous session ended before completing the workflow. "
            f"You are in worktree .loom/worktrees/issue-{ctx.config.issue} with changes ready.{diag_context} "
            f"Complete these steps: {instruction_oneline}. "
            f"Do NOT implement anything new - just complete the git/PR workflow."
        )

        exit_code = run_phase_with_retry(
            ctx,
            role="builder",
            name=f"builder-complete-{ctx.config.issue}",
            timeout=300,  # 5 minutes should be enough to commit/push/PR
            max_retries=1,
            phase="builder",
            worktree=ctx.worktree_path,
            args=completion_args,
        )

        return exit_code

    def _is_stale_worktree(self, worktree_path: Path) -> bool:
        """Check if an existing worktree is stale (abandoned without commits).

        A stale worktree is one where a previous builder:
        1. Created the worktree
        2. Failed/crashed/timed out before making any commits
        3. Left the worktree in place

        Returns True if the worktree exists, has no commits ahead of main,
        and has no uncommitted changes. This indicates the worktree can be
        safely reset or removed for a fresh start.
        """
        if not worktree_path.is_dir():
            return False

        # Check for uncommitted changes
        status_result = subprocess.run(
            ["git", "-C", str(worktree_path), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        if status_result.returncode != 0:
            return False  # Can't determine status, don't treat as stale
        uncommitted = (
            [line for line in status_result.stdout.splitlines() if line]
            if status_result.stdout.strip()
            else []
        )
        meaningful, _ = self._filter_build_artifacts(uncommitted)
        if meaningful:
            return False  # Has meaningful uncommitted changes, not stale

        # Check for commits ahead of main
        # Use origin/main instead of @{upstream} because the branch may not
        # have an upstream tracking branch set yet
        log_result = subprocess.run(
            ["git", "-C", str(worktree_path), "log", "--oneline", "origin/main..HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
        if log_result.returncode != 0:
            return False  # Can't determine commit status
        if log_result.stdout.strip():
            return False  # Has commits ahead of main, not stale

        # Worktree exists with no commits and no changes - it's stale
        return True

    def _reset_stale_worktree(self, ctx: ShepherdContext) -> None:
        """Reset a stale worktree to match origin/main.

        This method is called when we detect a stale worktree from a previous
        builder that crashed before making any commits. Instead of removing
        and recreating the worktree, we reset it to origin/main and let the
        builder continue fresh.

        This approach:
        1. Preserves the worktree directory structure
        2. Ensures the branch is in sync with main
        3. Is faster than remove + recreate
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return

        # Fetch latest from origin to ensure we have current main
        subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "fetch", "origin", "main"],
            capture_output=True,
            check=False,
        )

        # Hard reset to origin/main
        result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "reset", "--hard", "origin/main"],
            capture_output=True,
            text=True,
            check=False,
        )

        if result.returncode == 0:
            log_info(f"Reset stale worktree {ctx.worktree_path} to origin/main")
        else:
            # If reset fails, fall back to removing the worktree entirely
            log_warning(
                f"Failed to reset stale worktree, removing it: "
                f"{result.stderr.strip() or result.stdout.strip()}"
            )
            self._remove_stale_worktree(ctx)

    def _remove_stale_worktree(self, ctx: ShepherdContext) -> None:
        """Remove a stale worktree and its branches (local and remote).

        Called as a fallback when resetting fails. Removes the worktree
        and deletes both local and remote branches so worktree.sh can
        recreate cleanly without stale artifacts (issue #2415).
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return

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

        if branch and branch != "main":
            # Delete local branch
            subprocess.run(
                ["git", "-C", str(ctx.repo_root), "branch", "-D", branch],
                capture_output=True,
                check=False,
            )

            # Delete remote branch to prevent stale artifacts (issue #2415)
            del_result = subprocess.run(
                [
                    "git", "-C", str(ctx.repo_root),
                    "push", "origin", "--delete", branch,
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if del_result.returncode == 0:
                log_info(f"Deleted stale remote branch {branch}")

        log_info(f"Removed stale worktree {ctx.worktree_path} and branch {branch}")

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

            # Remove empty branch (local and remote, issue #2415)
            if branch and branch != "main":
                subprocess.run(
                    ["git", "-C", str(ctx.repo_root), "branch", "-d", branch],
                    capture_output=True,
                    check=False,
                )
                subprocess.run(
                    [
                        "git", "-C", str(ctx.repo_root),
                        "push", "origin", "--delete", branch,
                    ],
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

        # Revert issue label so it can be picked up again (atomic transition)
        transition_issue_labels(
            ctx.config.issue,
            add=["loom:issue"],
            remove=["loom:building"],
            repo_root=ctx.repo_root,
        )
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        log_info(f"Cleaned up worktree and reverted labels for issue #{ctx.config.issue}")

    def push_branch(self, ctx: ShepherdContext) -> bool:
        """Push the current branch to remote.

        Returns True if the push succeeded or the branch was already pushed.
        Public wrapper so the shepherd orchestrator can trigger a push
        (e.g. after the doctor applies test fixes).
        """
        return self._push_branch(ctx)

    def _stage_and_commit(self, ctx: ShepherdContext) -> bool:
        """Stage meaningful changes and create a commit in the worktree.

        Stages all non-artifact files (respects .gitignore) and commits with
        a descriptive message.  Returns True if a commit was created.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return False

        # Get current status
        status_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        if status_result.returncode != 0 or not status_result.stdout.strip():
            log_warning("Direct completion: no changes to commit")
            return False

        # Split without stripping the full output — the leading status
        # chars (e.g. " M") are significant in porcelain format.
        porcelain_lines = [
            line for line in status_result.stdout.splitlines() if line
        ]
        meaningful, _ = self._filter_build_artifacts(porcelain_lines)
        if not meaningful:
            log_warning(
                "Direct completion: only build artifacts uncommitted, "
                "nothing meaningful to commit"
            )
            return False

        # Extract file paths from porcelain lines (format: "XY filename")
        files_to_stage = []
        for line in meaningful:
            path = parse_porcelain_path(line)
            if path:
                files_to_stage.append(path)

        if not files_to_stage:
            return False

        # Stage the files
        stage_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "add", "--"] + files_to_stage,
            capture_output=True,
            text=True,
            check=False,
        )
        if stage_result.returncode != 0:
            log_warning(
                f"Direct completion: git add failed: "
                f"{(stage_result.stderr or '').strip()[:200]}"
            )
            return False

        # Commit
        commit_msg = f"feat: implement changes for issue #{ctx.config.issue}"
        commit_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "commit", "-m", commit_msg],
            capture_output=True,
            text=True,
            check=False,
        )
        if commit_result.returncode != 0:
            log_warning(
                f"Direct completion: git commit failed: "
                f"{(commit_result.stderr or '').strip()[:200]}"
            )
            return False

        return True

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

    def _has_uncommitted_changes(self, ctx: ShepherdContext) -> bool:
        """Check if the worktree has uncommitted changes (staged or unstaged)."""
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return False

        result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        return bool(result.returncode == 0 and result.stdout.strip())

    def _commit_interrupted_work(self, ctx: ShepherdContext, reason: str) -> bool:
        """Commit and push uncommitted work when builder is interrupted.

        When the builder exits abnormally with uncommitted changes, this method:
        1. Stages all changes (git add -A)
        2. Creates a WIP commit with descriptive message
        3. Pushes the commit to remote
        4. Labels the issue as loom:needs-fix for Doctor to pick up
        5. Adds a comment explaining the situation

        Args:
            ctx: Shepherd context
            reason: Brief description of why the builder was interrupted

        Returns:
            True if work was successfully committed and pushed, False otherwise.
        """
        if not ctx.worktree_path or not ctx.worktree_path.is_dir():
            return False

        if not self._has_uncommitted_changes(ctx):
            return False

        # Filter build artifacts — don't create WIP commits for artifact-only
        # changes (e.g. Cargo.lock from post-worktree hooks)
        status_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
        if status_result.returncode == 0 and status_result.stdout.strip():
            porcelain_lines = [
                line for line in status_result.stdout.splitlines() if line
            ]
            meaningful, _ = self._filter_build_artifacts(porcelain_lines)
            if not meaningful:
                log_info(
                    "Skipping WIP commit: only build artifact files are "
                    "uncommitted (no meaningful changes)"
                )
                return False

        log_info("Builder interrupted with uncommitted changes, preserving work")

        # Stage all changes
        stage_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "add", "-A"],
            capture_output=True,
            text=True,
            check=False,
        )
        if stage_result.returncode != 0:
            log_warning(
                f"Failed to stage changes: "
                f"{(stage_result.stderr or stage_result.stdout or '').strip()[:200]}"
            )
            return False

        # Create WIP commit
        commit_msg = (
            f"WIP: Builder interrupted for issue #{ctx.config.issue}\n\n"
            f"Reason: {reason}\n\n"
            f"This commit contains uncommitted work from a builder session that\n"
            f"was interrupted before completion. The Doctor or a subsequent\n"
            f"Builder can continue from this point."
        )
        commit_result = subprocess.run(
            ["git", "-C", str(ctx.worktree_path), "commit", "-m", commit_msg],
            capture_output=True,
            text=True,
            check=False,
        )
        if commit_result.returncode != 0:
            log_warning(
                f"Failed to create WIP commit: "
                f"{(commit_result.stderr or commit_result.stdout or '').strip()[:200]}"
            )
            return False

        log_info("Created WIP commit for interrupted work")

        # Push to remote
        if not self._push_branch(ctx):
            log_warning("Could not push WIP commit to remote")
            # Continue anyway - local commit still preserves work

        # Transition label: loom:building -> loom:needs-fix
        transition_issue_labels(
            ctx.config.issue,
            add=["loom:needs-fix"],
            remove=["loom:building"],
            repo_root=ctx.repo_root,
        )
        ctx.label_cache.invalidate_issue(ctx.config.issue)

        # Write context file for Doctor phase
        branch_name = NamingConventions.branch_name(ctx.config.issue)
        context_data = {
            "issue": ctx.config.issue,
            "failure_message": f"Builder interrupted: {reason}",
            "interrupted": True,
            "wip_commit": True,
        }
        context_file = ctx.worktree_path / ".loom-interrupted-context.json"
        try:
            context_file.write_text(json.dumps(context_data, indent=2))
            log_info(f"Wrote interrupted context to {context_file}")
        except OSError as e:
            log_warning(f"Could not write interrupted context: {e}")

        # Add comment with context
        worktree_rel = f".loom/worktrees/issue-{ctx.config.issue}"
        comment = (
            f"**Shepherd**: Builder was interrupted with uncommitted work. "
            f"Changes have been committed as a WIP commit and pushed.\n\n"
            f"- **Reason**: {reason}\n"
            f"- **Branch**: `{branch_name}`\n"
            f"- **Worktree**: `{worktree_rel}`\n\n"
            f"The Doctor or a subsequent Builder can pick this up and "
            f"continue from the WIP commit."
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
            reason="builder_interrupted",
            details=reason,
        )

        log_info(
            f"Preserved interrupted work for issue #{ctx.config.issue} "
            f"(WIP commit, labeled loom:needs-fix)"
        )
        return True

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

        # Transition label: loom:building -> loom:needs-fix (atomic)
        transition_issue_labels(
            ctx.config.issue,
            add=["loom:needs-fix"],
            remove=["loom:building"],
            repo_root=ctx.repo_root,
        )
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
                context_file.write_text(json.dumps(context_data, indent=2) + "\n")
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

        Note: For umbrella commands like ``pnpm check:ci:lite`` that run
        multiple test ecosystems, this returns "pnpm" based on the command
        name. Use :meth:`_detect_test_ecosystem_from_output` with actual
        test output for more accurate ecosystem attribution.
        """
        cmd_str = " ".join(test_cmd)
        if "cargo" in cmd_str:
            return "cargo"
        if any(kw in cmd_str for kw in ("pnpm", "npm", "vitest", "jest")):
            return "pnpm"
        if any(kw in cmd_str for kw in ("pytest", "python")):
            return "pytest"
        return None

    def _detect_test_ecosystem_from_output(self, output: str) -> str | None:
        """Determine which test ecosystem failed by parsing test output.

        For umbrella test commands like ``pnpm check:ci:lite`` that run
        multiple test ecosystems (TypeScript, Rust, Python), this parses
        the actual output to identify which test runner produced failures.

        This provides more accurate ecosystem attribution than command-based
        detection, enabling correct pre-check filtering for Doctor recovery.

        Args:
            output: Test command output (stdout + stderr combined).

        Returns:
            One of "cargo", "pnpm", "pytest", or None if unknown.
            Returns the ecosystem of the **first failure found** when
            scanning from the end of output (most recent failure).
        """
        # Scan lines from end to find the most recent failure pattern
        lines = output.strip().splitlines()

        for line in reversed(lines):
            stripped = line.strip()
            cleaned = stripped.strip("= ").strip()

            # cargo test failure: "test result: FAILED. N passed; N failed"
            if stripped.startswith("test result:") and "FAILED" in stripped:
                return "cargo"

            # cargo multi-target failure: "error: N target(s) failed:"
            if re.match(r"error:\s+\d+\s+targets?\s+failed", stripped):
                return "cargo"

            # cargo test individual failure: "test some::path ... FAILED"
            if re.match(r"test\s+\S+\s+\.\.\.\s+FAILED", stripped):
                return "cargo"

            # pytest failure: "FAILED tests/test_foo.py::test_bar"
            if stripped.startswith("FAILED ") and "::" in stripped:
                return "pytest"

            # pytest summary failure: "N failed" with "passed" in same line
            # (inside = borders like "== 1 failed, 14 passed in 2.45s ==")
            if (
                stripped.startswith("=")
                and "failed" in cleaned.lower()
                and "passed" in cleaned.lower()
            ):
                # Distinguish pytest from vitest by looking for "in Xs" pattern
                # pytest: "1 failed, 14 passed in 2.45s"
                # vitest: "Tests  1 failed, 14 passed"
                if re.search(r"in\s+[\d.]+s", cleaned):
                    return "pytest"

            # vitest/jest failure: "FAIL src/foo.test.ts" (at line start)
            if stripped.startswith("FAIL ") and "/" in stripped:
                return "pnpm"

            # vitest/jest summary: "Tests  N failed" (note double space)
            if re.match(r"Tests\s+\d+\s+failed", stripped, re.IGNORECASE):
                return "pnpm"

        return None

    def should_skip_doctor_recovery(
        self, ctx: ShepherdContext, test_cmd: list[str], test_output: str | None = None
    ) -> bool:
        """Check if Doctor recovery should be skipped for test failures.

        Compares the builder's changed files against the failing test
        ecosystem. If none of the changed files could plausibly affect
        the failing tests, the failures are pre-existing and Doctor
        recovery should be skipped.

        For umbrella test commands like ``pnpm check:ci:lite`` that run
        multiple test ecosystems, providing ``test_output`` enables
        accurate ecosystem attribution by parsing which test runner
        actually failed, rather than inferring from the command name.

        Args:
            ctx: Shepherd context with worktree path.
            test_cmd: The test command that was run.
            test_output: Optional test output for output-based ecosystem
                detection. When provided and contains recognizable failure
                patterns, this takes precedence over command-based detection.

        Returns:
            True if Doctor should be skipped (no overlap between changed
            files and failing test ecosystem).
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

        # Determine which ecosystem the failing tests belong to.
        # Prefer output-based detection (more accurate for umbrella commands),
        # fall back to command-based detection.
        test_ecosystem = None
        if test_output:
            test_ecosystem = self._detect_test_ecosystem_from_output(test_output)
            if test_ecosystem:
                log_info(f"Detected test ecosystem from output: {test_ecosystem}")

        if test_ecosystem is None:
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
