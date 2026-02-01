"""CLI entry point for shepherd orchestration."""

from __future__ import annotations

import argparse
import os
import sys
import time
from pathlib import Path

from loom_tools.common.git import get_uncommitted_files
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.shepherd.config import ExecutionMode, Phase, QualityGates, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
    ShepherdError,
    ShutdownSignal,
)
from loom_tools.shepherd.labels import get_pr_for_issue
from loom_tools.shepherd.phases import (
    ApprovalPhase,
    BuilderPhase,
    CuratorPhase,
    DoctorPhase,
    JudgePhase,
    MergePhase,
    PhaseStatus,
    PreflightPhase,
)
# Note: run_phase_with_retry is no longer used after removing test failure
# auto-recovery (Phase 3b/3c). Kept import commented for reference.
# from loom_tools.shepherd.phases.base import run_phase_with_retry


def _print_phase_header(title: str) -> None:
    """Print a phase header with formatting to stderr for consistent ordering."""
    width = 67
    print(file=sys.stderr)
    print(f"\033[0;36m{'═' * width}\033[0m", file=sys.stderr)
    print(f"\033[0;36m  {title}\033[0m", file=sys.stderr)
    print(f"\033[0;36m{'═' * width}\033[0m", file=sys.stderr)


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


def _check_main_repo_clean(repo_root: Path, allow_dirty: bool) -> bool:
    """Check if main repository has uncommitted changes and warn.

    When running from a worktree, test results can differ between:
    - Running tests from main (includes uncommitted changes)
    - Running tests from a worktree (clean checkout at HEAD)

    This check warns users about this potential source of confusion.

    Args:
        repo_root: The resolved repository root path
        allow_dirty: If True, only warn but don't block

    Returns:
        True if clean or allowed to proceed, False if dirty and should block
    """
    uncommitted = get_uncommitted_files(cwd=repo_root)
    if not uncommitted:
        return True

    # Warn about uncommitted changes
    log_warning(f"Main repository has {len(uncommitted)} uncommitted change(s):")
    for line in uncommitted[:10]:  # Show first 10 files
        # Parse porcelain format: "XY filename"
        status = line[:2].strip()
        filename = line[3:] if len(line) > 3 else line
        print(f"  {status} {filename}", file=sys.stderr)
    if len(uncommitted) > 10:
        print(f"  ... and {len(uncommitted) - 10} more", file=sys.stderr)
    print(file=sys.stderr)

    if allow_dirty:
        log_warning("Proceeding anyway (--allow-dirty-main specified)")
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

    config = ShepherdConfig(
        issue=args.issue,
        mode=mode,
        start_from=start_from,
        stop_after=args.stop_after,
        quality_gates=quality_gates,
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
        Exit code (0 for success, non-zero for error)
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
            return 0

        log_info(f"Issue: #{ctx.config.issue}")
        log_info(f"Mode: {ctx.config.mode.value}")
        if ctx.config.start_from:
            log_info(f"Start from: {ctx.config.start_from.value} phase")
        log_info(f"Task ID: {ctx.config.task_id}")
        log_info(f"Repository: {ctx.repo_root}")
        log_info(f"Title: {ctx.issue_title}")
        print(file=sys.stderr)

        # Report started milestone
        ctx.report_milestone("started", issue=ctx.config.issue, mode=ctx.config.mode.value)

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
                return 1

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
            return 0

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
            return 0

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
                return 1

            # Log result inline (no header for passing checks)
            log_info(f"Baseline health: {result.message}")

        # ─── PHASE 3: Builder ─────────────────────────────────────────────
        builder = BuilderPhase()
        skip, reason = builder.should_skip(ctx)

        if skip:
            log_info(f"Skipping builder phase ({reason})")
            completed_phases.append(f"Builder ({reason})")
        else:
            _print_phase_header("PHASE 3: BUILDER")
            phase_start = time.time()
            result = builder.run(ctx)
            elapsed = int(time.time() - phase_start)

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                # Check if this is a test failure with preserved worktree
                if result.data.get("test_failure"):
                    # Test failure: mark with explicit label and STOP
                    # No auto-recovery via Doctor - failures should be visible
                    log_error(f"Builder test verification failed ({elapsed}s)")
                    phase_durations["Builder"] = elapsed
                    ctx.report_milestone(
                        "phase_completed", phase="builder", duration_seconds=elapsed, status="test_failure"
                    )
                    _mark_builder_test_failure(ctx)
                    return 1
                else:
                    log_error(result.message)
                    return 1

            if result.status == PhaseStatus.SKIPPED:
                completed_phases.append(f"Builder ({result.message})")
            else:
                phase_durations["Builder"] = elapsed
                completed_phases.append(f"Builder (PR #{ctx.pr_number})")
                log_success(f"Builder phase complete - PR #{ctx.pr_number} created ({elapsed}s)")
                ctx.report_milestone(
                    "phase_completed", phase="builder", duration_seconds=elapsed, status="success"
                )

        # ─── PHASE 4/5: Judge/Doctor Loop ─────────────────────────────────

        # Precondition: PR must exist before entering Judge phase.
        # If builder failed without creating a PR (e.g., unexpected error,
        # timeout, or manual interruption), we cannot proceed to Judge.
        # This is a precondition failure, not a retryable error.
        if ctx.pr_number is None:
            log_error("Cannot enter Judge phase: no PR was created during Builder phase")
            _mark_builder_no_pr(ctx)
            return 1

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
                # Judge returned FAILED/STUCK with no label outcome.
                # Retry the judge phase before giving up (defense-in-depth
                # for cases where the judge worker silently fails without
                # submitting a review, leaving no loom:pr or
                # loom:changes-requested label).
                if judge_retries < ctx.config.judge_max_retries:
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
                    continue

                log_error(
                    f"Judge phase failed after {judge_retries} "
                    f"retry attempt(s): {result.message}"
                )
                _mark_judge_exhausted(ctx, judge_retries)
                return 1

            # Judge succeeded — reset retry counter for this loop iteration
            judge_retries = 0

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
                    return 1

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
                    log_error(result.message)
                    return 1

                completed_phases.append("Doctor (fixes applied)")
                log_success(f"Doctor applied fixes ({elapsed}s)")
                ctx.report_milestone(
                    "phase_completed",
                    phase="doctor",
                    duration_seconds=elapsed,
                    status="success",
                )
            else:
                # Unexpected result — neither approved, changes_requested,
                # nor FAILED/STUCK. Treat as a judge failure and retry.
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
                    continue

                log_error(
                    f"Judge phase returned unexpected result after {judge_retries} "
                    f"retry attempt(s): {result.message}"
                )
                _mark_judge_exhausted(ctx, judge_retries)
                return 1

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
            return 0

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
            return 1

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
            log_info(f"To merge manually: gh pr merge {ctx.pr_number} --squash --delete-branch")

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

        return 0

    except ShutdownSignal as e:
        log_warning(f"Shutdown signal detected: {e}")
        ctx.report_milestone(
            "blocked",
            reason="shutdown_signal",
            details=str(e),
        )
        log_info("Cleaning up and exiting gracefully...")
        return 0

    except IssueNotFoundError as e:
        log_error(str(e))
        return 1

    except IssueBlockedError as e:
        log_error(str(e))
        log_info("Use --force to override blocked status")
        return 1

    except ShepherdError as e:
        log_error(str(e))
        return 1


def _mark_builder_test_failure(ctx: ShepherdContext) -> None:
    """Mark issue with loom:failed:builder-tests after test verification failed.

    This replaces the old auto-recovery behavior. Instead of attempting to
    fix tests via Doctor, we now mark the failure explicitly and stop.
    The worktree and branch are preserved for manual intervention.
    """
    import subprocess

    # Atomic transition: loom:building -> loom:failed:builder-tests
    subprocess.run(
        [
            "gh",
            "issue",
            "edit",
            str(ctx.config.issue),
            "--remove-label",
            "loom:building",
            "--add-label",
            "loom:failed:builder-tests",
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
            f"gh pr create --label loom:review-requested --body 'Closes #{ctx.config.issue}'\n"
            f"gh issue edit {ctx.config.issue} --remove-label loom:failed:builder-tests\n"
            f"```\n\n"
            f"**Option B: Reset and retry**\n"
            f"```bash\n"
            f"git worktree remove {ctx.worktree_path or '.loom/worktrees/issue-' + str(ctx.config.issue)} --force\n"
            f"gh issue edit {ctx.config.issue} --remove-label loom:failed:builder-tests --add-label loom:issue\n"
            f"```",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _mark_doctor_exhausted(ctx: ShepherdContext) -> None:
    """Mark issue with loom:failed:doctor due to retry exhaustion."""
    import subprocess

    # Atomic transition: loom:building -> loom:failed:doctor
    subprocess.run(
        [
            "gh",
            "issue",
            "edit",
            str(ctx.config.issue),
            "--remove-label",
            "loom:building",
            "--add-label",
            "loom:failed:doctor",
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
        error_class="doctor_exhausted",
        phase="doctor",
        details=f"max retries ({ctx.config.doctor_max_retries}) exceeded",
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
            f"**Doctor phase failed**: Could not resolve Judge feedback after "
            f"{ctx.config.doctor_max_retries} attempts. Manual intervention required.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _mark_judge_exhausted(ctx: ShepherdContext, retries: int) -> None:
    """Mark issue with loom:failed:judge due to retry exhaustion."""
    import subprocess

    # Atomic transition: loom:building -> loom:failed:judge
    subprocess.run(
        [
            "gh",
            "issue",
            "edit",
            str(ctx.config.issue),
            "--remove-label",
            "loom:building",
            "--add-label",
            "loom:failed:judge",
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


def _mark_builder_no_pr(ctx: ShepherdContext) -> None:
    """Mark issue with loom:failed:builder because no PR was created.

    This handles the case where Builder completes without creating a PR,
    which is a precondition failure for the Judge phase. This covers unexpected
    errors, timeouts, or manual interruptions that leave no PR behind.
    """
    import subprocess

    # Atomic transition: loom:building -> loom:failed:builder
    subprocess.run(
        [
            "gh",
            "issue",
            "edit",
            str(ctx.config.issue),
            "--remove-label",
            "loom:building",
            "--add-label",
            "loom:failed:builder",
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
        error_class="builder_no_pr",
        phase="builder",
        details="Builder phase completed but no PR was created",
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
            "**Builder phase failed**: No PR was created. "
            "Cannot proceed to Judge phase without a PR to review. "
            "Worktree and branch may be preserved for manual investigation.",
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

    from loom_tools.shepherd.phases import PhaseResult  # noqa: F811

    issue_tracking = result.data.get("issue_tracking", "")
    failing_tests = result.data.get("failing_tests", [])

    # Atomic transition: loom:building -> loom:blocked
    # (issue may or may not have loom:building at this point)
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


def main(argv: list[str] | None = None) -> int:
    """Main entry point for loom-shepherd CLI."""
    args = _parse_args(argv)
    config = _create_config(args)

    # Auto-navigate out of worktree before creating context.
    # This prevents issues where shepherd deletes a worktree that
    # contains its own shell session's CWD.
    repo_root = find_repo_root()
    _auto_navigate_out_of_worktree(repo_root)

    # Pre-flight check: warn if main repo has uncommitted changes.
    # This prevents confusion when tests pass in main but fail in worktrees
    # (or vice versa) due to uncommitted local changes.
    if not _check_main_repo_clean(repo_root, args.allow_dirty_main):
        return 1

    ctx = ShepherdContext(config=config)

    # Print header
    _print_phase_header("SHEPHERD ORCHESTRATION STARTED")
    print(file=sys.stderr)

    try:
        return orchestrate(ctx)
    finally:
        _remove_worktree_marker(ctx)


if __name__ == "__main__":
    sys.exit(main())
