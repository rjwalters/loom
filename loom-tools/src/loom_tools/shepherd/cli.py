"""CLI entry point for shepherd orchestration."""

from __future__ import annotations

import argparse
import os
import sys
import time
from pathlib import Path

from loom_tools.common.git import get_commit_count
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.shepherd.config import ExecutionMode, Phase, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
    ShepherdError,
    ShutdownSignal,
)
from loom_tools.shepherd.labels import (
    add_issue_label,
    get_pr_for_issue,
    remove_issue_label,
)
from loom_tools.shepherd.phases import (
    ApprovalPhase,
    BuilderPhase,
    CuratorPhase,
    DoctorPhase,
    JudgePhase,
    MergePhase,
    PhaseStatus,
)
from loom_tools.shepherd.phases.base import run_phase_with_retry


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

    config = ShepherdConfig(
        issue=args.issue,
        mode=mode,
        start_from=start_from,
        stop_after=args.stop_after,
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

        # ─── PHASE 3: Builder ─────────────────────────────────────────────
        builder = BuilderPhase()
        skip, reason = builder.should_skip(ctx)

        test_failure_recovery = False

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
                    log_warning(f"Builder test verification failed ({elapsed}s) — routing to Doctor for fix")
                    completed_phases.append(f"Builder (test failure, worktree preserved)")
                    phase_durations["Builder"] = elapsed
                    ctx.report_milestone(
                        "phase_completed", phase="builder", duration_seconds=elapsed, status="test_failure"
                    )
                    test_failure_recovery = True
                else:
                    log_error(result.message)
                    return 1

            if not test_failure_recovery:
                if result.status == PhaseStatus.SKIPPED:
                    completed_phases.append(f"Builder ({result.message})")
                else:
                    phase_durations["Builder"] = elapsed
                    completed_phases.append(f"Builder (PR #{ctx.pr_number})")
                    log_success(f"Builder phase complete - PR #{ctx.pr_number} created ({elapsed}s)")
                    ctx.report_milestone(
                        "phase_completed", phase="builder", duration_seconds=elapsed, status="success"
                    )

        # ─── Test Failure Recovery (Doctor fixes failing tests) ────────────
        if test_failure_recovery:
            _print_phase_header("PHASE 3b: DOCTOR (test failure recovery)")

            # Pre-check: are the failures related to the builder's changes?
            builder_phase = BuilderPhase()
            test_info = builder_phase._detect_test_command(ctx.worktree_path) if ctx.worktree_path else None
            test_cmd = test_info[0] if test_info else []

            if test_cmd and builder_phase.should_skip_doctor_recovery(ctx, test_cmd):
                log_warning(
                    "Skipping Doctor: test failures are unrelated to builder's "
                    "changes (no changed files affect the failing test ecosystem)"
                )
                completed_phases.append("Doctor test fix (skipped — unrelated failures)")
                ctx.report_milestone(
                    "phase_completed",
                    phase="doctor_testfix",
                    duration_seconds=0,
                    status="skipped_unrelated",
                )
                # Treat as pre-existing failures — restore labels and continue
                # to PR creation. We skip Phase 3c (full builder re-run)
                # because it would re-run test verification which would fail
                # again on the same pre-existing errors.
                remove_issue_label(ctx.config.issue, "loom:needs-fix", ctx.repo_root)
                add_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
                ctx.label_cache.invalidate_issue(ctx.config.issue)

                # Check for existing PR or let the builder create one
                pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
                if pr is not None:
                    ctx.pr_number = pr
                test_failure_recovery = False
            else:
                phase_start = time.time()

                # Restore loom:building label for Doctor phase
                remove_issue_label(ctx.config.issue, "loom:needs-fix", ctx.repo_root)
                add_issue_label(ctx.config.issue, "loom:building", ctx.repo_root)
                ctx.label_cache.invalidate_issue(ctx.config.issue)

                # Record commit count before Doctor so we can detect if it changed anything
                commits_before = get_commit_count(cwd=ctx.worktree_path)

                # Doctor works in the same worktree to fix tests.
                # Pass context file path so doctor knows what failed.
                doctor_args = f"--test-fix {ctx.config.issue}"
                if ctx.worktree_path:
                    context_file = ctx.worktree_path / ".loom-test-failure-context.json"
                    if context_file.is_file():
                        doctor_args += f" --context {context_file}"

                exit_code = run_phase_with_retry(
                    ctx,
                    role="doctor",
                    name=f"doctor-testfix-{ctx.config.issue}",
                    timeout=ctx.config.doctor_timeout,
                    max_retries=ctx.config.stuck_max_retries,
                    phase="doctor",
                    worktree=ctx.worktree_path,
                    args=doctor_args,
                )
                elapsed = int(time.time() - phase_start)

                if exit_code == 3:
                    raise ShutdownSignal("shutdown signal detected during doctor test fix")

                if exit_code == 5:
                    # Doctor explicitly signaled failures are pre-existing
                    log_info("Doctor determined failures are pre-existing (exit code 5)")
                    completed_phases.append(f"Doctor test fix (pre-existing — explicit signal, {elapsed}s)")
                    ctx.report_milestone(
                        "phase_completed",
                        phase="doctor_testfix",
                        duration_seconds=elapsed,
                        status="preexisting_explicit",
                    )
                    # Check for existing PR or let the builder create one
                    pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
                    if pr is not None:
                        ctx.pr_number = pr
                    # Skip re-verification and continue to PR creation
                    test_failure_recovery = False
                elif exit_code not in (0, 3, 5):
                    log_error(f"Doctor test fix failed (exit code {exit_code})")
                    completed_phases.append("Doctor test fix (failed)")
                    _mark_test_fix_failed(ctx)
                    return 1

                # Check if Doctor actually made any commits (fallback for exit code 0)
                if exit_code == 0:
                    commits_after = get_commit_count(cwd=ctx.worktree_path)
                    doctor_made_changes = commits_after > commits_before
                else:
                    # For exit code 5, we already handled it above
                    doctor_made_changes = False

                # Only process exit code 0 (explicit pre-existing handled above)
                if exit_code == 0:
                    if not doctor_made_changes:
                        # Doctor made no commits — failures are pre-existing.
                        # Skip re-verification to avoid non-deterministic comparison
                        # producing worse results (see #1935, #1937).
                        log_warning(
                            "Doctor made no commits — treating test failures as "
                            "pre-existing (skipping re-verification)"
                        )
                        completed_phases.append(f"Doctor test fix (no changes — pre-existing failures, {elapsed}s)")
                        ctx.report_milestone(
                            "phase_completed",
                            phase="doctor_testfix",
                            duration_seconds=elapsed,
                            status="no_changes",
                        )
                    else:
                        # Re-run test verification after Doctor fix
                        retest_result = builder_phase._run_test_verification(ctx)

                        if retest_result is not None and retest_result.status == PhaseStatus.FAILED:
                            log_error(f"Tests still failing after Doctor fix: {retest_result.message}")
                            completed_phases.append("Doctor test fix (tests still failing)")
                            # Mark blocked since Doctor couldn't fix it
                            _mark_test_fix_failed(ctx)
                            return 1

                        completed_phases.append(f"Doctor test fix (tests fixed, {elapsed}s)")
                        log_success(f"Doctor fixed failing tests ({elapsed}s)")
                        ctx.report_milestone(
                            "phase_completed", phase="doctor_testfix", duration_seconds=elapsed, status="success"
                        )

                    # Validate and find/create PR
                    if not builder_phase.validate(ctx):
                        log_warning("Builder validation failed after Doctor test fix — running builder to create PR")

                    # Check for PR
                    pr = get_pr_for_issue(ctx.config.issue, repo_root=ctx.repo_root)
                    if pr is not None:
                        ctx.pr_number = pr

            # If no PR exists yet and Doctor actually ran, the builder
            # didn't get that far. Re-run builder to create the PR.
            # When Doctor was skipped (test_failure_recovery=False),
            # Phase 3c would re-run test verification which would fail
            # again on the same pre-existing errors, so we skip it.
            #
            # When Phase 3c does run, we skip test verification because:
            # 1. If Doctor made no changes, failures are pre-existing
            # 2. If Doctor made changes, tests were already re-verified above
            # See issue #1946 for context on this fix.
            if ctx.pr_number is None and test_failure_recovery:
                _print_phase_header("PHASE 3c: BUILDER (PR creation after test fix)")
                phase_start = time.time()

                # Re-run builder to create PR, skipping test verification
                # since tests were already verified (or failures are pre-existing)
                result = builder.run(ctx, skip_test_verification=True)
                elapsed = int(time.time() - phase_start)

                if result.is_shutdown:
                    raise ShutdownSignal(result.message)

                if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                    log_error(result.message)
                    return 1

                phase_durations["Builder (post-fix)"] = elapsed
                if ctx.pr_number:
                    completed_phases.append(f"Builder post-fix (PR #{ctx.pr_number})")
                    log_success(f"PR #{ctx.pr_number} created after test fix ({elapsed}s)")

        # ─── PHASE 4/5: Judge/Doctor Loop ─────────────────────────────────
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
            _print_phase_header(f"PHASE 4: JUDGE (attempt {doctor_attempts + 1})")

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


def _mark_test_fix_failed(ctx: ShepherdContext) -> None:
    """Mark issue as blocked after Doctor failed to fix tests."""
    import subprocess

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
        error_class="test_fix_failed",
        phase="doctor",
        details="Doctor could not fix failing tests after builder implementation",
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
            "**Shepherd blocked**: Doctor could not fix failing tests. "
            "Worktree and branch are preserved for manual intervention.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _mark_doctor_exhausted(ctx: ShepherdContext) -> None:
    """Mark issue as blocked due to doctor retry exhaustion."""
    import subprocess

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
            f"**Shepherd blocked**: Doctor could not resolve Judge feedback after {ctx.config.doctor_max_retries} attempts.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def _mark_judge_exhausted(ctx: ShepherdContext, retries: int) -> None:
    """Mark issue as blocked due to judge retry exhaustion."""
    import subprocess

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
            f"**Shepherd blocked**: Judge phase failed after {retries} retry attempt(s). "
            "No approval or changes-requested outcome was produced. "
            "This may indicate a judge worker failure — see #1908 for related diagnostics.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def main(argv: list[str] | None = None) -> int:
    """Main entry point for loom-shepherd CLI."""
    args = _parse_args(argv)
    config = _create_config(args)

    # Auto-navigate out of worktree before creating context.
    # This prevents issues where shepherd deletes a worktree that
    # contains its own shell session's CWD.
    repo_root = find_repo_root()
    _auto_navigate_out_of_worktree(repo_root)

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
