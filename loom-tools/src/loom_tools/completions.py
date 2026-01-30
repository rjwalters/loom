"""Check task completions and detect silent failures.

This module polls all active task IDs from daemon-state.json and checks if
their output files indicate completion. It detects silent failures where
tasks have exited but issues are still in loom:building state.

Detects:
    - completed: Task completed successfully
    - errored: Task exited with error
    - stale: No heartbeat for extended period
    - orphaned: Issue in loom:building but no active task
    - missing_output: Output file doesn't exist

Exit codes:
    0 - All tasks healthy (or recovered with --recover)
    1 - Silent failures detected
    2 - State file not found
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.common.github import gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_daemon_state, read_json_file
from loom_tools.common.time_utils import elapsed_seconds, now_utc
from loom_tools.models.daemon_state import DaemonState

# Staleness thresholds (in seconds)
HEARTBEAT_STALE_THRESHOLD = int(os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD", "300"))
OUTPUT_STALE_THRESHOLD = int(os.environ.get("LOOM_OUTPUT_STALE_THRESHOLD", "600"))


@dataclass
class TaskStatus:
    """Status of a single task."""

    id: str
    category: str  # "shepherd" or "support"
    issue: int | None
    task_id: str | None
    status: str  # "completed", "errored", "stale", "orphaned", "running", "missing_output"
    reason: str | None = None


@dataclass
class CompletionReport:
    """Report of task completion checks."""

    completed: list[TaskStatus] = field(default_factory=list)
    errored: list[TaskStatus] = field(default_factory=list)
    stale: list[TaskStatus] = field(default_factory=list)
    orphaned: list[int] = field(default_factory=list)
    running: list[TaskStatus] = field(default_factory=list)
    recoveries: list[str] = field(default_factory=list)

    @property
    def has_failures(self) -> bool:
        return len(self.errored) > 0 or len(self.orphaned) > 0


def _get_file_mtime(path: pathlib.Path) -> float:
    """Get file modification time as epoch seconds."""
    try:
        return path.stat().st_mtime
    except (FileNotFoundError, OSError):
        return 0


def _check_output_for_completion(output_file: pathlib.Path) -> str | None:
    """Check output file for completion/error markers.

    Returns:
        "completed" if successful exit, "errored" if error exit, None if still running.
    """
    if not output_file.exists():
        return None

    try:
        content = output_file.read_text()
        if "AGENT_EXIT_CODE=0" in content:
            return "completed"
        elif "AGENT_EXIT_CODE=" in content:
            return "errored"
    except (OSError, UnicodeDecodeError):
        pass

    return None


def check_shepherd_tasks(
    repo_root: pathlib.Path,
    daemon_state: DaemonState,
    verbose: bool = False,
) -> tuple[list[TaskStatus], list[TaskStatus], list[TaskStatus], list[TaskStatus]]:
    """Check all shepherd tasks and categorize them.

    Returns:
        Tuple of (completed, errored, stale, running) lists.
    """
    progress_dir = repo_root / ".loom" / "progress"
    now_epoch = now_utc().timestamp()

    completed = []
    errored = []
    stale = []
    running = []

    for shepherd_id, shepherd in daemon_state.shepherds.items():
        if verbose:
            log_info(
                f"Checking shepherd {shepherd_id}: "
                f"status={shepherd.status}, issue={shepherd.issue}, task_id={shepherd.task_id}"
            )

        # Skip idle shepherds
        if shepherd.status == "idle":
            if verbose:
                log_info(f"  Skipping {shepherd_id} (idle)")
            continue

        if shepherd.status != "working":
            continue

        task_id = shepherd.task_id
        issue = shepherd.issue

        # Check progress file for heartbeat
        if task_id and progress_dir.is_dir():
            progress_file = progress_dir / f"shepherd-{task_id}.json"
            if progress_file.exists():
                progress_data = read_json_file(progress_file)
                if isinstance(progress_data, dict):
                    last_heartbeat = progress_data.get("last_heartbeat")
                    if last_heartbeat:
                        try:
                            heartbeat_age = elapsed_seconds(last_heartbeat)
                            if verbose:
                                log_info(f"  Heartbeat age: {heartbeat_age}s")

                            if heartbeat_age > HEARTBEAT_STALE_THRESHOLD:
                                stale.append(
                                    TaskStatus(
                                        id=shepherd_id,
                                        category="shepherd",
                                        issue=issue,
                                        task_id=task_id,
                                        status="stale",
                                        reason=f"heartbeat_stale:{heartbeat_age}s",
                                    )
                                )
                                log_warning(f"Shepherd {shepherd_id} has stale heartbeat ({heartbeat_age}s)")
                                continue
                        except Exception:
                            pass

                    # Check progress file status
                    progress_status = progress_data.get("status", "working")
                    if progress_status == "completed":
                        completed.append(
                            TaskStatus(
                                id=shepherd_id,
                                category="shepherd",
                                issue=issue,
                                task_id=task_id,
                                status="completed",
                            )
                        )
                        log_success(f"Shepherd {shepherd_id} completed (issue #{issue})")
                        continue
                    elif progress_status == "errored":
                        errored.append(
                            TaskStatus(
                                id=shepherd_id,
                                category="shepherd",
                                issue=issue,
                                task_id=task_id,
                                status="errored",
                                reason="progress_error",
                            )
                        )
                        log_error(f"Shepherd {shepherd_id} errored (issue #{issue})")
                        continue

        # Check output file for direct mode tasks
        execution_mode = shepherd.execution_mode or "direct"
        output_file_path = shepherd.output_file

        if execution_mode == "direct" and output_file_path:
            output_file = pathlib.Path(output_file_path)

            if not output_file.exists():
                errored.append(
                    TaskStatus(
                        id=shepherd_id,
                        category="shepherd",
                        issue=issue,
                        task_id=task_id,
                        status="errored",
                        reason="missing_output",
                    )
                )
                log_warning(f"Shepherd {shepherd_id} output file missing: {output_file_path}")
                continue

            # Check output file modification time
            output_mtime = _get_file_mtime(output_file)
            output_age = int(now_epoch - output_mtime)

            if output_age > OUTPUT_STALE_THRESHOLD:
                stale.append(
                    TaskStatus(
                        id=shepherd_id,
                        category="shepherd",
                        issue=issue,
                        task_id=task_id,
                        status="stale",
                        reason=f"output_stale:{output_age}s",
                    )
                )
                log_warning(f"Shepherd {shepherd_id} has stale output ({output_age}s)")
                continue

            # Check output for completion/error markers
            result = _check_output_for_completion(output_file)
            if result == "completed":
                completed.append(
                    TaskStatus(
                        id=shepherd_id,
                        category="shepherd",
                        issue=issue,
                        task_id=task_id,
                        status="completed",
                    )
                )
                log_success(f"Shepherd {shepherd_id} completed (issue #{issue})")
                continue
            elif result == "errored":
                errored.append(
                    TaskStatus(
                        id=shepherd_id,
                        category="shepherd",
                        issue=issue,
                        task_id=task_id,
                        status="errored",
                        reason="exit_error",
                    )
                )
                log_error(f"Shepherd {shepherd_id} exited with error (issue #{issue})")
                continue

        # Still running
        running.append(
            TaskStatus(
                id=shepherd_id,
                category="shepherd",
                issue=issue,
                task_id=task_id,
                status="running",
            )
        )
        if verbose:
            log_info(f"  {shepherd_id} still running")

    return completed, errored, stale, running


def check_support_role_tasks(
    daemon_state: DaemonState,
    verbose: bool = False,
) -> tuple[list[TaskStatus], list[TaskStatus], list[TaskStatus], list[TaskStatus]]:
    """Check all support role tasks and categorize them.

    Returns:
        Tuple of (completed, errored, stale, running) lists.
    """
    now_epoch = now_utc().timestamp()

    completed = []
    errored = []
    stale = []
    running = []

    for role_name, role in daemon_state.support_roles.items():
        if verbose:
            log_info(f"Checking support role {role_name}: status={role.status}, task_id={role.task_id}")

        # Skip idle roles
        if role.status == "idle":
            if verbose:
                log_info(f"  Skipping {role_name} (idle)")
            continue

        if role.status != "running":
            continue

        task_id = role.task_id

        # For support roles, we don't have output_file in the model
        # We can only check based on last_completed timestamp
        if role.last_completed:
            try:
                age = elapsed_seconds(role.last_completed)
                if age > OUTPUT_STALE_THRESHOLD:
                    stale.append(
                        TaskStatus(
                            id=role_name,
                            category="support",
                            issue=None,
                            task_id=task_id,
                            status="stale",
                            reason=f"output_stale:{age}s",
                        )
                    )
                    log_warning(f"Support role {role_name} has stale output ({age}s)")
                    continue
            except Exception:
                pass

        # Still running
        running.append(
            TaskStatus(
                id=role_name,
                category="support",
                issue=None,
                task_id=task_id,
                status="running",
            )
        )
        if verbose:
            log_info(f"  {role_name} still running")

    return completed, errored, stale, running


def check_orphaned_building(
    daemon_state: DaemonState,
) -> list[int]:
    """Check for issues labeled loom:building but not tracked by any shepherd."""
    # Get tracked issues from active shepherds
    tracked_issues = {
        s.issue
        for s in daemon_state.shepherds.values()
        if s.status == "working" and s.issue is not None
    }

    # Query GitHub for loom:building issues
    try:
        result = gh_run(
            ["issue", "list", "--label", "loom:building", "--state", "open", "--json", "number"],
            check=False,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return []
        building_issues = json.loads(result.stdout)
    except Exception:
        return []

    orphaned = []
    for issue in building_issues:
        issue_num = issue.get("number")
        if issue_num and issue_num not in tracked_issues:
            orphaned.append(issue_num)

    return orphaned


def recover_issue(issue_number: int, dry_run: bool = False) -> bool:
    """Recover an issue by reverting labels from loom:building to loom:issue.

    Returns:
        True if recovery succeeded, False otherwise.
    """
    if dry_run:
        log_info(f"  Would revert issue #{issue_number} from loom:building to loom:issue")
        return True

    try:
        result = gh_run(
            [
                "issue",
                "edit",
                str(issue_number),
                "--remove-label",
                "loom:building",
                "--add-label",
                "loom:issue",
            ],
            check=False,
        )

        if result.returncode == 0:
            log_success(f"  Reverted issue #{issue_number} to loom:issue")

            # Add recovery comment
            from loom_tools.common.time_utils import now_utc

            timestamp = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
            comment = f"""**Silent Failure Recovery**

This issue was automatically recovered after a silent task failure.

**What happened**:
- The shepherd task exited without completing
- The issue was left in `loom:building` state

**Action taken**:
- Returned to `loom:issue` state for re-processing

---
*Recovered by loom-check-completions at {timestamp}*"""

            gh_run(
                ["issue", "comment", str(issue_number), "--body", comment],
                check=False,
            )
            return True
        else:
            log_error(f"  Failed to revert issue #{issue_number}")
            return False
    except Exception:
        log_error(f"  Failed to revert issue #{issue_number}")
        return False


def run_check(
    verbose: bool = False,
    recover: bool = False,
    dry_run: bool = False,
    json_output: bool = False,
) -> CompletionReport:
    """Run completion checks and return a report."""
    report = CompletionReport()

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return report

    state_file = repo_root / ".loom" / "daemon-state.json"

    if not state_file.exists():
        if json_output:
            print(json.dumps({"error": "state_file_not_found", "file": str(state_file)}))
        else:
            log_error(f"State file not found: {state_file}")
        return report

    # Load state
    daemon_state = read_daemon_state(repo_root)

    # Check shepherd tasks
    if not json_output:
        log_info("Checking shepherd tasks...")

    s_completed, s_errored, s_stale, s_running = check_shepherd_tasks(
        repo_root, daemon_state, verbose
    )
    report.completed.extend(s_completed)
    report.errored.extend(s_errored)
    report.stale.extend(s_stale)
    report.running.extend(s_running)

    # Check support role tasks
    if not json_output:
        log_info("Checking support role tasks...")

    r_completed, r_errored, r_stale, r_running = check_support_role_tasks(
        daemon_state, verbose
    )
    report.completed.extend(r_completed)
    report.errored.extend(r_errored)
    report.stale.extend(r_stale)
    report.running.extend(r_running)

    # Check for orphaned issues
    if not json_output:
        log_info("Checking for orphaned issues...")

    report.orphaned = check_orphaned_building(daemon_state)

    for issue_num in report.orphaned:
        log_warning(f"Issue #{issue_num} is in loom:building but not tracked by any shepherd")

    # Recovery actions
    if recover:
        if not json_output:
            log_info("Performing recovery actions...")

        # Recover errored shepherds
        for task in report.errored:
            if task.category == "shepherd" and task.issue:
                if recover_issue(task.issue, dry_run):
                    report.recoveries.append(f"revert:{task.issue}")

        # Recover orphaned issues
        for issue_num in report.orphaned:
            if recover_issue(issue_num, dry_run):
                report.recoveries.append(f"revert_orphan:{issue_num}")

    return report


def format_json_output(report: CompletionReport) -> str:
    """Format the report as JSON."""
    completed_json = [f"{t.id}:{t.issue}:{t.task_id}" for t in report.completed]
    errored_json = [
        f"{t.id}:{t.issue}:{t.task_id}:{t.reason or 'unknown'}"
        for t in report.errored
    ]
    stale_json = [
        f"{t.id}:{t.issue}:{t.task_id}:{t.reason or 'unknown'}"
        for t in report.stale
    ]
    orphaned_json = [f"issue:{n}" for n in report.orphaned]
    running_json = [f"{t.id}:{t.issue}:{t.task_id}" for t in report.running]

    output = {
        "completed": completed_json,
        "errored": errored_json,
        "stale": stale_json,
        "orphaned": orphaned_json,
        "running": running_json,
        "recoveries": report.recoveries,
        "summary": {
            "completed_count": len(report.completed),
            "errored_count": len(report.errored),
            "stale_count": len(report.stale),
            "orphaned_count": len(report.orphaned),
            "running_count": len(report.running),
            "recovery_count": len(report.recoveries),
            "has_failures": report.has_failures,
        },
    }

    return json.dumps(output, indent=2)


def format_human_output(report: CompletionReport) -> str:
    """Format the report for human-readable output."""
    lines = [
        "",
        "=== Task Completion Summary ===",
        f"  Running:   {len(report.running)}",
        f"  Completed: {len(report.completed)}",
        f"  Errored:   {len(report.errored)}",
        f"  Stale:     {len(report.stale)}",
        f"  Orphaned:  {len(report.orphaned)}",
    ]

    if report.recoveries:
        lines.append(f"  Recovered: {len(report.recoveries)}")

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the check-completions CLI."""
    parser = argparse.ArgumentParser(
        description="Check task completions and detect silent failures",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Exit codes:
  0 - All tasks healthy (or recovered with --recover)
  1 - Silent failures detected
  2 - State file not found

Task states detected:
  completed        Task completed successfully
  errored          Task exited with error
  stale            No heartbeat for extended period
  orphaned         Issue in loom:building but no active task
  missing_output   Output file doesn't exist
  running          Task is still active

Environment variables:
  LOOM_HEARTBEAT_STALE_THRESHOLD   Seconds before heartbeat is stale (default: 300)
  LOOM_OUTPUT_STALE_THRESHOLD      Seconds before output is stale (default: 600)

Examples:
  loom-check-completions                     # Check all tasks
  loom-check-completions --json              # Machine-readable output
  loom-check-completions --recover --verbose # Recover and show details
""",
    )

    parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON for programmatic use",
    )
    parser.add_argument(
        "--recover",
        action="store_true",
        help="Auto-recover silently failed tasks (revert labels)",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Show detailed progress",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be recovered without making changes",
    )

    args = parser.parse_args(argv)

    report = run_check(
        verbose=args.verbose,
        recover=args.recover,
        dry_run=args.dry_run,
        json_output=args.json,
    )

    if args.json:
        print(format_json_output(report))
    else:
        print(format_human_output(report))

    # Exit code
    if report.has_failures:
        if args.recover and report.recoveries:
            return 0  # Issues detected but recovered
        return 1  # Silent failures detected

    return 0


if __name__ == "__main__":
    sys.exit(main())
