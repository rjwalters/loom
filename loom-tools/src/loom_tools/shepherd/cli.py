"""CLI entry point for shepherd orchestration."""

from __future__ import annotations

import argparse
import io
import os
import shlex
import sys
import time
from pathlib import Path

from loom_tools.common.config import env_int
from loom_tools.common.git import (
    attempt_rebase,
    get_uncommitted_files,
    is_branch_behind,
    parse_porcelain_path,
    run_git,
)
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_daemon_state
from loom_tools.shepherd.config import ExecutionMode, Phase, QualityGates, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueIsEpicError,
    IssueNotFoundError,
    ShepherdError,
    ShutdownSignal,
)
from loom_tools.shepherd.exit_codes import ShepherdExitCode
from loom_tools.shepherd.labels import get_pr_for_issue, transition_issue_labels, transition_labels
from loom_tools.shepherd.phases import (
    ApprovalPhase,
    BuilderPhase,
    CuratorPhase,
    DoctorPhase,
    JudgePhase,
    MergePhase,
    PhaseStatus,
    PreflightPhase,
    RebasePhase,
)
from loom_tools.shepherd.phases.base import PhaseResult


def _print_phase_header(title: str) -> None:
    """Print a phase header with formatting to stderr for consistent ordering."""
    width = 67
    print(file=sys.stderr, flush=True)
    print(f"\033[0;36m{'═' * width}\033[0m", file=sys.stderr, flush=True)
    print(f"\033[0;36m  {title}\033[0m", file=sys.stderr, flush=True)
    print(f"\033[0;36m{'═' * width}\033[0m", file=sys.stderr, flush=True)


def _get_prior_failure_info(
    repo_root: Path, issue: int
) -> tuple[int, int, str | None]:
    """Get prior failure count for an issue from daemon state.

    Reads ``recent_failures`` from ``daemon-state.json`` and filters for the
    given issue number.

    Args:
        repo_root: Repository root path.
        issue: Issue number to check.

    Returns:
        Tuple of (failure_count, threshold, last_error_class).
        ``last_error_class`` is ``None`` when there are no prior failures.
    """
    threshold = env_int("LOOM_SYSTEMATIC_FAILURE_THRESHOLD", default=3)
    try:
        state = read_daemon_state(repo_root)
    except Exception:
        return 0, threshold, None

    issue_failures = [f for f in state.recent_failures if f.issue == issue]
    count = len(issue_failures)
    last_error = issue_failures[-1].error_class if issue_failures else None
    return count, threshold, last_error


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse command line arguments."""
    parser = argparse.ArgumentParser(
        prog="loom-shepherd",
        description="Shepherd orchestration for issue lifecycle management",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
PHASES:
    1. Curator    - Enhance issue with implementation guidance
    2. Approval   - Wait for loom:issue label (or auto-approve in force mode)
    3. Builder    - Create worktree, implement, create PR
    4. Judge      - Review PR, approve or request changes (always runs, even in force mode)
    5. Doctor     - Address requested changes (if any)
    6. Merge      - Auto-merge (--force/--merge) or exit at loom:pr (default)

EXIT CODES:
    0  SUCCESS            - Full success (merged/approved)
    1  BUILDER_FAILED     - No PR created (builder failed)
    2  PR_TESTS_FAILED    - PR created but tests failed
    3  SHUTDOWN           - Shutdown signal received
    4  NEEDS_INTERVENTION - Stuck/blocked, needs human intervention
    5  SKIPPED            - Issue already complete (no action needed)
    6  NO_CHANGES_NEEDED  - No changes determined, issue marked blocked
    9  SYSTEMIC_FAILURE   - Auth/API failure, do not retry immediately
   13  RATE_LIMIT_ABORT   - CLI hit usage/plan limit, do not retry

NOTE:
    Force mode does NOT skip the Judge phase. Code review always runs because
    GitHub's API prevents self-approval of PRs. Force mode enables auto-approval
    at phase 2 and auto-merge at phase 6.

    Without --force, the shepherd exits after the PR is approved (loom:pr).
    The Champion role handles merging approved PRs.

EXAMPLES:
    # Create PR, exit after approval (default behavior)
    loom-shepherd 42

    # Full automation with auto-merge
    loom-shepherd 42 --force
    loom-shepherd 42 -f
    loom-shepherd 42 --merge
    loom-shepherd 42 -m

    # Stop after curation (for review before building)
    loom-shepherd 42 --to curated

    # Skip builder, auto-detect existing PR, run judge + merge
    loom-shepherd 42 --skip-builder --merge

    # Skip builder, use specific PR number
    loom-shepherd 42 --pr 312 --merge

    # Resume from judge phase (PR already exists)
    loom-shepherd 42 --from judge --force
""",
    )

    parser.add_argument(
        "issue",
        type=int,
        help="Issue number to orchestrate",
    )

    parser.add_argument(
        "--force",
        "-f",
        action="store_true",
        dest="force",
        help="Auto-approve, resolve conflicts, auto-merge after approval",
    )
    parser.add_argument(
        "--merge",
        "-m",
        action="store_true",
        dest="force",
        help="Alias for --force (matches bash shepherd-loop.sh)",
    )

    parser.add_argument(
        "--from",
        dest="start_from",
        choices=["curator", "builder", "judge", "merge"],
        help="Start from specified phase (skip earlier phases)",
    )

    parser.add_argument(
        "--to",
        dest="stop_after",
        choices=["curated", "approved", "pr"],
        help="Stop after specified phase",
    )

    parser.add_argument(
        "--task-id",
        dest="task_id",
        help="Use specific task ID (generated if not provided)",
    )

    parser.add_argument(
        "--strict-quality",
        action="store_true",
        dest="strict_quality",
        help="Block builder if issue is missing acceptance criteria",
    )

    parser.add_argument(
        "--allow-dirty-main",
        action="store_true",
        dest="allow_dirty_main",
        help="Proceed even if main repo has uncommitted changes",
    )

    parser.add_argument(
        "--skip-builder",
        action="store_true",
        dest="skip_builder",
        help="Skip builder phase and auto-detect existing PR for the issue",
    )

    parser.add_argument(
        "--pr",
        type=int,
        dest="pr_number",
        help="Skip builder phase and use specified PR number directly",
    )

    parser.add_argument(
        "--resume",
        action="store_true",
        help="Resume from prior work (existing branch/checkpoint). "
        "Passed automatically by daemon when prior work is detected.",
    )

    # Deprecated flags
    parser.add_argument(
        "--wait",
        action="store_true",
        help=argparse.SUPPRESS,  # Deprecated
    )

    parser.add_argument(
        "--force-pr",
        action="store_true",
        help=argparse.SUPPRESS,  # Deprecated
    )

    parser.add_argument(
        "--force-merge",
        action="store_true",
        help=argparse.SUPPRESS,  # Deprecated
    )

    return parser.parse_args(argv)


def _is_loom_runtime(porcelain_line: str) -> bool:
    """Check if a git status porcelain line refers to a loom runtime file.

    Matches both ``.loom/`` directory files and ``.loom-*`` root-level files
    (e.g. ``.loom-checkpoint``).

    Args:
        porcelain_line: A line from ``git status --porcelain``, e.g. ``"?? .loom/daemon-state.json"``

    Returns:
        True if the file path starts with ``.loom/`` or ``.loom-``
    """
    # Format: "XY filename" or "XY filename -> renamed"
    path = parse_porcelain_path(porcelain_line)
    if " -> " in path:
        path = path.split(" -> ")[-1]  # Use destination for renames
    return path.startswith(".loom/") or path.startswith(".loom-")


def _worktree_has_diff_for_file(worktree_path: Path, rel_path: str) -> bool:
    """Check whether a worktree has actually modified a file relative to origin/main.

    Runs ``git diff origin/main -- <rel_path>`` inside the worktree and returns
    ``True`` only when the output is non-empty, meaning the worktree made real
    modifications to that file.  File *existence* is intentionally not used
    because every worktree (being a clone of the same repository) will contain
    most source files regardless of whether that worktree modified them.

    Args:
        worktree_path: Absolute path to the worktree directory.
        rel_path: Repository-relative file path to check.

    Returns:
        ``True`` if the file was modified in this worktree relative to
        ``origin/main``; ``False`` otherwise (including on any error).
    """
    try:
        result = run_git(
            ["diff", "origin/main", "--", rel_path],
            cwd=worktree_path,
            check=False,
        )
        return bool(result.returncode == 0 and result.stdout.strip())
    except Exception:
        return False


def _find_source_issues_for_dirty_files(
    repo_root: Path, dirty_filenames: list[str]
) -> dict[int, list[str]]:
    """Cross-reference dirty files against active issue worktrees.

    Scans ``.loom/worktrees/issue-*/`` to find which worktrees have *actually
    modified* the same files as the ones found dirty in the main repo.  This
    helps identify which issue's builder work may have leaked into main.

    The check uses ``git diff origin/main -- <rel_path>`` inside each worktree
    rather than file existence, because every worktree (being a clone of the
    same repository) contains most source files regardless of whether that
    worktree modified them.  Only worktrees with non-empty diff output are
    included in the result.

    Args:
        repo_root: The resolved repository root path.
        dirty_filenames: Relative file paths from the main repo (as returned
            by ``parse_porcelain_path``).

    Returns:
        A mapping of issue_number -> list of matching file paths.  Only
        issues with at least one file that the worktree modified relative to
        ``origin/main`` are included.
    """
    worktrees_dir = repo_root / ".loom" / "worktrees"
    if not worktrees_dir.is_dir():
        return {}

    matches: dict[int, list[str]] = {}
    try:
        for entry in sorted(worktrees_dir.iterdir()):
            if not entry.is_dir():
                continue
            issue_num = NamingConventions.issue_from_worktree(entry.name)
            if issue_num is None:
                continue
            # Check which dirty files were actually modified in this worktree
            # relative to origin/main (not just whether the file exists there).
            found: list[str] = []
            for rel_path in dirty_filenames:
                if _worktree_has_diff_for_file(entry, rel_path):
                    found.append(rel_path)
            if found:
                matches[issue_num] = found
    except OSError:
        pass
    return matches


def _check_main_repo_clean(
    repo_root: Path, allow_dirty: bool, allow_dirty_reason: str = "--allow-dirty-main specified"
) -> bool:
    """Check if main repository has uncommitted changes and warn.

    When running from a worktree, test results can differ between:
    - Running tests from main (includes uncommitted changes)
    - Running tests from a worktree (clean checkout at HEAD)

    This check warns users about this potential source of confusion.
    Files under ``.loom/`` are filtered out since they are runtime artifacts,
    not source code.  When dirty files are detected the function also
    cross-references them against active issue worktrees and prints a
    suggested recovery path when likely source issues are found.

    Args:
        repo_root: The resolved repository root path
        allow_dirty: If True, only warn but don't block
        allow_dirty_reason: Human-readable explanation of why dirty main is allowed

    Returns:
        True if clean or allowed to proceed, False if dirty and should block
    """
    uncommitted = get_uncommitted_files(cwd=repo_root)
    # Filter out .loom/ runtime files - these are never source code
    uncommitted = [f for f in uncommitted if not _is_loom_runtime(f)]
    if not uncommitted:
        return True

    # Warn about uncommitted changes
    log_warning(f"Main repository has {len(uncommitted)} uncommitted change(s):")
    for line in uncommitted[:10]:  # Show first 10 files
        # Parse porcelain format: "XY filename"
        status = line[:2].strip()
        filename = parse_porcelain_path(line)
        print(f"  {status} {filename}", file=sys.stderr)
    if len(uncommitted) > 10:
        print(f"  ... and {len(uncommitted) - 10} more", file=sys.stderr)
    print(file=sys.stderr)

    # Cross-reference dirty files against active issue worktrees to suggest
    # which issue the leaked changes likely belong to.
    dirty_filenames = [parse_porcelain_path(line) for line in uncommitted]
    source_issues = _find_source_issues_for_dirty_files(repo_root, dirty_filenames)
    if source_issues:
        issue_list = ", ".join(f"#{n}" for n in sorted(source_issues))
        log_warning(
            f"These changes may be orphaned builder work for issue(s): {issue_list}\n"
            "  To preserve them before proceeding, run one of:\n"
            "    git stash push -u -m 'orphaned-builder-work'\n"
            "    git checkout -b orphaned-builder-work && git add -A && git commit -m 'wip: preserve orphaned builder work'"
        )
        print(file=sys.stderr)

    if allow_dirty:
        log_warning(f"Proceeding anyway ({allow_dirty_reason})")
        print(file=sys.stderr)
        return True

    log_error(
        "Main repo has uncommitted changes that could cause test inconsistencies.\n"
        "  Tests in worktrees run against HEAD, not your uncommitted changes.\n"
        "  Options:\n"
        "    1. Commit or stash your changes before running shepherd\n"
        "    2. Use --allow-dirty-main to proceed anyway"
    )
    return False


def _auto_navigate_out_of_worktree(repo_root: Path) -> None:
    """Navigate to repo root if CWD is inside a worktree.

    This prevents issues where shepherd deletes a worktree that
    contains its own shell session's CWD, causing "No such file
    or directory" errors when the worktree is recreated.

    Args:
        repo_root: The resolved repository root path
    """
    cwd = Path.cwd().resolve()
    worktrees_dir = repo_root / ".loom" / "worktrees"

    try:
        # Check if CWD is inside .loom/worktrees/
        cwd.relative_to(worktrees_dir)
        # If we get here, CWD is inside worktrees dir
        log_warning(f"CWD is inside worktree ({cwd}), navigating to {repo_root}")
        os.chdir(repo_root)
    except ValueError:
        # Not inside worktrees - nothing to do
        pass


def _create_config(args: argparse.Namespace) -> ShepherdConfig:
    """Create ShepherdConfig from parsed arguments."""
    # Handle deprecated flags
    mode = ExecutionMode.DEFAULT

    if args.wait:
        log_warning("Flag --wait is deprecated (shepherd always exits after PR approval)")
        mode = ExecutionMode.NORMAL

    if args.force_pr:
        log_warning("Flag --force-pr is deprecated (now default behavior)")
        mode = ExecutionMode.DEFAULT

    if args.force_merge:
        log_warning("Flag --force-merge is deprecated (use --force or -f instead)")
        mode = ExecutionMode.FORCE_MERGE

    if args.force:
        mode = ExecutionMode.FORCE_MERGE

    # Parse --from phase
    start_from = None
    if args.start_from:
        phase_map = {
            "curator": Phase.CURATOR,
            "builder": Phase.BUILDER,
            "judge": Phase.JUDGE,
            "merge": Phase.MERGE,
        }
        start_from = phase_map.get(args.start_from)

    # Configure quality gates
    quality_gates = QualityGates()
    if args.strict_quality:
        quality_gates = QualityGates.strict()

    # Handle --skip-builder and --pr
    skip_builder = getattr(args, "skip_builder", False)
    pr_number_override = getattr(args, "pr_number", None)

    # --pr implies --skip-builder
    if pr_number_override is not None:
        skip_builder = True

    config = ShepherdConfig(
        issue=args.issue,
        mode=mode,
        start_from=start_from,
        stop_after=args.stop_after,
        quality_gates=quality_gates,
        skip_builder=skip_builder,
        pr_number_override=pr_number_override,
        resume=args.resume,
    )

    if args.task_id:
        config.task_id = args.task_id

    return config


def _remove_worktree_marker(ctx: ShepherdContext) -> None:
    """Remove worktree marker file on exit."""
    if ctx.worktree_path:
        marker_path = ctx.worktree_path / ctx.config.worktree_marker_file
        if marker_path.is_file():
            try:
                marker_path.unlink()
            except OSError:
                pass


def orchestrate(ctx: ShepherdContext) -> int:
    """Run the shepherd orchestration loop.

    Returns:
        Exit code from ShepherdExitCode enum:
        - SUCCESS (0): Full success (merged/approved)
        - BUILDER_FAILED (1): No PR created
        - PR_TESTS_FAILED (2): PR created but tests failed
        - SHUTDOWN (3): Shutdown signal received
        - NEEDS_INTERVENTION (4): Stuck/blocked, needs human intervention
        - SKIPPED (5): Issue already complete
        - NO_CHANGES_NEEDED (6): No changes determined, issue marked blocked
    """
    start_time = time.time()
    completed_phases: list[str] = []
    phase_durations: dict[str, int] = {}

    try:
        # Validate issue
        try:
            ctx.validate_issue()
        except IssueClosedError as e:
            log_info(f"Issue #{ctx.config.issue} is already {e.state} - no orchestration needed")
            return ShepherdExitCode.SKIPPED
        except IssueIsEpicError as e:
            log_info(str(e))
            return ShepherdExitCode.SKIPPED

        log_info(f"Issue: #{ctx.config.issue}")
        log_info(f"Mode: {ctx.config.mode.value}")
        if ctx.config.start_from:
            log_info(f"Start from: {ctx.config.start_from.value} phase")
        log_info(f"Task ID: {ctx.config.task_id}")
        log_info(f"Repository: {ctx.repo_root}")
        log_info(f"Title: {ctx.issue_title}")

        # Log prior failure count for observability
        fail_count, fail_threshold, last_err = _get_prior_failure_info(
            ctx.repo_root, ctx.config.issue
        )
        if fail_count > 0:
            remaining = max(0, fail_threshold - fail_count)
            log_info(
                f"Prior failures for issue #{ctx.config.issue}: "
                f"{fail_count}/{fail_threshold} (last: {last_err})"
            )
            log_info(f"Remaining retry budget: {remaining}")
        else:
            log_info(
                f"Prior failures for issue #{ctx.config.issue}: "
                f"0/{fail_threshold} (clean slate)"
            )

        print(file=sys.stderr)

        # Report started milestone
        ctx.report_milestone("started", issue=ctx.config.issue, mode=ctx.config.mode.value)

        # ─── Force mode: clear prior failures for this issue ─────────────
        if ctx.config.is_force_mode:
            from loom_tools.common.systematic_failure import clear_failures_for_issue

            cleared = clear_failures_for_issue(ctx.repo_root, ctx.config.issue)
            if cleared > 0:
                log_info(
                    f"Force mode: cleared {cleared} prior failure(s) for issue "
                    f"#{ctx.config.issue} — full retry window restored"
                )

        # ─── PHASE 1: Curator ─────────────────────────────────────────────
        curator = CuratorPhase()
        skip, reason = curator.should_skip(ctx)

        if skip:
            log_info(f"Skipping curator phase ({reason})")
            completed_phases.append(f"Curator ({reason})")
        else:
            _print_phase_header("PHASE 1: CURATOR")
            phase_start = time.time()
            result = curator.run(ctx)
            elapsed = int(time.time() - phase_start)

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status == PhaseStatus.FAILED:
                log_error(result.message)
                if result.data.get("auth_failure"):
                    return ShepherdExitCode.SYSTEMIC_FAILURE
                elif result.data.get("rate_limit_abort"):
                    return ShepherdExitCode.RATE_LIMIT_ABORT
                return ShepherdExitCode.BUILDER_FAILED

            if result.status == PhaseStatus.SKIPPED:
                completed_phases.append(f"Curator ({result.message})")
            else:
                phase_durations["Curator"] = elapsed
                completed_phases.append("Curator")
                log_success(f"Curator phase complete ({elapsed}s)")
                ctx.report_milestone(
                    "phase_completed", phase="curator", duration_seconds=elapsed, status="success"
                )

        if ctx.config.stop_after == "curated":
            _print_phase_header("STOPPING: Reached --to curated")
            return ShepherdExitCode.SUCCESS

        # ─── POST-CURATOR: Re-check issue state ─────────────────────────
        # The curator may have flagged the issue as blocked (e.g. duplicate).
        # Re-read labels and abort if loom:blocked was added during this pipeline,
        # even in merge mode — merge mode overrides *pre-existing* blocks, not
        # fresh signals from the current pipeline's curator.
        ctx.label_cache.invalidate_issue(ctx.config.issue)
        if ctx.has_issue_label("loom:blocked"):
            log_warning("Curator flagged issue as blocked — aborting pipeline")
            ctx.report_milestone(
                "blocked", reason="curator_blocked", details="Curator added loom:blocked during this pipeline"
            )
            return ShepherdExitCode.NO_CHANGES_NEEDED

        # ─── PHASE 2: Approval Gate ───────────────────────────────────────
        _print_phase_header("PHASE 2: APPROVAL GATE")
        phase_start = time.time()
        approval = ApprovalPhase()
        result = approval.run(ctx)
        elapsed = int(time.time() - phase_start)

        if result.is_shutdown:
            raise ShutdownSignal(result.message)

        phase_durations["Approval"] = elapsed
        completed_phases.append(f"Approval ({result.data.get('summary', result.message)})")
        if result.status == PhaseStatus.SUCCESS:
            log_success(f"{result.message} ({elapsed}s)")
            ctx.report_milestone(
                "phase_completed", phase="approval", duration_seconds=elapsed, status="success"
            )

        if ctx.config.stop_after == "approved":
            _print_phase_header("STOPPING: Reached --to approved")
            return ShepherdExitCode.SUCCESS

        # ─── PRE-FLIGHT: Baseline Health Check ───────────────────────────
        # This is a lightweight cache lookup (no subprocess, no timing needed).
        preflight = PreflightPhase()
        skip, reason = preflight.should_skip(ctx)

        if skip:
            log_info(f"Skipping preflight check ({reason})")
        else:
            result = preflight.run(ctx)

            if result.status == PhaseStatus.FAILED:
                _print_phase_header("PRE-FLIGHT: BASELINE HEALTH CHECK")
                log_warning(f"Preflight: {result.message}")
                ctx.report_milestone(
                    "blocked",
                    reason="baseline_failing",
                    details=result.message,
                )
                _mark_baseline_blocked(ctx, result)
                return ShepherdExitCode.NEEDS_INTERVENTION

            # Store preflight result so builder can skip baseline tests
            ctx.preflight_baseline_status = result.data.get("baseline_status")

            # Log result inline (no header for passing checks)
            log_info(f"Baseline health: {result.message}")

        # ─── PHASE 3: Builder (with test-fix Doctor loop) ────────────────
        builder = BuilderPhase()
        skip, reason = builder.should_skip(ctx)
        test_fix_attempts = 0
        builder_total_elapsed = 0
        doctor_total_elapsed_test_fix = 0
        prev_error_count: int | None = None  # Track errors for regression detection

        if skip:
            log_info(f"Skipping builder phase ({reason})")
            completed_phases.append(f"Builder ({reason})")
        else:
            _print_phase_header("PHASE 3: BUILDER")
            phase_start = time.time()
            result = builder.run(ctx)
            elapsed = int(time.time() - phase_start)
            builder_total_elapsed = elapsed

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            # Handle test timeouts — skip Doctor since the code is likely correct
            if (
                result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK)
                and result.data.get("test_timeout")
            ):
                log_error(
                    f"Test verification timed out. Skipping Doctor — "
                    f"timeouts indicate a resource/configuration issue, not a code defect. "
                    f"Consider increasing test timeout or using scoped tests."
                )
                phase_durations["Builder"] = builder_total_elapsed
                ctx.report_milestone(
                    "phase_completed",
                    phase="builder",
                    duration_seconds=builder_total_elapsed,
                    status="test_timeout",
                )
                _mark_builder_test_failure(ctx)
                return ShepherdExitCode.PR_TESTS_FAILED

            # Handle test failures with Doctor test-fix loop
            while (
                result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK)
                and result.data.get("test_failure")
            ):
                # Track error count for regression detection
                current_error_count = result.data.get("new_error_count")
                if prev_error_count is None:
                    # First iteration — record initial error count
                    prev_error_count = current_error_count
                elif (
                    current_error_count is not None
                    and prev_error_count is not None
                    and current_error_count > prev_error_count
                ):
                    # Doctor made things worse — abort immediately
                    log_error(
                        f"Doctor introduced regressions "
                        f"({prev_error_count} → {current_error_count} test failure(s)), "
                        f"aborting test-fix loop"
                    )
                    phase_durations["Builder"] = builder_total_elapsed
                    if doctor_total_elapsed_test_fix > 0:
                        phase_durations["Doctor (test-fix)"] = doctor_total_elapsed_test_fix
                    ctx.report_milestone(
                        "phase_completed",
                        phase="builder",
                        duration_seconds=builder_total_elapsed,
                        status="doctor_regression",
                    )
                    _mark_builder_test_failure(ctx)
                    return ShepherdExitCode.PR_TESTS_FAILED
                else:
                    # Update tracked count (same or improved)
                    prev_error_count = current_error_count

                test_fix_attempts += 1

                if test_fix_attempts > ctx.config.test_fix_max_retries:
                    # Max retries exceeded - fall back to failure label
                    log_error(
                        f"Builder test verification failed after "
                        f"{test_fix_attempts - 1} Doctor fix attempt(s) ({builder_total_elapsed}s)"
                    )
                    phase_durations["Builder"] = builder_total_elapsed
                    if doctor_total_elapsed_test_fix > 0:
                        phase_durations["Doctor (test-fix)"] = doctor_total_elapsed_test_fix
                    ctx.report_milestone(
                        "phase_completed",
                        phase="builder",
                        duration_seconds=builder_total_elapsed,
                        status="test_failure",
                    )
                    _mark_builder_test_failure(ctx)
                    return ShepherdExitCode.PR_TESTS_FAILED

                # Check if failing tests are in files the builder didn't modify.
                # If so, try rebasing onto latest main first — the fix may
                # already exist upstream, avoiding a wasted Doctor attempt.
                _preexisting = False
                if test_fix_attempts == 1:  # Only on first attempt
                    failing_test_files = set(result.data.get("failing_test_files", []))
                    changed_files = set(result.data.get("changed_files", []))

                    if failing_test_files and not (failing_test_files & changed_files):
                        log_warning(
                            f"Failing tests are in files not modified by builder: "
                            f"{sorted(failing_test_files)}"
                        )

                        if is_branch_behind(cwd=ctx.worktree_path):
                            log_info(
                                "Branch is behind origin/main, "
                                "attempting rebase before Doctor"
                            )
                            ctx.report_milestone(
                                "heartbeat",
                                action="rebasing onto main (failing tests in unmodified files)",
                            )

                            success, detail = attempt_rebase(cwd=ctx.worktree_path)
                            if success:
                                log_success(
                                    "Rebase succeeded, re-running test verification"
                                )

                                # Push rebased branch
                                if not builder.push_branch(ctx):
                                    log_warning(
                                        "Could not push rebased branch, "
                                        "continuing anyway"
                                    )

                                # Re-run tests after rebase
                                _print_phase_header(
                                    "PHASE 3b: TEST VERIFICATION (after rebase)"
                                )
                                test_start = time.time()
                                test_result = builder.run_test_verification_only(ctx)
                                test_elapsed = int(time.time() - test_start)
                                builder_total_elapsed += test_elapsed

                                if test_result is None:
                                    # Tests pass after rebase — skip Doctor
                                    log_success(
                                        f"Tests pass after rebase ({test_elapsed}s)"
                                    )
                                    result = PhaseResult(
                                        status=PhaseStatus.SUCCESS,
                                        message="builder complete (tests fixed by rebase)",
                                        phase_name="builder",
                                        data={"test_fixed_by_rebase": True},
                                    )
                                    break
                                else:
                                    # Tests still fail after rebase — since
                                    # failures are in unmodified files and
                                    # branch is now at parity with main,
                                    # these are pre-existing failures.
                                    log_warning(
                                        f"Tests still fail after rebase in "
                                        f"unmodified files ({test_elapsed}s), "
                                        f"treating as pre-existing"
                                    )
                                    ctx.report_milestone(
                                        "heartbeat",
                                        action="pre-existing test failures (post-rebase)",
                                    )
                                    completed_phases.append(
                                        "Pre-existing test failures (post-rebase)"
                                    )
                                    result = test_result
                                    elapsed = test_elapsed
                                    _preexisting = True
                            else:
                                log_warning(
                                    f"Rebase failed ({detail}), "
                                    f"proceeding to Doctor"
                                )
                        else:
                            # Branch is already up-to-date with main and
                            # failures are in files the builder didn't modify.
                            # These are pre-existing failures on main.
                            log_warning(
                                "Branch is up-to-date with main and failing "
                                "tests are in unmodified files, treating as "
                                "pre-existing"
                            )
                            ctx.report_milestone(
                                "heartbeat",
                                action="pre-existing test failures (branch up-to-date)",
                            )
                            completed_phases.append(
                                "Pre-existing test failures (branch up-to-date)"
                            )
                            _preexisting = True

                # Skip Doctor when pre-existing failures detected in
                # unmodified files (issue #2809)
                if _preexisting:
                    _print_phase_header(
                        "PHASE 3d: COMPLETION VALIDATION (pre-existing failures)"
                    )
                    completion_start = time.time()
                    completion_result = builder.validate_and_complete(ctx)
                    completion_elapsed = int(time.time() - completion_start)
                    builder_total_elapsed += completion_elapsed

                    if completion_result.is_shutdown:
                        raise ShutdownSignal(completion_result.message)

                    if completion_result.status == PhaseStatus.FAILED:
                        log_error(completion_result.message)
                        _mark_builder_no_pr(ctx)
                        return 1

                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="builder complete (pre-existing test failures)",
                        phase_name="builder",
                        data={
                            "preexisting_failures": True,
                            "pr_number": completion_result.data.get("pr_number"),
                        },
                    )
                    log_success(
                        f"Completion validation passed ({completion_elapsed}s)"
                    )
                    break

                # Route to Doctor for test fix
                log_warning(
                    f"Builder test verification failed ({elapsed}s), "
                    f"routing to Doctor (attempt {test_fix_attempts}/{ctx.config.test_fix_max_retries})"
                )
                ctx.report_milestone(
                    "heartbeat",
                    action=f"test failure, routing to Doctor (attempt {test_fix_attempts})",
                )

                _print_phase_header(f"PHASE 3b: DOCTOR TEST-FIX (attempt {test_fix_attempts})")
                doctor_start = time.time()
                doctor = DoctorPhase()
                doctor_result = doctor.run_test_fix(ctx, result.data)
                doctor_elapsed = int(time.time() - doctor_start)
                doctor_total_elapsed_test_fix += doctor_elapsed

                if doctor_result.is_shutdown:
                    raise ShutdownSignal(doctor_result.message)

                if doctor_result.status == PhaseStatus.SKIPPED:
                    # Doctor determined failures are pre-existing
                    log_warning(
                        f"Doctor determined test failures are pre-existing ({doctor_elapsed}s)"
                    )
                    completed_phases.append("Doctor (pre-existing failures)")

                    # Run validation and completion to ensure PR exists
                    # Even with pre-existing failures, we need to verify the
                    # builder actually committed and created a PR
                    _print_phase_header("PHASE 3d: COMPLETION VALIDATION (pre-existing failures)")
                    completion_start = time.time()
                    completion_result = builder.validate_and_complete(ctx)
                    completion_elapsed = int(time.time() - completion_start)
                    builder_total_elapsed += completion_elapsed

                    if completion_result.is_shutdown:
                        raise ShutdownSignal(completion_result.message)

                    if completion_result.status == PhaseStatus.FAILED:
                        log_error(completion_result.message)
                        _mark_builder_no_pr(ctx)
                        return 1

                    # Continue to PR creation - pre-existing failures are acceptable
                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="builder complete (pre-existing test failures)",
                        phase_name="builder",
                        data={
                            "preexisting_failures": True,
                            "pr_number": completion_result.data.get("pr_number"),
                        },
                    )
                    log_success(f"Completion validation passed ({completion_elapsed}s)")
                    break

                if doctor_result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                    # Doctor couldn't fix - re-run test verification to see current state
                    log_warning(
                        f"Doctor test-fix failed ({doctor_result.message}), "
                        f"re-running test verification"
                    )
                    completed_phases.append(f"Doctor (attempt {test_fix_attempts}, failed)")
                else:
                    # Doctor succeeded - re-run test verification to confirm fix
                    log_success(f"Doctor applied test fixes ({doctor_elapsed}s)")
                    completed_phases.append(f"Doctor (attempt {test_fix_attempts}, fixes applied)")
                    ctx.report_milestone(
                        "phase_completed",
                        phase="doctor-test-fix",
                        duration_seconds=doctor_elapsed,
                        status="success",
                    )

                    # Push doctor's fixes to remote immediately so CI starts
                    # and work is preserved even if the shepherd crashes.
                    if not builder.push_branch(ctx):
                        log_warning("Could not push doctor fixes to remote, continuing anyway")

                # Re-run test verification
                _print_phase_header(f"PHASE 3c: TEST VERIFICATION (after Doctor attempt {test_fix_attempts})")
                test_start = time.time()
                test_result = builder.run_test_verification_only(ctx)
                test_elapsed = int(time.time() - test_start)
                builder_total_elapsed += test_elapsed

                if test_result is None:
                    # Tests passed - now validate PR exists and complete if needed
                    log_success(f"Tests now pass after Doctor fixes ({test_elapsed}s)")

                    # Run validation and completion to ensure PR exists
                    # This handles the case where builder created code but
                    # didn't commit/push/create PR before doctor fixed tests
                    _print_phase_header("PHASE 3d: COMPLETION VALIDATION (after Doctor fixes)")
                    completion_start = time.time()
                    completion_result = builder.validate_and_complete(ctx)
                    completion_elapsed = int(time.time() - completion_start)
                    builder_total_elapsed += completion_elapsed

                    if completion_result.is_shutdown:
                        raise ShutdownSignal(completion_result.message)

                    if completion_result.status == PhaseStatus.FAILED:
                        log_error(completion_result.message)
                        _mark_builder_no_pr(ctx)
                        return 1

                    # Use the completion result which has the PR number
                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="builder complete (tests fixed by Doctor)",
                        phase_name="builder",
                        data={
                            "test_fixed_by_doctor": True,
                            "pr_number": completion_result.data.get("pr_number"),
                        },
                    )
                    log_success(f"Completion validation passed ({completion_elapsed}s)")
                    break
                else:
                    # Tests still failing - update result and loop
                    log_warning(f"Tests still failing after Doctor fixes ({test_elapsed}s)")
                    result = test_result
                    elapsed = test_elapsed
                    # Continue loop to try Doctor again or exhaust retries

            # Check for non-test failures
            if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                if not result.data.get("test_failure"):
                    log_error(result.message)
                    # Classify non-test failures with distinct exit codes so the
                    # daemon can make smarter retry/escalate decisions.
                    if result.data.get("worktree_escape"):
                        exit_code = ShepherdExitCode.WORKTREE_ESCAPE
                    elif result.data.get("auth_failure"):
                        exit_code = ShepherdExitCode.SYSTEMIC_FAILURE
                    elif result.data.get("rate_limit_abort"):
                        exit_code = ShepherdExitCode.RATE_LIMIT_ABORT
                    elif result.data.get("worktree_conflict"):
                        # Branch already checked out in another worktree —
                        # infrastructure failure, not an issue with the code.
                        # Use SYSTEMIC_FAILURE so _record_fallback_failure maps
                        # it to an infrastructure error class excluded from the
                        # systematic failure counter.  See issue #2918.
                        exit_code = ShepherdExitCode.SYSTEMIC_FAILURE
                    else:
                        exit_code = ShepherdExitCode.BUILDER_FAILED
                    # Record abandonment details so _post_fallback_failure_comment
                    # can generate a specific GitHub comment.  See issue #2839.
                    ctx.abandonment_info = {
                        "phase": result.phase_name or "builder",
                        "exit_code": int(exit_code),
                        "failure_data": dict(result.data),
                        "message": result.message,
                        "task_id": ctx.config.task_id,
                    }
                    return exit_code
                # Test failure after exhausting retries is handled above

            # Builder succeeded or was skipped
            if result.status == PhaseStatus.SKIPPED:
                # Check if this is "no changes needed" (different from regular skip)
                if result.data.get("no_changes_needed"):
                    # Mark as blocked for human review
                    _handle_no_changes_needed(ctx, result)
                    log_success(
                        f"Issue #{ctx.config.issue} marked blocked - builder could not determine changes needed"
                    )
                    return ShepherdExitCode.NO_CHANGES_NEEDED
                completed_phases.append(f"Builder ({result.message})")
            elif result.status == PhaseStatus.SUCCESS:
                phase_durations["Builder"] = builder_total_elapsed
                if doctor_total_elapsed_test_fix > 0:
                    phase_durations["Doctor (test-fix)"] = doctor_total_elapsed_test_fix
                completed_phases.append(f"Builder (PR #{ctx.pr_number})")
                log_success(
                    f"Builder phase complete - PR #{ctx.pr_number} created ({builder_total_elapsed}s)"
                )
                ctx.report_milestone(
                    "phase_completed",
                    phase="builder",
                    duration_seconds=builder_total_elapsed,
                    status="success",
                )

        # ─── PHASE 4/5: Judge/Doctor Loop ─────────────────────────────────

        # Precondition: PR must exist before entering Judge phase.
        # If builder failed without creating a PR (e.g., unexpected error,
        # timeout, or manual interruption), we cannot proceed to Judge.
        # This is a precondition failure, not a retryable error.
        if ctx.pr_number is None:
            log_error("Cannot enter Judge phase: no PR was created during Builder phase")
            _mark_builder_no_pr(ctx)
            return ShepherdExitCode.BUILDER_FAILED

        doctor_attempts = 0
        judge_retries = 0
        pr_approved = False

        # Check for --from merge skip
        judge = JudgePhase()
        skip, reason = judge.should_skip(ctx)

        if skip:
            log_info(f"Skipping judge phase ({reason})")
            completed_phases.append(f"Judge ({reason})")
            pr_approved = True

        judge_total_elapsed = 0
        doctor_total_elapsed = 0

        while not pr_approved and doctor_attempts < ctx.config.doctor_max_retries:
            _print_phase_header(f"PHASE 4: JUDGE (attempt {judge_retries + 1})")

            phase_start = time.time()
            result = judge.run(ctx)
            elapsed = int(time.time() - phase_start)
            judge_total_elapsed += elapsed

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                # Before retrying, check if the judge already completed its
                # work (applied loom:pr or loom:changes-requested) before the
                # failure was detected.  See issue #2335.
                ctx.label_cache.invalidate_pr(ctx.pr_number)
                if ctx.has_pr_label("loom:pr"):
                    log_info(
                        f"Judge already approved PR #{ctx.pr_number} "
                        f"(loom:pr label present), skipping retry"
                    )
                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="judge approved (detected post-failure)",
                        phase_name="judge",
                        data={"approved": True},
                    )
                    # Fall through to the approved handling below
                elif ctx.has_pr_label("loom:changes-requested"):
                    log_info(
                        f"Judge already requested changes on PR #{ctx.pr_number} "
                        f"(loom:changes-requested label present), skipping retry"
                    )
                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="judge completed (detected post-failure)",
                        phase_name="judge",
                        data={"changes_requested": True},
                    )
                    # Fall through to the changes_requested handling below

                # Judge returned FAILED/STUCK with no label outcome.
                # Retry the judge phase before giving up (defense-in-depth
                # for cases where the judge worker silently fails without
                # submitting a review, leaving no loom:pr or
                # loom:changes-requested label).
                elif judge_retries < ctx.config.judge_max_retries:
                    judge_retries += 1
                    log_warning(
                        f"Judge phase failed ({result.message}), "
                        f"retrying ({judge_retries}/{ctx.config.judge_max_retries})"
                    )
                    ctx.report_milestone(
                        "judge_retry",
                        attempt=judge_retries,
                        max_retries=ctx.config.judge_max_retries,
                        reason=result.message,
                    )
                    # Backoff for infrastructure failures (issue #2666).
                    # When the failure is due to low output, MCP failure,
                    # or ghost session, the underlying issue is persistent
                    # and rapid retrying just hits the same problem.
                    _judge_retry_backoff(ctx, result, judge_retries)
                    continue
                else:
                    log_error(
                        f"Judge phase failed after {judge_retries} "
                        f"retry attempt(s): {result.message}"
                    )
                    _mark_judge_exhausted(ctx, judge_retries)
                    return ShepherdExitCode.NEEDS_INTERVENTION

            # Judge succeeded — reset retry counter for this loop iteration
            judge_retries = 0

            # For results that lack expected data flags, check PR labels.
            # The judge may have completed its work (applied loom:pr or
            # loom:changes-requested) even though the result object doesn't
            # carry the corresponding flag.  See issue #2345.
            if not result.data.get("approved") and not result.data.get(
                "changes_requested"
            ):
                ctx.label_cache.invalidate_pr(ctx.pr_number)
                if ctx.has_pr_label("loom:pr"):
                    log_info(
                        f"Judge already approved PR #{ctx.pr_number} "
                        f"(loom:pr label present), skipping retry"
                    )
                    pr_approved = True
                    completed_phases.append(
                        "Judge (approved, detected from labels)"
                    )
                    ctx.report_milestone(
                        "phase_completed",
                        phase="judge",
                        duration_seconds=elapsed,
                        status="approved",
                    )
                    break
                elif ctx.has_pr_label("loom:changes-requested"):
                    log_info(
                        f"Judge already requested changes on PR #{ctx.pr_number} "
                        f"(loom:changes-requested label present), skipping retry"
                    )
                    # Override result so the changes_requested path below
                    # routes to the Doctor loop.
                    result = PhaseResult(
                        status=PhaseStatus.SUCCESS,
                        message="judge completed (detected from labels)",
                        phase_name="judge",
                        data={"changes_requested": True},
                    )
                    # Fall through to elif result.data.get("changes_requested")

            if result.data.get("approved"):
                pr_approved = True
                completed_phases.append("Judge (approved)")
                log_success(f"PR #{ctx.pr_number} approved by Judge ({elapsed}s)")
                ctx.report_milestone(
                    "phase_completed",
                    phase="judge",
                    duration_seconds=elapsed,
                    status="approved",
                )
            elif result.data.get("changes_requested"):
                log_warning(f"Judge requested changes on PR #{ctx.pr_number} ({elapsed}s)")
                completed_phases.append("Judge (changes requested)")
                ctx.report_milestone(
                    "phase_completed",
                    phase="judge",
                    duration_seconds=elapsed,
                    status="changes_requested",
                )

                doctor_attempts += 1

                if doctor_attempts >= ctx.config.doctor_max_retries:
                    log_error(f"Doctor max retries ({ctx.config.doctor_max_retries}) exceeded")
                    _mark_doctor_exhausted(ctx)
                    return ShepherdExitCode.NEEDS_INTERVENTION

                # ─── Doctor Phase ─────────────────────────────────────
                _print_phase_header(f"PHASE 5: DOCTOR (attempt {doctor_attempts})")

                phase_start = time.time()
                doctor = DoctorPhase()
                result = doctor.run(ctx)
                elapsed = int(time.time() - phase_start)
                doctor_total_elapsed += elapsed

                if result.is_shutdown:
                    raise ShutdownSignal(result.message)

                if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                    # Use failure mode to provide better diagnostics
                    failure_mode = result.data.get("failure_mode")
                    commits_made = result.data.get("commits_made", 0)

                    if failure_mode == "no_progress":
                        log_error(
                            f"Doctor made no progress ({result.message}). "
                            "Retry unlikely to help."
                        )
                        _mark_doctor_exhausted(ctx, failure_mode="no_progress")
                    elif failure_mode == "validation_failed" and commits_made > 0:
                        log_error(
                            f"Doctor made {commits_made} commit(s) but validation failed. "
                            "Label state may need manual recovery."
                        )
                        _mark_doctor_exhausted(ctx, failure_mode="validation_failed")
                    else:
                        log_error(result.message)
                        _mark_doctor_exhausted(ctx)

                    return ShepherdExitCode.NEEDS_INTERVENTION

                completed_phases.append("Doctor (fixes applied)")
                log_success(f"Doctor applied fixes ({elapsed}s)")
                ctx.report_milestone(
                    "phase_completed",
                    phase="doctor",
                    duration_seconds=elapsed,
                    status="success",
                )
            else:
                # Truly unexpected result with no label fallback (labels
                # were already checked in the pre-check above) — retry
                # or exhaust.  See issues #2335 and #2345.
                if judge_retries < ctx.config.judge_max_retries:
                    judge_retries += 1
                    log_warning(
                        f"Judge returned unexpected result ({result.message}), "
                        f"retrying ({judge_retries}/{ctx.config.judge_max_retries})"
                    )
                    ctx.report_milestone(
                        "judge_retry",
                        attempt=judge_retries,
                        max_retries=ctx.config.judge_max_retries,
                        reason=result.message,
                    )
                    # Backoff for infrastructure failures (issue #2666).
                    _judge_retry_backoff(ctx, result, judge_retries)
                    continue
                else:
                    log_error(
                        f"Judge phase returned unexpected result after {judge_retries} "
                        f"retry attempt(s): {result.message}"
                    )
                    _mark_judge_exhausted(ctx, judge_retries)
                    return ShepherdExitCode.NEEDS_INTERVENTION

        # Print skipped header if Doctor never ran (Judge approved first try)
        if doctor_attempts == 0 and not skip:
            _print_phase_header("PHASE 5: DOCTOR (skipped - no changes requested)")
            completed_phases.append("Doctor (skipped)")

        if judge_total_elapsed > 0:
            phase_durations["Judge"] = judge_total_elapsed
        if doctor_total_elapsed > 0:
            phase_durations["Doctor"] = doctor_total_elapsed

        if ctx.config.stop_after == "pr":
            _print_phase_header("STOPPING: Reached --to pr")
            return ShepherdExitCode.SUCCESS

        # ─── PHASE 5.5: Rebase (if needed) ────────────────────────────────
        _print_phase_header("PHASE 5.5: REBASE")
        phase_start = time.time()
        rebase = RebasePhase()
        result = rebase.run(ctx)
        elapsed = int(time.time() - phase_start)

        if result.is_shutdown:
            raise ShutdownSignal(result.message)

        if result.status == PhaseStatus.FAILED:
            log_error(result.message)
            return ShepherdExitCode.NEEDS_INTERVENTION

        if result.status == PhaseStatus.SKIPPED:
            completed_phases.append("Rebase (up to date)")
        else:
            phase_durations["Rebase"] = elapsed
            completed_phases.append("Rebase (rebased on main)")
            ctx.report_milestone(
                "phase_completed", phase="rebase", duration_seconds=elapsed, status="rebased"
            )

        # ─── PHASE 6: Merge Gate ──────────────────────────────────────────
        _print_phase_header("PHASE 6: MERGE GATE")
        phase_start = time.time()
        merge = MergePhase()
        result = merge.run(ctx)
        elapsed = int(time.time() - phase_start)

        if result.is_shutdown:
            raise ShutdownSignal(result.message)

        if result.status == PhaseStatus.FAILED:
            log_error(result.message)
            return ShepherdExitCode.NEEDS_INTERVENTION

        phase_durations["Merge"] = elapsed
        if result.data.get("merged"):
            completed_phases.append("Merge (auto-merged)")
            log_success(f"PR #{ctx.pr_number} merged successfully ({elapsed}s)")
            ctx.report_milestone(
                "phase_completed", phase="merge", duration_seconds=elapsed, status="merged"
            )
        else:
            completed_phases.append("Merge (awaiting merge)")
            log_info(f"PR #{ctx.pr_number} is approved and ready for Champion to merge ({elapsed}s)")
            log_info(f"To merge manually: ./.loom/scripts/merge-pr.sh {ctx.pr_number}")

        # ─── Complete ─────────────────────────────────────────────────────
        duration = int(time.time() - start_time)

        # Report completion
        if ctx.config.is_force_mode:
            ctx.report_milestone("completed", pr_merged=True)
        else:
            ctx.report_milestone("completed")

        _print_phase_header("SHEPHERD ORCHESTRATION COMPLETE")
        print(file=sys.stderr)
        log_info(f"Issue: #{ctx.config.issue} - {ctx.issue_title}")
        log_info(f"Mode: {ctx.config.mode.value}")
        log_info(f"Duration: {duration}s")
        print(file=sys.stderr)
        if phase_durations:
            log_info("Phase timing:")
            for phase_name, phase_secs in phase_durations.items():
                pct = int(phase_secs * 100 / duration) if duration > 0 else 0
                log_info(f"  - {phase_name}: {phase_secs}s ({pct}%)")
        else:
            log_info("Phases completed:")
            for phase in completed_phases:
                print(f"  - {phase}", file=sys.stderr)
        print(file=sys.stderr)
        log_success("Orchestration complete!")

        return ShepherdExitCode.SUCCESS

    except ShutdownSignal as e:
        log_warning(f"Shutdown signal detected: {e}")
        ctx.report_milestone(
            "blocked",
            reason="shutdown_signal",
            details=str(e),
        )
        log_info("Cleaning up and exiting gracefully...")
        return ShepherdExitCode.SHUTDOWN

    except IssueNotFoundError as e:
        log_error(str(e))
        return ShepherdExitCode.BUILDER_FAILED

    except IssueBlockedError as e:
        log_error(str(e))
        log_info("Use --force to override blocked status")
        return ShepherdExitCode.NEEDS_INTERVENTION

    except ShepherdError as e:
        log_error(str(e))
        return ShepherdExitCode.NEEDS_INTERVENTION


def _apply_failure_label(
    ctx: ShepherdContext, *, remove_label: str = "loom:building"
) -> bool:
    """Apply loom:blocked to an issue, removing the in-progress label.

    Transitions from ``remove_label`` to ``loom:blocked``.

    Args:
        ctx: Shepherd context
        remove_label: The label to remove (default "loom:building")

    Returns:
        True if the label was applied successfully.
    """
    success = transition_issue_labels(
        ctx.config.issue,
        add=["loom:blocked"],
        remove=[remove_label],
        repo_root=ctx.repo_root,
    )

    if not success:
        log_error(
            f"Failed to apply 'loom:blocked' to issue #{ctx.config.issue}. "
            f"Issue may be in inconsistent label state."
        )

    return success


def _mark_builder_test_failure(ctx: ShepherdContext) -> None:
    """Mark issue with loom:blocked after test verification failed.

    This replaces the old auto-recovery behavior. Instead of attempting to
    fix tests via Doctor, we now mark the failure explicitly and stop.
    The worktree and branch are preserved for manual intervention.
    """
    import subprocess

    # Atomic transition: loom:building -> loom:blocked
    _apply_failure_label(ctx)

    # Record blocked reason and update systematic failure tracking
    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    record_blocked_reason(
        ctx.repo_root,
        ctx.config.issue,
        error_class="builder_test_failure",
        phase="builder",
        details="Builder test verification failed",
    )
    detect_systematic_failure(ctx.repo_root)

    # Build diagnostic comment
    worktree_info = ""
    if ctx.worktree_path:
        worktree_info = f"\n\n**Worktree**: `{ctx.worktree_path}`"

    # Add comment with recovery instructions
    subprocess.run(
        [
            "gh",
            "issue",
            "comment",
            str(ctx.config.issue),
            "--body",
            f"**Builder test verification failed**\n\n"
            f"Tests failed after implementation. Worktree and branch are "
            f"preserved for manual intervention.{worktree_info}\n\n"
            f"### Recovery Options\n\n"
            f"**Option A: Fix tests manually**\n"
            f"```bash\n"
            f"cd {ctx.worktree_path or '.loom/worktrees/issue-' + str(ctx.config.issue)}\n"
            f"# Fix the failing tests\n"
            f"git add . && git commit -m 'Fix failing tests'\n"
            f"git push\n"
            f"gh pr create --title {shlex.quote(NamingConventions.pr_title(ctx.issue_title, ctx.config.issue))} --label loom:review-requested --body 'Closes #{ctx.config.issue}'\n"
            f"gh issue edit {ctx.config.issue} --remove-label loom:blocked\n"
            f"```\n\n"
            f"**Option B: Reset and retry**\n"
            f"```bash\n"
            f'cd "$(git rev-parse --show-toplevel)"  # Avoid broken CWD after removal\n'
            f"git worktree remove {ctx.worktree_path or '.loom/worktrees/issue-' + str(ctx.config.issue)} --force\n"
            f"gh issue edit {ctx.config.issue} --remove-label loom:blocked --add-label loom:issue\n"
            f"```",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _mark_doctor_exhausted(
    ctx: ShepherdContext, *, failure_mode: str | None = None
) -> None:
    """Mark issue with loom:blocked due to doctor retry exhaustion.

    Args:
        ctx: Shepherd context
        failure_mode: Optional failure classification (no_progress, insufficient_changes,
                      validation_failed) for better diagnostics
    """
    import subprocess

    # Atomic transition: loom:building -> loom:blocked
    _apply_failure_label(ctx)

    # Record blocked reason and update systematic failure tracking
    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    # Build error class and details based on failure mode
    error_class = f"doctor_{failure_mode}" if failure_mode else "doctor_exhausted"
    if failure_mode == "no_progress":
        details = "Doctor made no commits - no progress toward resolution"
    elif failure_mode == "validation_failed":
        details = "Doctor committed but label transition failed - label state inconsistent"
    elif failure_mode == "insufficient_changes":
        details = "Doctor committed but changes did not resolve the issue"
    else:
        details = f"max retries ({ctx.config.doctor_max_retries}) exceeded"

    record_blocked_reason(
        ctx.repo_root,
        ctx.config.issue,
        error_class=error_class,
        phase="doctor",
        details=details,
    )
    detect_systematic_failure(ctx.repo_root)

    # Build appropriate comment based on failure mode
    if failure_mode == "no_progress":
        comment_body = (
            "**Doctor phase failed**: Doctor made no commits toward resolution. "
            "This suggests the issue may be too complex for automated fixing or "
            "the feedback was unclear. Manual intervention required."
        )
    elif failure_mode == "validation_failed":
        comment_body = (
            "**Doctor phase failed**: Doctor committed changes but did not complete "
            "the label transition (missing `loom:review-requested`). PR label state "
            "may need manual recovery. Check PR labels and apply `loom:review-requested` "
            "if commits address the feedback."
        )
    else:
        comment_body = (
            f"**Doctor phase failed**: Could not resolve Judge feedback after "
            f"{ctx.config.doctor_max_retries} attempts. Manual intervention required."
        )

    subprocess.run(
        [
            "gh",
            "issue",
            "comment",
            str(ctx.config.issue),
            "--body",
            comment_body,
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _judge_retry_backoff(
    ctx: ShepherdContext,
    result: PhaseResult,
    judge_retries: int,
) -> None:
    """Apply backoff before judge retry when failure is infrastructure-related.

    Infrastructure failures (low output, MCP failure, ghost session) are
    persistent — rapid retrying just hits the same problem.  Adding backoff
    gives transient issues time to resolve (e.g., MCP server restart, plugin
    recovery).  See issue #2666.
    """
    is_infrastructure = (
        result.data.get("low_output")
        or result.data.get("mcp_failure")
        or result.data.get("ghost_session")
    )
    if is_infrastructure:
        backoff = min(30 * judge_retries, 120)  # 30s, 60s, 120s
        log_info(
            f"Infrastructure failure detected, waiting {backoff}s before retry"
        )
        ctx.report_milestone(
            "heartbeat",
            action=f"judge infrastructure backoff {backoff}s before retry {judge_retries}",
        )
        time.sleep(backoff)


def _mark_judge_exhausted(ctx: ShepherdContext, retries: int) -> None:
    """Mark issue with loom:blocked due to judge retry exhaustion."""
    import subprocess

    # Atomic transition: loom:building -> loom:blocked
    _apply_failure_label(ctx)

    # Record blocked reason and update systematic failure tracking
    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    record_blocked_reason(
        ctx.repo_root,
        ctx.config.issue,
        error_class="judge_exhausted",
        phase="judge",
        details=f"judge failed after {retries} retry attempt(s)",
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
            f"**Judge phase failed**: No review outcome after {retries} retry attempt(s). "
            "Neither approval nor changes-requested was produced. Manual intervention required.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _gather_no_pr_diagnostics(ctx: ShepherdContext) -> dict[str, str | int | list[str]]:
    """Gather diagnostic information when no PR was created.

    Collects worktree state information to help diagnose why the builder
    failed to create a PR.

    Returns:
        Dictionary with diagnostic information:
        - worktree_exists: bool
        - worktree_path: str (if exists)
        - uncommitted_files: list of file paths with status
        - uncommitted_count: int
        - commits_ahead_of_main: int
        - remote_branch_exists: bool
        - current_branch: str or None
        - suggested_recovery: str
    """
    from loom_tools.common.git import (
        get_commit_count,
        get_current_branch,
        get_uncommitted_files,
        run_git,
    )

    diagnostics: dict[str, str | int | list[str] | bool] = {}
    worktree_path = ctx.worktree_path
    issue = ctx.config.issue
    branch_name = f"feature/issue-{issue}"

    # Check if worktree exists
    diagnostics["worktree_exists"] = worktree_path is not None and worktree_path.is_dir()
    if worktree_path:
        diagnostics["worktree_path"] = str(worktree_path)

    # If worktree exists, gather git state from it
    cwd = worktree_path if diagnostics["worktree_exists"] else ctx.repo_root

    # Get uncommitted files
    uncommitted = get_uncommitted_files(cwd=cwd)
    diagnostics["uncommitted_files"] = uncommitted
    diagnostics["uncommitted_count"] = len(uncommitted)

    # Get commits ahead of main
    diagnostics["commits_ahead_of_main"] = get_commit_count(base="origin/main", cwd=cwd)

    # Get current branch
    diagnostics["current_branch"] = get_current_branch(cwd=cwd)

    # Check if remote branch exists
    try:
        result = run_git(
            ["ls-remote", "--heads", "origin", branch_name],
            cwd=ctx.repo_root,
            check=False,
        )
        diagnostics["remote_branch_exists"] = (
            result.returncode == 0 and bool(result.stdout.strip())
        )
    except Exception:
        diagnostics["remote_branch_exists"] = False

    # Determine suggested recovery
    uncommitted_count = diagnostics["uncommitted_count"]
    commits_ahead = diagnostics["commits_ahead_of_main"]
    remote_exists = diagnostics["remote_branch_exists"]

    if uncommitted_count > 0 and commits_ahead == 0:
        diagnostics["suggested_recovery"] = "commit changes, push, create PR manually"
    elif uncommitted_count > 0 and commits_ahead > 0:
        diagnostics["suggested_recovery"] = "commit remaining changes, push, create PR manually"
    elif commits_ahead > 0 and not remote_exists:
        diagnostics["suggested_recovery"] = "push branch, create PR manually"
    elif commits_ahead > 0 and remote_exists:
        diagnostics["suggested_recovery"] = "create PR manually (branch already pushed)"
    elif not diagnostics["worktree_exists"]:
        diagnostics["suggested_recovery"] = "re-run shepherd or create worktree manually"
    else:
        diagnostics["suggested_recovery"] = "investigate worktree state, no commits found"

    return diagnostics


def _format_diagnostics_for_log(diagnostics: dict[str, str | int | list[str] | bool]) -> str:
    """Format diagnostic information for log output."""
    lines = ["Diagnostics:"]
    lines.append(f"  Worktree exists: {'yes' if diagnostics.get('worktree_exists') else 'no'}"
                 + (f" ({diagnostics.get('worktree_path')})" if diagnostics.get('worktree_exists') else ""))

    uncommitted = diagnostics.get("uncommitted_files", [])
    if uncommitted:
        # Group files by directory prefix for readability
        lines.append(f"  Uncommitted changes: {len(uncommitted)} file(s)")
        # Show first few files
        for f in uncommitted[:5]:
            lines.append(f"    {f}")
        if len(uncommitted) > 5:
            lines.append(f"    ... and {len(uncommitted) - 5} more")
    else:
        lines.append("  Uncommitted changes: none")

    lines.append(f"  Commits ahead of main: {diagnostics.get('commits_ahead_of_main', 0)}")
    lines.append(f"  Remote branch exists: {'yes' if diagnostics.get('remote_branch_exists') else 'no'}")
    lines.append(f"  Current branch: {diagnostics.get('current_branch') or 'unknown'}")
    lines.append(f"  Suggested recovery: {diagnostics.get('suggested_recovery', 'unknown')}")
    return "\n".join(lines)


def _format_diagnostics_for_comment(
    diagnostics: dict[str, str | int | list[str] | bool],
    issue: int,
    issue_title: str = "",
) -> str:
    """Format diagnostic information for GitHub issue comment."""
    lines = ["**Builder phase failed**: No PR was created.",
             "",
             "Cannot proceed to Judge phase without a PR to review.",
             "",
             "### Diagnostics",
             ""]

    worktree_exists = diagnostics.get("worktree_exists", False)
    worktree_path = diagnostics.get("worktree_path", f".loom/worktrees/issue-{issue}")
    lines.append(f"| Property | Value |")
    lines.append(f"|----------|-------|")
    lines.append(f"| Worktree exists | {'yes' if worktree_exists else 'no'} |")
    if worktree_exists:
        lines.append(f"| Worktree path | `{worktree_path}` |")
    lines.append(f"| Uncommitted changes | {diagnostics.get('uncommitted_count', 0)} file(s) |")
    lines.append(f"| Commits ahead of main | {diagnostics.get('commits_ahead_of_main', 0)} |")
    lines.append(f"| Remote branch exists | {'yes' if diagnostics.get('remote_branch_exists') else 'no'} |")
    lines.append(f"| Current branch | `{diagnostics.get('current_branch') or 'unknown'}` |")

    uncommitted = diagnostics.get("uncommitted_files", [])
    if uncommitted:
        lines.append("")
        lines.append("**Uncommitted files:**")
        for f in uncommitted[:10]:
            lines.append(f"- `{f}`")
        if len(uncommitted) > 10:
            lines.append(f"- ... and {len(uncommitted) - 10} more")

    lines.append("")
    lines.append("### Suggested Recovery")
    lines.append("")
    suggested = diagnostics.get("suggested_recovery", "investigate worktree state")
    lines.append(f"**{suggested}**")
    lines.append("")

    # Provide concrete recovery commands based on the state
    if worktree_exists:
        commits_ahead = diagnostics.get("commits_ahead_of_main", 0)
        uncommitted_count = diagnostics.get("uncommitted_count", 0)
        remote_exists = diagnostics.get("remote_branch_exists", False)

        lines.append("```bash")
        lines.append(f"cd {worktree_path}")
        if uncommitted_count > 0:
            lines.append("git add .")
            lines.append('git commit -m "Complete implementation"')
        if not remote_exists or commits_ahead > 0 or uncommitted_count > 0:
            lines.append(f"git push -u origin feature/issue-{issue}")
        title = NamingConventions.pr_title(issue_title, issue)
        lines.append(f'gh pr create --title {shlex.quote(title)} --label "loom:review-requested" --body "Closes #{issue}"')
        lines.append(f"gh issue edit {issue} --remove-label loom:blocked")
        lines.append("```")

    return "\n".join(lines)


def _mark_builder_no_pr(ctx: ShepherdContext) -> None:
    """Mark issue with loom:blocked because no PR was created.

    This handles the case where Builder completes without creating a PR,
    which is a precondition failure for the Judge phase. This covers unexpected
    errors, timeouts, or manual interruptions that leave no PR behind.

    Gathers and logs diagnostic information to help operators understand
    what state the worktree is in and how to recover.
    """
    import subprocess

    # Gather diagnostic information before any state changes
    diagnostics = _gather_no_pr_diagnostics(ctx)

    # Log diagnostics to stderr for visibility
    log_info(_format_diagnostics_for_log(diagnostics))

    # Atomic transition: loom:building -> loom:blocked
    _apply_failure_label(ctx)

    # Record blocked reason and update systematic failure tracking
    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    record_blocked_reason(
        ctx.repo_root,
        ctx.config.issue,
        error_class="builder_no_pr",
        phase="builder",
        details="Builder phase completed but no PR was created",
    )
    detect_systematic_failure(ctx.repo_root)

    # Add diagnostic comment to the issue
    comment_body = _format_diagnostics_for_comment(diagnostics, ctx.config.issue, ctx.issue_title)
    subprocess.run(
        [
            "gh",
            "issue",
            "comment",
            str(ctx.config.issue),
            "--body",
            comment_body,
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _handle_no_changes_needed(ctx: ShepherdContext, result: "PhaseResult") -> None:
    """Handle the case where Builder determined no changes are needed.

    Marks the issue as blocked with an explanatory comment so a human can
    review whether the issue is truly resolved or needs better specification.
    The builder should never close issues — only PR merges close issues.
    """
    import subprocess

    reason = result.data.get("reason", "already_resolved")
    reason_text = {
        "already_resolved": "The reported problem appears to be already resolved on main.",
        "no_changes_required": "Analysis indicates no code changes are required.",
    }.get(reason, "No changes were determined to be necessary.")

    # Build comment body
    comment_body = f"""**Shepherd: Builder could not determine changes needed**

{reason_text}

The Builder phase analyzed this issue but could not identify code changes to make.
This issue has been marked as `loom:blocked` for human review.

Possible next steps:
- Add more implementation guidance to the issue description
- Verify the issue is still relevant
- Close the issue manually if it is truly resolved
"""

    # Add explanatory comment
    subprocess.run(
        [
            "gh",
            "issue",
            "comment",
            str(ctx.config.issue),
            "--body",
            comment_body,
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )

    # Transition labels: remove loom:building, add loom:blocked
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


def _mark_baseline_blocked(ctx: ShepherdContext, result: "PhaseResult") -> None:
    """Mark issue as blocked due to failing baseline tests on main.

    Instead of proceeding to the builder phase where the shepherd would
    independently discover the same baseline failures, we block early
    and add a comment explaining the situation.
    """
    import subprocess

    issue_tracking = result.data.get("issue_tracking", "")
    failing_tests = result.data.get("failing_tests", [])

    # Atomic transition: loom:building -> loom:blocked
    # (issue may or may not have loom:building at this point)
    success = transition_issue_labels(
        ctx.config.issue,
        add=["loom:blocked"],
        remove=["loom:building"],
        repo_root=ctx.repo_root,
    )
    if not success:
        log_warning(
            f"Failed to transition issue #{ctx.config.issue} to loom:blocked"
        )

    # Build comment
    test_list = ""
    if failing_tests:
        test_items = "\n".join(f"- `{t}`" for t in failing_tests)
        test_list = f"\n\n**Failing tests:**\n{test_items}"

    tracking_ref = ""
    if issue_tracking:
        tracking_ref = f"\n\n**Tracking issue:** {issue_tracking}"

    subprocess.run(
        [
            "gh",
            "issue",
            "comment",
            str(ctx.config.issue),
            "--body",
            f"**Blocked: main branch tests failing**\n\n"
            f"Pre-flight health check detected that baseline tests on main "
            f"are currently failing. Blocking builder to avoid redundant "
            f"failure discovery.{test_list}{tracking_ref}\n\n"
            f"This issue will be unblocked automatically when the Auditor "
            f"confirms main is healthy again, or you can override with:\n"
            f"```bash\n"
            f"gh issue edit {ctx.config.issue} --remove-label loom:blocked "
            f"--add-label loom:issue\n"
            f"```",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )

    # Record blocked reason
    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    record_blocked_reason(
        ctx.repo_root,
        ctx.config.issue,
        error_class="baseline_failing",
        phase="preflight",
        details=result.message,
    )
    detect_systematic_failure(ctx.repo_root)


def _cleanup_pr_labels_on_failure(ctx: ShepherdContext) -> None:
    """Best-effort cleanup of stale workflow labels on the associated PR.

    When shepherd fails, PR labels from in-progress workflow states can be
    left behind (e.g., loom:treating, loom:reviewing, loom:changes-requested).
    These are workflow state labels that become contradictory once the shepherd
    is no longer driving the lifecycle.

    Special case: loom:review-requested is preserved when the judge never
    produced an outcome (no loom:pr or loom:changes-requested on the PR).
    This keeps the PR visible for retry instead of leaving it with zero labels.

    Factual status labels (loom:merge-conflict, loom:ci-failure) are kept
    since they reflect actual PR state regardless of shepherd status.
    """
    # Workflow state labels to remove from PR on shepherd failure.
    # These indicate an active workflow step that is no longer running.
    PR_WORKFLOW_LABELS = {
        "loom:review-requested",
        "loom:changes-requested",
        "loom:treating",
        "loom:reviewing",
    }

    # Find the associated PR
    pr_number = ctx.pr_number
    if pr_number is None:
        try:
            pr_number = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        except Exception:
            return

    if pr_number is None:
        return  # No PR exists, nothing to clean up

    try:
        pr_labels = ctx.label_cache.get_pr_labels(pr_number, refresh=True)
    except Exception:
        return

    # If the judge never produced an outcome (no loom:pr or loom:changes-requested),
    # preserve loom:review-requested so the PR remains visible for retry.
    judge_produced_outcome = bool(
        {"loom:pr", "loom:changes-requested"} & pr_labels
    )
    labels_to_remove = PR_WORKFLOW_LABELS.copy()
    if not judge_produced_outcome:
        labels_to_remove.discard("loom:review-requested")

    stale_labels = labels_to_remove & pr_labels
    if not stale_labels:
        return

    try:
        transition_labels(
            "pr",
            pr_number,
            remove=sorted(stale_labels),
            repo_root=ctx.repo_root,
        )
        log_info(
            f"Label cleanup: removed stale workflow labels "
            f"{sorted(stale_labels)} from PR #{pr_number}"
        )
    except Exception:
        pass


def _cleanup_labels_on_failure(ctx: ShepherdContext, exit_code: int) -> None:
    """Best-effort cleanup of stale workflow labels when shepherd fails.

    This is defense-in-depth: known failure modes already handle their own
    label cleanup via _mark_* functions. This handler catches cases where
    an unhandled exception or unexpected failure path leaves the issue in
    an inconsistent label state (e.g., loom:building stuck on a dead issue).

    Rules:
    - On success/skip: no cleanup needed
    - If a _mark_* handler already set loom:blocked: remove any
      contradictory state labels (loom:building)
    - If loom:building is present with no blocked label: revert to loom:issue
    - Clean up stale PR workflow labels (loom:treating, loom:reviewing, etc.)
    """
    # No cleanup needed on success or skip
    if exit_code in (ShepherdExitCode.SUCCESS, ShepherdExitCode.SKIPPED):
        return

    issue = ctx.config.issue

    # Always attempt PR label cleanup on failure
    try:
        _cleanup_pr_labels_on_failure(ctx)
    except Exception:
        pass

    try:
        # Fetch current labels fresh from API (cache may be stale after crash)
        current_labels = ctx.label_cache.get_issue_labels(issue, refresh=True)
    except Exception:
        # Can't reach GitHub API - nothing we can do
        return

    # Check if a _mark_* handler already set loom:blocked
    has_blocked_label = "loom:blocked" in current_labels

    if has_blocked_label:
        # _mark_* already handled the transition - just clean up any
        # contradictory state labels that shouldn't coexist with loom:blocked
        if "loom:building" in current_labels:
            try:
                transition_issue_labels(
                    issue,
                    remove=["loom:building"],
                    repo_root=ctx.repo_root,
                )
                log_info(
                    f"Label cleanup: removed contradictory label "
                    f"loom:building from issue #{issue}"
                )
            except Exception:
                pass
        return

    # Check if the PR for this issue is already merged.  If so, the work
    # is complete and we must NOT revert labels — doing so would re-queue
    # an already-finished issue.  (See #2515.)
    try:
        from loom_tools.shepherd.phases.rebase import _is_pr_merged

        pr_to_check = ctx.pr_number
        if pr_to_check is None:
            # Try to find a merged PR by issue number
            pr_to_check = get_pr_for_issue(issue, state="merged", repo_root=ctx.repo_root)
        if pr_to_check is not None and _is_pr_merged(pr_to_check, ctx.repo_root):
            log_info(
                f"Label cleanup: PR #{pr_to_check} is already merged "
                f"— not reverting issue #{issue} labels"
            )
            return
    except Exception:
        pass

    # No failure label was set - revert loom:building to loom:issue
    # so the issue returns to the ready pool for retry
    if "loom:building" in current_labels:
        try:
            transition_issue_labels(
                issue,
                add=["loom:issue"],
                remove=["loom:building"],
                repo_root=ctx.repo_root,
            )
            log_info(
                f"Label cleanup: reverted issue #{issue} "
                f"from loom:building to loom:issue"
            )
        except Exception:
            pass

        # Post a diagnostic comment so operators know why the issue was recycled.
        # Without this comment, failures are invisible — the issue silently
        # returns to the ready pool with no record of what went wrong.
        _post_fallback_failure_comment(ctx, exit_code)

        # Track repeated fallback failures via the systematic failure detector
        # so escalation happens after N consecutive recycling events.
        _record_fallback_failure(ctx, exit_code)


def _post_fallback_failure_comment(ctx: ShepherdContext, exit_code: int) -> None:
    """Post a diagnostic comment when the fallback cleanup path fires.

    This makes silent recycling visible: operators can see *why* the issue
    was returned to the ready pool and distinguish infrastructure failures
    (auth/API) from implementation failures.

    When ctx.abandonment_info is set (by the orchestrator for specific failure
    modes), generates a detailed comment with the failure mode, task ID, and
    log file path.  Otherwise falls back to a generic message.  See issue #2839.

    Best-effort — never raises.
    """
    import datetime
    import subprocess

    from loom_tools.common.paths import LoomPaths
    from loom_tools.shepherd.exit_codes import describe_exit_code

    issue = ctx.config.issue
    task_id = ctx.config.task_id or "unknown"
    today = datetime.date.today().isoformat()

    # Use specific abandonment info recorded by the orchestrator for known
    # non-retryable failure modes (thinking stall, planning stall, etc.)
    if ctx.abandonment_info is not None:
        info = ctx.abandonment_info
        phase = info.get("phase", "builder")
        failure_data = info.get("failure_data", {})
        message = info.get("message", "")

        # Derive human-readable failure mode and advice from failure_data flags
        if failure_data.get("thinking_stall"):
            failure_mode = (
                "thinking stall — extended thinking output with zero tool calls "
                "(retry budget exhausted)"
            )
            safe_to_retry = True
            advice = (
                "The issue has been returned to `loom:issue` and is safe to retry. "
                "If this recurs, the issue may need more specific implementation guidance."
            )
        elif failure_data.get("planning_stall"):
            timeout = failure_data.get("planning_timeout", "unknown")
            failure_mode = (
                f"planning stall — agent did not progress past planning "
                f"within {timeout}s"
            )
            safe_to_retry = True
            advice = (
                "The issue has been returned to `loom:issue` and is safe to retry. "
                "Consider adding more specific implementation guidance to help "
                "the agent make progress."
            )
        elif failure_data.get("degraded_session"):
            failure_mode = (
                "degraded session — rate limits detected during execution "
                "(Crystallizing loop or Stop-and-wait modal)"
            )
            safe_to_retry = True
            advice = (
                "The issue has been returned to `loom:issue`. "
                "Retry after the rate limit resets (usually a few minutes)."
            )
        elif failure_data.get("worktree_conflict"):
            error_detail = failure_data.get("error_detail", "")
            failure_mode = (
                "worktree branch conflict — the feature branch is already "
                "checked out in another worktree (infrastructure failure, "
                "not a defect in the issue)"
            )
            safe_to_retry = True
            detail_line = f"\n\nGit error: `{error_detail}`" if error_detail else ""
            advice = (
                "The issue has been returned to `loom:issue` and is safe to retry. "
                "To resolve: run `loom-clean --force` or `git worktree prune` "
                "to remove the stale worktree, then retry the issue."
                f"{detail_line}"
            )
        elif failure_data.get("auth_failure"):
            failure_mode = (
                "auth pre-flight failure — authentication check timed out "
                "(parent session may hold config lock)"
            )
            safe_to_retry = False
            advice = (
                "This is an **infrastructure failure**, not an issue with the code. "
                "Check authentication tokens and API availability before retrying."
            )
        elif failure_data.get("rate_limit_abort"):
            failure_mode = (
                "rate limit abort — CLI hit usage/plan limit "
                "(interactive prompt in headless mode)"
            )
            safe_to_retry = False
            advice = (
                "Wait for the usage or plan limit to reset, or re-authenticate "
                "with a different plan before retrying."
            )
        elif failure_data.get("api_rate_limited"):
            failure_mode = (
                "API rate limit pre-check — usage exceeded configured threshold "
                "before builder was started"
            )
            safe_to_retry = True
            advice = (
                "The issue has been returned to `loom:issue`. "
                "Retry after the rate limit resets (usually a few minutes)."
            )
        else:
            # Generic builder failure (MCP failure, low-output, etc.)
            failure_mode = message or "unexpected failure"
            safe_to_retry = True
            advice = (
                "The issue has been returned to `loom:issue` and is safe to retry. "
                "If this issue fails repeatedly, it may need manual investigation "
                "or more detailed implementation guidance."
            )

        log_path = failure_data.get("log_file")
        if log_path is None:
            try:
                log_path = str(LoomPaths(ctx.repo_root).builder_log_file(issue))
            except Exception:
                log_path = None
        log_line = f"\nLog: `{log_path}`" if log_path else ""

        comment_body = (
            f"**Shepherd abandoned issue** (task `{task_id}`, {today})\n\n"
            f"Phase **{phase}** failed: {failure_mode}."
            f"{log_line}\n\n"
            f"{advice}"
        )
    else:
        # No specific abandonment info — use exit-code-based messages
        if exit_code == ShepherdExitCode.SYSTEMIC_FAILURE:
            failure_type = "infrastructure failure (auth/API)"
            advice = (
                "This is an **infrastructure failure**, not an issue with the code. "
                "Check authentication tokens and API availability before retrying."
            )
        elif exit_code == ShepherdExitCode.RATE_LIMIT_ABORT:
            failure_type = "rate limit abort (usage/plan limit)"
            advice = (
                "The CLI hit a **usage or plan limit** and showed an interactive prompt "
                "that cannot be answered in headless mode. Wait for the limit to reset "
                "or re-authenticate with a different plan before retrying."
            )
        else:
            failure_type = "unexpected failure"
            advice = (
                "If this issue fails repeatedly, it may need manual investigation "
                "or more detailed implementation guidance."
            )

        comment_body = (
            f"**Shepherd abandoned issue** (task `{task_id}`, {today})\n\n"
            f"The builder phase did not produce a PR. "
            f"The issue has been returned to the ready pool.\n\n"
            f"**Exit code**: {exit_code} ({describe_exit_code(exit_code)})\n"
            f"**Failure type**: {failure_type}\n\n"
            f"{advice}"
        )

    try:
        subprocess.run(
            [
                "gh", "issue", "comment", str(issue),
                "--body", comment_body,
            ],
            cwd=ctx.repo_root,
            capture_output=True,
            check=False,
        )
    except Exception:
        pass


def _record_fallback_failure(ctx: ShepherdContext, exit_code: int) -> None:
    """Record failure metadata when the fallback cleanup path fires.

    Feeds into the systematic failure detector so repeated fallback
    recycling triggers escalation.  When a systematic failure pattern
    is detected (≥N consecutive failures with the same error class),
    escalates the issue from ``loom:issue`` to ``loom:blocked`` to
    prevent further wasteful automated attempts.

    Best-effort — never raises.
    """
    import subprocess

    from loom_tools.common.systematic_failure import (
        detect_systematic_failure,
        record_blocked_reason,
    )

    # Classify the failure based on exit code and abandonment_info flags.
    if exit_code == ShepherdExitCode.SYSTEMIC_FAILURE:
        # Check if this SYSTEMIC_FAILURE was actually a worktree branch conflict
        # (branch already checked out in another worktree).  abandonment_info is
        # set by the orchestrator before returning the exit code.  See #2918.
        # Explicitly check isinstance(dict) to avoid false-positive from MagicMock
        # in tests where abandonment_info is not set.
        _ainfo = ctx.abandonment_info
        _abandonment_data = _ainfo.get("failure_data", {}) if isinstance(_ainfo, dict) else {}
        if _abandonment_data.get("worktree_conflict"):
            error_class = "worktree_conflict"
            error_detail = _abandonment_data.get("error_detail", "")
            details = (
                f"Worktree creation failed: branch already checked out in another "
                f"worktree (infrastructure failure, not an issue defect)"
            )
            if error_detail:
                details = f"{details} — {error_detail}"
        else:
            error_class = "auth_infrastructure_failure"
            details = "Builder failed due to auth/API infrastructure issue (fallback cleanup)"
    elif exit_code == ShepherdExitCode.WORKTREE_ESCAPE:
        error_class = "builder_worktree_escape"
        details = "Builder escaped worktree and modified main instead (fallback cleanup)"
    elif exit_code == ShepherdExitCode.RATE_LIMIT_ABORT:
        error_class = "rate_limit_abort"
        details = "CLI hit usage/plan limit — interactive prompt in headless mode (fallback cleanup)"
    elif (
        ctx.abandonment_info is not None
        and ctx.abandonment_info.get("failure_data", {}).get("thinking_stall")
    ):
        # Thinking stall exhaustion: builder produced thinking output but zero
        # tool calls across all retries.  Classified separately from generic
        # unknown failures so operators can distinguish it in diagnostics and
        # recovery guidance can be tailored (e.g. "increase
        # LOOM_BUILDER_THINKING_STALL_TIMEOUT").  See issue #2921.
        error_class = "builder_thinking_stall"
        details = (
            "Builder thinking stall retry budget exhausted — extended thinking "
            "output with zero tool calls detected (fallback cleanup)"
        )
    else:
        error_class = "builder_unknown_failure"
        # Include post-mortem diagnostics if available (issue #2766).
        postmortem_summary = ""
        if ctx.last_postmortem is not None:
            postmortem_summary = f" | post-mortem: {ctx.last_postmortem.get('summary', 'n/a')}"
        details = (
            f"Builder failed without specific handler "
            f"(exit code {exit_code}, fallback cleanup){postmortem_summary}"
        )

        # Check builder log for MCP failure markers before accepting the
        # generic unknown_failure class.  MCP failures are infrastructure
        # issues (server init, resource contention) that should not count
        # against the issue itself.  See issue #2768.
        try:
            import re

            from loom_tools.common.paths import LoomPaths
            from loom_tools.shepherd.phases.base import MCP_FAILURE_PATTERNS

            log_path = LoomPaths(ctx.repo_root).builder_log_file(ctx.config.issue)
            if log_path.is_file():
                content = log_path.read_text()
                for pattern in MCP_FAILURE_PATTERNS:
                    if re.search(pattern, content, re.IGNORECASE):
                        error_class = "mcp_infrastructure_failure"
                        details = f"Builder failed due to MCP server failure (exit code {exit_code}, fallback cleanup)"
                        break
        except Exception:
            pass  # Best-effort — fall through to unknown_failure

    try:
        # Skip systematic failure recording if a PR already exists for this
        # issue — the builder succeeded at its core task; only the post-
        # completion teardown failed (e.g. thinking stall).  Counting this
        # against the issue would cause false-positive loom:blocked
        # escalations.  See issue #2854.
        existing_pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
        if existing_pr is not None:
            log_info(
                f"Skipping systematic failure counter: PR #{existing_pr} already "
                f"exists for issue #{ctx.config.issue} (builder succeeded)"
            )
            return

        record_blocked_reason(
            ctx.repo_root,
            ctx.config.issue,
            error_class=error_class,
            phase="builder",
            details=details,
        )
        sf = detect_systematic_failure(ctx.repo_root)

        # Escalate: when systematic failure is detected, block the issue
        # to prevent further automated pickup (see issue #2707).
        #
        # Guard: only escalate the *current* issue if it has accumulated enough
        # failures on its own.  The systematic failure counter is cross-issue
        # (it looks at the global recent_failures window), so a fresh issue
        # could be blocked because *other* issues hit the same error class.
        # Per-issue escalation only fires when this issue's own failure count
        # reaches the threshold — preventing false-positive blocking.
        # See issue #2919.
        if sf is not None:
            per_issue_count, threshold, _ = _get_prior_failure_info(
                ctx.repo_root, ctx.config.issue
            )
            if per_issue_count < threshold:
                log_warning(
                    f"Systematic failure active (pattern={sf.pattern}, "
                    f"global count={sf.count}) but issue #{ctx.config.issue} "
                    f"only has {per_issue_count}/{threshold} per-issue failures "
                    f"— skipping escalation to loom:blocked"
                )
            else:
                transition_issue_labels(
                    ctx.config.issue,
                    add=["loom:blocked"],
                    remove=["loom:issue"],
                    repo_root=ctx.repo_root,
                )
                log_warning(
                    f"Systematic failure escalation: issue #{ctx.config.issue} "
                    f"moved to loom:blocked (pattern={sf.pattern}, count={sf.count}, "
                    f"per-issue={per_issue_count}/{threshold})"
                )
                subprocess.run(
                    [
                        "gh", "issue", "comment", str(ctx.config.issue),
                        "--body",
                        f"**Systematic failure detected** — the builder has hit "
                        f"the same error pattern (`{sf.pattern}`) **{sf.count}** "
                        f"times in a row across recent issues, and this issue "
                        f"itself has reached {per_issue_count}/{threshold} failures "
                        f"with the same pattern. Blocked as a precaution.\n\n"
                        f"### Recovery\n\n"
                        f"Investigate the failure pattern and either fix the "
                        f"underlying issue or add more guidance to the issue "
                        f"description, then remove the `loom:blocked` label to "
                        f"re-enable automated processing.\n\n"
                        f"```bash\n"
                        f"gh issue edit {ctx.config.issue} --remove-label "
                        f"loom:blocked --add-label loom:issue\n"
                        f"```",
                    ],
                    cwd=ctx.repo_root,
                    capture_output=True,
                    check=False,
                )
    except Exception:
        pass


def main(argv: list[str] | None = None) -> int:
    """Main entry point for loom-shepherd CLI."""
    # Route shepherd output through stdout to eliminate Bash tool duplication.
    # Claude Code's Bash tool captures output via two mechanisms: PTY (which
    # includes stderr) and explicit stderr capture.  Both are shown in the
    # error block when the exit code is non-zero, making every stderr line
    # appear twice.  Routing through stdout is captured by the PTY only,
    # appearing once.  Since shepherd produces only human-readable progress
    # (no machine-readable data on stdout), this is safe.  See issue #2840.
    sys.stderr = sys.stdout  # type: ignore[assignment]

    # Force line-buffered stdout so all output is visible immediately even when
    # invoked non-interactively (e.g., from the Claude Code Bash tool or piped
    # contexts). Without this, Python may block-buffer when it's not a tty,
    # causing output to be lost if the process crashes before flushing.
    # The wrapper script also sets PYTHONUNBUFFERED=1 as a belt-and-suspenders
    # measure, but this handles the case where the CLI is invoked directly.
    try:
        sys.stdout.reconfigure(line_buffering=True)  # type: ignore[union-attr]
    except (AttributeError, io.UnsupportedOperation):
        pass  # Some environments don't support reconfigure; flush=True handles it

    args = _parse_args(argv)
    config = _create_config(args)

    # Auto-navigate out of worktree before creating context.
    # This prevents issues where shepherd deletes a worktree that
    # contains its own shell session's CWD.
    repo_root = find_repo_root()
    _auto_navigate_out_of_worktree(repo_root)

    # --force / --merge implies --allow-dirty-main: the user wants fully
    # autonomous operation and the builder works in an isolated worktree,
    # so uncommitted main changes shouldn't block.
    dirty_implied_by_force = args.force and not args.allow_dirty_main
    if args.force:
        args.allow_dirty_main = True

    # Pre-flight check: warn if main repo has uncommitted changes.
    # This prevents confusion when tests pass in main but fail in worktrees
    # (or vice versa) due to uncommitted local changes.
    allow_dirty_reason = (
        "implied by --force/--merge" if dirty_implied_by_force else "--allow-dirty-main specified"
    )
    if not _check_main_repo_clean(repo_root, args.allow_dirty_main, allow_dirty_reason):
        return ShepherdExitCode.NEEDS_INTERVENTION

    # Pre-claim check: skip orchestration if the issue is already closed.
    # This avoids the unnecessary claim/release cycle for closed issues.
    from loom_tools.common.github import gh_issue_view

    meta = gh_issue_view(config.issue, fields=["state"], cwd=repo_root)
    if meta and meta.get("state", "").upper() != "OPEN":
        log_info(f"Issue #{config.issue} is already {meta['state']} - no orchestration needed")
        return ShepherdExitCode.SKIPPED

    # Acquire file-based claim to prevent concurrent shepherds on the same issue.
    # Uses atomic mkdir for mutual exclusion. TTL of 2 hours covers long runs.
    from loom_tools.claim import claim_issue, release_claim

    agent_id = f"shepherd-{config.task_id}"
    claim_result = claim_issue(repo_root, config.issue, agent_id=agent_id, ttl=7200)
    if claim_result != 0:
        log_error(
            f"Cannot start shepherd for issue #{config.issue}: "
            f"another shepherd already holds the claim"
        )
        return ShepherdExitCode.NEEDS_INTERVENTION

    ctx = ShepherdContext(config=config)

    # Print header
    _print_phase_header("SHEPHERD ORCHESTRATION STARTED")
    print(file=sys.stderr)

    exit_code = ShepherdExitCode.NEEDS_INTERVENTION
    try:
        exit_code = orchestrate(ctx)
        return exit_code
    except Exception:
        exit_code = ShepherdExitCode.NEEDS_INTERVENTION
        raise
    finally:
        if exit_code not in (ShepherdExitCode.SUCCESS, ShepherdExitCode.SKIPPED):
            _cleanup_labels_on_failure(ctx, exit_code)
        _remove_worktree_marker(ctx)
        # Always release the file-based claim on exit
        release_claim(repo_root, config.issue, agent_id)
        # Delete our own progress file.  The daemon handles this via
        # handle_shepherd_complete(), but manual /shepherd runs never
        # trigger that path, so files accumulate indefinitely.
        try:
            LoomPaths(repo_root).progress_file(config.task_id).unlink(missing_ok=True)
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
