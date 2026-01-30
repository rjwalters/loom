"""CLI entry point for shepherd orchestration."""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.shepherd.config import ExecutionMode, Phase, ShepherdConfig
from loom_tools.shepherd.context import ShepherdContext
from loom_tools.shepherd.errors import (
    IssueBlockedError,
    IssueClosedError,
    IssueNotFoundError,
    ShepherdError,
    ShutdownSignal,
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


def _print_phase_header(title: str) -> None:
    """Print a phase header with formatting."""
    width = 67
    print()
    print(f"\033[0;36m{'═' * width}\033[0m")
    print(f"\033[0;36m  {title}\033[0m")
    print(f"\033[0;36m{'═' * width}\033[0m")


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
        print()

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
            result = curator.run(ctx)

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status == PhaseStatus.FAILED:
                log_error(result.message)
                return 1

            if result.status == PhaseStatus.SKIPPED:
                completed_phases.append(f"Curator ({result.message})")
            else:
                completed_phases.append("Curator")
                log_success("Curator phase complete")

        if ctx.config.stop_after == "curated":
            _print_phase_header("STOPPING: Reached --to curated")
            return 0

        # ─── PHASE 2: Approval Gate ───────────────────────────────────────
        _print_phase_header("PHASE 2: APPROVAL GATE")
        approval = ApprovalPhase()
        result = approval.run(ctx)

        if result.is_shutdown:
            raise ShutdownSignal(result.message)

        completed_phases.append(f"Approval ({result.message.split('(')[-1].rstrip(')')}")
        if result.status == PhaseStatus.SUCCESS:
            log_success(result.message)

        if ctx.config.stop_after == "approved":
            _print_phase_header("STOPPING: Reached --to approved")
            return 0

        # ─── PHASE 3: Builder ─────────────────────────────────────────────
        builder = BuilderPhase()
        skip, reason = builder.should_skip(ctx)

        if skip:
            log_info(f"Skipping builder phase ({reason})")
            completed_phases.append(f"Builder ({reason})")
        else:
            _print_phase_header("PHASE 3: BUILDER")
            result = builder.run(ctx)

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                log_error(result.message)
                return 1

            if result.status == PhaseStatus.SKIPPED:
                completed_phases.append(f"Builder ({result.message})")
            else:
                completed_phases.append(f"Builder (PR #{ctx.pr_number})")
                log_success(f"Builder phase complete - PR #{ctx.pr_number} created")

        # ─── PHASE 4/5: Judge/Doctor Loop ─────────────────────────────────
        doctor_attempts = 0
        pr_approved = False

        # Check for --from merge skip
        judge = JudgePhase()
        skip, reason = judge.should_skip(ctx)

        if skip:
            log_info(f"Skipping judge phase ({reason})")
            completed_phases.append(f"Judge ({reason})")
            pr_approved = True

        while not pr_approved and doctor_attempts < ctx.config.doctor_max_retries:
            _print_phase_header(f"PHASE 4: JUDGE (attempt {doctor_attempts + 1})")

            result = judge.run(ctx)

            if result.is_shutdown:
                raise ShutdownSignal(result.message)

            if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                log_error(result.message)
                return 1

            if result.data.get("approved"):
                pr_approved = True
                completed_phases.append("Judge (approved)")
                log_success(f"PR #{ctx.pr_number} approved by Judge")
            elif result.data.get("changes_requested"):
                log_warning(f"Judge requested changes on PR #{ctx.pr_number}")
                completed_phases.append("Judge (changes requested)")

                doctor_attempts += 1

                if doctor_attempts >= ctx.config.doctor_max_retries:
                    log_error(f"Doctor max retries ({ctx.config.doctor_max_retries}) exceeded")
                    _mark_doctor_exhausted(ctx)
                    return 1

                # ─── Doctor Phase ─────────────────────────────────────
                _print_phase_header(f"PHASE 5: DOCTOR (attempt {doctor_attempts})")

                doctor = DoctorPhase()
                result = doctor.run(ctx)

                if result.is_shutdown:
                    raise ShutdownSignal(result.message)

                if result.status in (PhaseStatus.FAILED, PhaseStatus.STUCK):
                    log_error(result.message)
                    return 1

                completed_phases.append("Doctor (fixes applied)")
                log_success("Doctor applied fixes")
            else:
                log_error(result.message)
                return 1

        if ctx.config.stop_after == "pr":
            _print_phase_header("STOPPING: Reached --to pr")
            return 0

        # ─── PHASE 6: Merge Gate ──────────────────────────────────────────
        _print_phase_header("PHASE 6: MERGE GATE")
        merge = MergePhase()
        result = merge.run(ctx)

        if result.is_shutdown:
            raise ShutdownSignal(result.message)

        if result.status == PhaseStatus.FAILED:
            log_error(result.message)
            return 1

        if result.data.get("merged"):
            completed_phases.append("Merge (auto-merged)")
            log_success(f"PR #{ctx.pr_number} merged successfully")
        else:
            completed_phases.append("Merge (awaiting merge)")
            log_info(f"PR #{ctx.pr_number} is approved and ready for Champion to merge")
            log_info(f"To merge manually: gh pr merge {ctx.pr_number} --squash --delete-branch")

        # ─── Complete ─────────────────────────────────────────────────────
        duration = int(time.time() - start_time)

        # Report completion
        if ctx.config.is_force_mode:
            ctx.report_milestone("completed", pr_merged=True)
        else:
            ctx.report_milestone("completed")

        _print_phase_header("SHEPHERD ORCHESTRATION COMPLETE")
        print()
        log_info(f"Issue: #{ctx.config.issue} - {ctx.issue_title}")
        log_info(f"Mode: {ctx.config.mode.value}")
        log_info(f"Duration: {duration}s")
        print()
        log_info("Phases completed:")
        for phase in completed_phases:
            print(f"  - {phase}")
        print()
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

    # Record blocked reason
    ctx.run_script(
        "record-blocked-reason.sh",
        [
            str(ctx.config.issue),
            "--error-class",
            "doctor_exhausted",
            "--phase",
            "doctor",
            "--details",
            f"max retries ({ctx.config.doctor_max_retries}) exceeded",
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
            f"**Shepherd blocked**: Doctor could not resolve Judge feedback after {ctx.config.doctor_max_retries} attempts.",
        ],
        cwd=ctx.repo_root,
        capture_output=True,
        check=False,
    )


def main(argv: list[str] | None = None) -> int:
    """Main entry point for loom-shepherd CLI."""
    args = _parse_args(argv)
    config = _create_config(args)
    ctx = ShepherdContext(config=config)

    # Print header
    _print_phase_header("SHEPHERD ORCHESTRATION STARTED")
    print()

    try:
        return orchestrate(ctx)
    finally:
        _remove_worktree_marker(ctx)


if __name__ == "__main__":
    sys.exit(main())
