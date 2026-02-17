"""Orphaned shepherd detection and recovery for Loom daemon.

Detects and recovers orphaned shepherd state that occurs when:
- Daemon crashes mid-session leaving task_ids that no longer exist
- Issues have loom:building label but no active shepherd
- Progress files exist but the shepherd task is not running

This is distinct from stuck detection (stuck_detection.py):
- Stuck = running but struggling
- Orphan = not running at all

Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
from dataclasses import dataclass, field
from typing import Any

from loom_tools.claim import has_valid_claim
from loom_tools.common.github import gh_issue_list, gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import (
    read_daemon_state,
    read_json_file,
    read_progress_files,
    write_json_file,
)
from loom_tools.common.time_utils import elapsed_seconds, format_duration, now_utc
from loom_tools.models.daemon_state import DaemonState, ShepherdEntry
from loom_tools.models.progress import ShepherdProgress

# Default heartbeat stale threshold (5 minutes for orphan recovery)
# This is intentionally higher than stuck_detection's 120s because
# orphan recovery is post-crash cleanup, not real-time monitoring.
DEFAULT_HEARTBEAT_STALE_THRESHOLD = 300

# Task ID format: exactly 7 lowercase hex characters
TASK_ID_PATTERN = re.compile(r"^[a-f0-9]{7}$")


@dataclass
class OrphanEntry:
    """A detected orphan."""

    type: str  # stale_task_id, invalid_task_id, untracked_building, stale_heartbeat
    shepherd_id: str | None = None
    issue: int | None = None
    task_id: str | None = None
    title: str | None = None
    reason: str = ""
    age_seconds: int | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"type": self.type, "reason": self.reason}
        if self.shepherd_id is not None:
            d["shepherd_id"] = self.shepherd_id
        if self.issue is not None:
            d["issue"] = self.issue
        if self.task_id is not None:
            d["task_id"] = self.task_id
        if self.title is not None:
            d["title"] = self.title
        if self.age_seconds is not None:
            d["age_seconds"] = self.age_seconds
        return d


@dataclass
class RecoveryEntry:
    """A recovery action taken."""

    action: str  # reset_shepherd, reset_issue_label, cleanup_stale_worktree, mark_progress_errored
    shepherd_id: str | None = None
    issue: int | None = None
    task_id: str | None = None
    reason: str = ""

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"action": self.action, "reason": self.reason}
        if self.shepherd_id is not None:
            d["shepherd_id"] = self.shepherd_id
        if self.issue is not None:
            d["issue"] = self.issue
        if self.task_id is not None:
            d["task_id"] = self.task_id
        return d


@dataclass
class OrphanRecoveryResult:
    """Result of orphan detection and recovery."""

    orphaned: list[OrphanEntry] = field(default_factory=list)
    recovered: list[RecoveryEntry] = field(default_factory=list)
    recover_mode: bool = False

    @property
    def total_orphaned(self) -> int:
        return len(self.orphaned)

    @property
    def total_recovered(self) -> int:
        return len(self.recovered)

    def to_dict(self) -> dict[str, Any]:
        return {
            "orphaned": [o.to_dict() for o in self.orphaned],
            "recovered": [r.to_dict() for r in self.recovered],
            "total_orphaned": self.total_orphaned,
            "total_recovered": self.total_recovered,
            "recover_mode": self.recover_mode,
        }


def _get_heartbeat_stale_threshold() -> int:
    """Get heartbeat stale threshold from env var or default."""
    env_val = os.environ.get("LOOM_HEARTBEAT_STALE_THRESHOLD")
    if env_val is not None:
        try:
            return int(env_val)
        except ValueError:
            pass
    return DEFAULT_HEARTBEAT_STALE_THRESHOLD


def _is_valid_task_id(task_id: str) -> bool:
    """Check if a task ID matches the expected 7-char hex format."""
    return bool(TASK_ID_PATTERN.fullmatch(task_id))


def _check_task_exists(task_id: str, output_file: str | None) -> bool:
    """Check if a task likely exists by verifying its output file.

    Checks the recorded output file path, and also scans common
    task output locations under /tmp/claude.
    """
    if output_file and pathlib.Path(output_file).is_file():
        return True

    claude_task_dir = pathlib.Path("/tmp/claude")
    if claude_task_dir.is_dir():
        for p in claude_task_dir.rglob("*.output"):
            if task_id in p.name:
                return True

    return False


def check_daemon_state_tasks(
    daemon_state: DaemonState,
    result: OrphanRecoveryResult,
    *,
    verbose: bool = False,
) -> None:
    """Phase 1: Validate task_ids in daemon-state.json.

    Checks working shepherds for:
    - Invalid task ID format (not 7-char hex)
    - Stale task IDs (output file no longer exists)
    """
    for shepherd_id, entry in daemon_state.shepherds.items():
        if entry.status != "working":
            continue

        task_id = entry.task_id
        if task_id is None:
            continue

        if verbose:
            log_info(
                f"Checking {shepherd_id}: task_id={task_id}, "
                f"issue=#{entry.issue}, status={entry.status}"
            )

        if not _is_valid_task_id(task_id):
            if verbose:
                log_warning(
                    f"  ORPHANED: {shepherd_id} has invalid task_id "
                    f"format '{task_id}' (expected 7 hex chars)"
                )
            result.orphaned.append(
                OrphanEntry(
                    type="invalid_task_id",
                    shepherd_id=shepherd_id,
                    task_id=task_id,
                    issue=entry.issue,
                    reason="invalid_task_id_format",
                )
            )
            continue

        if not _check_task_exists(task_id, entry.output_file):
            if verbose:
                log_warning(
                    f"  ORPHANED: {shepherd_id} has stale task_id {task_id}"
                )
            result.orphaned.append(
                OrphanEntry(
                    type="stale_task_id",
                    shepherd_id=shepherd_id,
                    task_id=task_id,
                    issue=entry.issue,
                    reason="task_not_found",
                )
            )
        elif verbose:
            log_info(f"  OK: task exists for {shepherd_id}")


def check_untracked_building(
    daemon_state: DaemonState,
    progress_files: list[ShepherdProgress],
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
    heartbeat_threshold: int = DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    verbose: bool = False,
) -> None:
    """Phase 2: Find loom:building issues without active shepherds.

    Cross-references loom:building issues with daemon-state tracked issues
    and checks progress files for fresh heartbeats.  Issues with a valid
    file-based claim are skipped even if no daemon entry or fresh heartbeat
    exists, because a CLI shepherd may be legitimately working on them.
    """
    try:
        building_issues = gh_issue_list(labels=["loom:building"])
    except Exception as exc:
        log_error(f"Failed to list loom:building issues: {exc}")
        return

    if not building_issues:
        if verbose:
            log_info("No loom:building issues found")
        return

    tracked_issues: set[int] = set()
    for entry in daemon_state.shepherds.values():
        if entry.status == "working" and entry.issue is not None:
            tracked_issues.add(entry.issue)

    for issue_data in building_issues:
        issue_num = issue_data.get("number", 0)
        issue_title = issue_data.get("title", "")

        if verbose:
            log_info(f"Checking issue #{issue_num}")

        if issue_num in tracked_issues:
            if verbose:
                log_info(f"  OK: tracked in daemon-state")
            continue

        # Not tracked in daemon-state; check progress files
        has_fresh_progress = False
        for progress in progress_files:
            if progress.issue != issue_num:
                continue
            if progress.status != "working":
                continue

            hb = progress.last_heartbeat
            if not hb:
                if verbose:
                    log_info(
                        f"  Progress file for issue #{issue_num} "
                        "has no heartbeat -- not trusted"
                    )
                continue

            try:
                age = elapsed_seconds(hb)
            except (ValueError, OverflowError):
                if verbose:
                    log_info(
                        f"  Progress file for issue #{issue_num} "
                        "has unparseable heartbeat -- not trusted"
                    )
                continue

            if age > heartbeat_threshold:
                if verbose:
                    log_info(
                        f"  Progress file for issue #{issue_num} "
                        f"has stale heartbeat ({age}s old) -- not trusted"
                    )
                continue

            has_fresh_progress = True
            if verbose:
                log_info(
                    f"  Found active progress file for issue #{issue_num} "
                    f"(heartbeat {age}s old)"
                )
            break

        if not has_fresh_progress:
            # Check file-based claim before flagging as orphaned.
            # A CLI shepherd may hold a valid claim without a daemon entry
            # or fresh progress heartbeat (e.g., during a long builder subprocess).
            if repo_root is not None and has_valid_claim(repo_root, issue_num):
                if verbose:
                    log_info(
                        f"  SKIPPED: #{issue_num} has a valid file-based claim"
                    )
                continue

            if verbose:
                log_warning(
                    f"  ORPHANED: #{issue_num} has loom:building "
                    "but no active shepherd"
                )
            result.orphaned.append(
                OrphanEntry(
                    type="untracked_building",
                    issue=issue_num,
                    title=issue_title,
                    reason="no_daemon_entry",
                )
            )


def check_stale_progress(
    progress_files: list[ShepherdProgress],
    result: OrphanRecoveryResult,
    *,
    heartbeat_threshold: int = DEFAULT_HEARTBEAT_STALE_THRESHOLD,
    verbose: bool = False,
) -> None:
    """Phase 3: Check progress files for stale heartbeats.

    Iterates progress files and flags those with stale heartbeats
    as orphaned.
    """
    for progress in progress_files:
        if verbose:
            log_info(
                f"Checking progress: task={progress.task_id}, "
                f"issue=#{progress.issue}, status={progress.status}"
            )

        if progress.status != "working":
            if verbose:
                log_info(f"  Skipping (status: {progress.status})")
            continue

        hb = progress.last_heartbeat
        if not hb:
            continue

        try:
            age = elapsed_seconds(hb)
        except (ValueError, OverflowError):
            continue

        if verbose:
            threshold_mins = heartbeat_threshold // 60
            log_info(
                f"  Heartbeat age: {age // 60}m "
                f"(threshold: {threshold_mins}m)"
            )

        if age > heartbeat_threshold:
            if verbose:
                log_warning(
                    f"  ORPHANED: task {progress.task_id} "
                    f"has stale heartbeat ({age // 60}m old)"
                )
            result.orphaned.append(
                OrphanEntry(
                    type="stale_heartbeat",
                    task_id=progress.task_id,
                    issue=progress.issue if progress.issue else None,
                    age_seconds=age,
                    reason="heartbeat_stale",
                )
            )


def recover_shepherd(
    repo_root: pathlib.Path,
    shepherd_id: str,
    issue: int | None,
    task_id: str | None,
    reason: str,
    result: OrphanRecoveryResult,
) -> None:
    """Recovery action: Reset shepherd entry in daemon-state to idle."""
    daemon_state_path = repo_root / ".loom" / "daemon-state.json"
    data = read_json_file(daemon_state_path)
    if not isinstance(data, dict):
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")

    shepherds = data.get("shepherds", {})
    old_entry = shepherds.get(shepherd_id, {})

    shepherds[shepherd_id] = {
        "status": "idle",
        "idle_since": ts,
        "idle_reason": "orphan_recovery",
        "last_issue": old_entry.get("issue"),
        "last_completed": ts,
    }
    data["shepherds"] = shepherds
    write_json_file(daemon_state_path, data)

    log_info(f"Reset shepherd {shepherd_id} to idle in daemon-state")

    if issue is not None and issue != 0:
        recover_issue(issue, reason, result, repo_root=repo_root)

    result.recovered.append(
        RecoveryEntry(
            action="reset_shepherd",
            shepherd_id=shepherd_id,
            issue=issue,
            task_id=task_id,
            reason=reason,
        )
    )


def _cleanup_stale_worktree(repo_root: pathlib.Path, issue: int) -> bool:
    """Remove a stale worktree and its local/remote branches for an issue.

    A worktree is considered stale when it has zero commits ahead of main
    and no meaningful uncommitted changes (build artifacts are ignored).

    Returns True if cleanup was performed, False otherwise.
    """
    worktree_path = repo_root / ".loom" / "worktrees" / f"issue-{issue}"
    if not worktree_path.is_dir():
        return False

    # Check for commits ahead of main
    log_result = subprocess.run(
        ["git", "-C", str(worktree_path), "log", "--oneline", "origin/main..HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    if log_result.returncode != 0:
        log_warning(
            f"Cannot determine commit status for worktree issue-{issue}, "
            "skipping cleanup"
        )
        return False

    if log_result.stdout.strip():
        log_info(
            f"Worktree issue-{issue} has commits ahead of main, skipping cleanup"
        )
        return False

    # Check for meaningful uncommitted changes (ignore build artifacts)
    status_result = subprocess.run(
        ["git", "-C", str(worktree_path), "status", "--porcelain"],
        capture_output=True,
        text=True,
        check=False,
    )
    if status_result.returncode != 0:
        log_warning(
            f"Cannot determine status for worktree issue-{issue}, skipping cleanup"
        )
        return False

    build_artifact_patterns = (
        "node_modules",
        "pnpm-lock.yaml",
        ".venv",
        "target/",
        "Cargo.lock",
        "coverage/",
        ".loom-checkpoint",
        ".loom-in-use",
    )
    for line in status_result.stdout.strip().splitlines():
        filepath = line[3:].strip().strip('"')
        if not any(pat in filepath for pat in build_artifact_patterns):
            log_info(
                f"Worktree issue-{issue} has meaningful uncommitted changes, "
                "skipping cleanup"
            )
            return False

    # Get branch name before removal
    branch_result = subprocess.run(
        ["git", "-C", str(worktree_path), "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    branch = branch_result.stdout.strip() if branch_result.returncode == 0 else ""

    # Remove worktree
    remove_result = subprocess.run(
        ["git", "worktree", "remove", str(worktree_path), "--force"],
        cwd=str(repo_root),
        capture_output=True,
        text=True,
        check=False,
    )
    if remove_result.returncode != 0:
        log_warning(
            f"Failed to remove worktree issue-{issue}: "
            f"{remove_result.stderr.strip()}"
        )
        return False

    # Delete local branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "branch", "-D", branch],
            capture_output=True,
            check=False,
        )

    # Delete remote branch (best-effort)
    if branch and branch != "main":
        subprocess.run(
            ["git", "-C", str(repo_root), "push", "origin", "--delete", branch],
            capture_output=True,
            check=False,
        )

    log_info(
        f"Cleaned up stale worktree issue-{issue}"
        + (f" (branch {branch})" if branch else "")
    )
    return True


def recover_issue(
    issue: int,
    reason: str,
    result: OrphanRecoveryResult,
    *,
    repo_root: pathlib.Path | None = None,
) -> None:
    """Recovery action: Reset issue labels from loom:building to loom:issue.

    If ``repo_root`` is provided and a valid file-based claim exists for the
    issue, recovery is skipped to avoid disrupting an active shepherd.
    """
    if repo_root is not None and has_valid_claim(repo_root, issue):
        log_warning(
            f"Skipping recovery for issue #{issue}: valid file-based claim exists"
        )
        return

    # Clean up stale worktree if present (0 commits ahead, no meaningful changes)
    worktree_cleaned = False
    if repo_root is not None:
        worktree_cleaned = _cleanup_stale_worktree(repo_root, issue)
        if worktree_cleaned:
            result.recovered.append(
                RecoveryEntry(
                    action="cleanup_stale_worktree",
                    issue=issue,
                    reason=reason,
                )
            )


    try:
        gh_run([
            "issue", "edit", str(issue),
            "--remove-label", "loom:building",
            "--add-label", "loom:issue",
        ])
    except Exception as exc:
        log_warning(f"Failed to update labels for issue #{issue}: {exc}")
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    actions = [
        "- Removed `loom:building` label",
        "- Added `loom:issue` label to return to ready queue",
    ]
    if worktree_cleaned:
        actions.append("- Cleaned up stale worktree and branches")

    comment = (
        "## Orphan Recovery\n\n"
        "This issue was automatically recovered from an orphaned state.\n\n"
        f"**Reason**: {reason}\n"
        "**What happened**:\n"
        "- The daemon or shepherd that was working on this issue "
        "crashed or was terminated\n"
        "- The issue was left in `loom:building` state with no active worker\n\n"
        "**Action taken**:\n"
        + "\n".join(actions)
        + "\n\n"
        "This issue is now available for a new shepherd to pick up.\n\n"
        "---\n"
        f"*Recovered by loom-recover-orphans at {ts}*"
    )

    try:
        gh_run(["issue", "comment", str(issue), "--body", comment])
    except Exception as exc:
        log_warning(f"Failed to add comment to issue #{issue}: {exc}")

    result.recovered.append(
        RecoveryEntry(
            action="reset_issue_label",
            issue=issue,
            reason=reason,
        )
    )

    log_success(f"Recovered issue #{issue}")


def recover_progress_file(
    repo_root: pathlib.Path,
    progress: ShepherdProgress,
    result: OrphanRecoveryResult,
) -> None:
    """Recovery action: Mark progress file status as errored."""
    progress_dir = repo_root / ".loom" / "progress"
    progress_path = progress_dir / f"shepherd-{progress.task_id}.json"

    if not progress_path.is_file():
        return

    data = read_json_file(progress_path)
    if not isinstance(data, dict):
        return

    ts = now_utc().strftime("%Y-%m-%dT%H:%M:%SZ")
    data["status"] = "errored"
    data["last_heartbeat"] = ts

    milestones = data.get("milestones", [])
    milestones.append({
        "event": "error",
        "timestamp": ts,
        "data": {"error": "orphan_recovery", "will_retry": False},
    })
    data["milestones"] = milestones

    write_json_file(progress_path, data)
    log_info(f"Marked progress file for task {progress.task_id} as errored")

    issue = progress.issue if progress.issue else None
    if issue is not None and issue != 0:
        recover_issue(issue, "stale_heartbeat", result, repo_root=repo_root)

    result.recovered.append(
        RecoveryEntry(
            action="mark_progress_errored",
            task_id=progress.task_id,
            issue=issue,
            reason="stale_heartbeat",
        )
    )


def run_orphan_recovery(
    repo_root: pathlib.Path,
    *,
    recover: bool = False,
    verbose: bool = False,
) -> OrphanRecoveryResult:
    """Run all orphan detection phases and optionally recover.

    Returns an OrphanRecoveryResult with all detected orphans
    and any recovery actions taken.
    """
    result = OrphanRecoveryResult(recover_mode=recover)
    heartbeat_threshold = _get_heartbeat_stale_threshold()

    daemon_state = read_daemon_state(repo_root)
    progress_files = read_progress_files(repo_root)

    # Phase 1: Check daemon-state task IDs
    check_daemon_state_tasks(daemon_state, result, verbose=verbose)

    # Phase 2: Check untracked loom:building issues
    check_untracked_building(
        daemon_state,
        progress_files,
        result,
        repo_root=repo_root,
        heartbeat_threshold=heartbeat_threshold,
        verbose=verbose,
    )

    # Phase 3: Check stale progress files
    check_stale_progress(
        progress_files,
        result,
        heartbeat_threshold=heartbeat_threshold,
        verbose=verbose,
    )

    if not recover:
        return result

    # Perform recovery for detected orphans
    for orphan in list(result.orphaned):
        if orphan.type in ("stale_task_id", "invalid_task_id"):
            if orphan.shepherd_id:
                recover_shepherd(
                    repo_root,
                    orphan.shepherd_id,
                    orphan.issue,
                    orphan.task_id,
                    orphan.reason,
                    result,
                )
        elif orphan.type == "untracked_building":
            if orphan.issue:
                recover_issue(orphan.issue, orphan.reason, result, repo_root=repo_root)
        elif orphan.type == "stale_heartbeat":
            # Find the matching progress file
            for progress in progress_files:
                if progress.task_id == orphan.task_id:
                    recover_progress_file(repo_root, progress, result)
                    break

    return result


def format_result_json(result: OrphanRecoveryResult) -> str:
    """Format result as JSON string."""
    return json.dumps(result.to_dict(), indent=2)


def format_result_human(result: OrphanRecoveryResult) -> str:
    """Format result as human-readable text."""
    lines: list[str] = []

    if result.total_orphaned == 0:
        lines.append("No orphaned shepherds found")
    else:
        lines.append(f"Found {result.total_orphaned} orphaned shepherd(s)")
        lines.append("")

        for orphan in result.orphaned:
            if orphan.type == "invalid_task_id":
                lines.append(
                    f"  [{orphan.type}] {orphan.shepherd_id}: "
                    f"invalid task_id '{orphan.task_id}' "
                    f"(issue #{orphan.issue})"
                )
            elif orphan.type == "stale_task_id":
                lines.append(
                    f"  [{orphan.type}] {orphan.shepherd_id}: "
                    f"task {orphan.task_id} not found "
                    f"(issue #{orphan.issue})"
                )
            elif orphan.type == "untracked_building":
                lines.append(
                    f"  [{orphan.type}] #{orphan.issue}: "
                    f"{orphan.title or 'no title'} "
                    f"-- no active shepherd"
                )
            elif orphan.type == "stale_heartbeat":
                age_str = format_duration(orphan.age_seconds or 0)
                lines.append(
                    f"  [{orphan.type}] task {orphan.task_id}: "
                    f"heartbeat stale ({age_str}) "
                    f"(issue #{orphan.issue})"
                )

        if result.recover_mode:
            lines.append("")
            lines.append(f"Recovered {result.total_recovered} item(s)")
        else:
            lines.append("")
            lines.append("Run with --recover to fix these issues")

    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """Main entry point for orphan recovery CLI."""
    parser = argparse.ArgumentParser(
        description="Detect and recover orphaned shepherd state",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""\
Exit codes:
    0 - No orphans detected
    1 - Error occurred
    2 - Orphans detected

Orphan types:
    stale_task_id       - Shepherd has task_id but task output file is gone
    invalid_task_id     - Shepherd has malformed task_id (not 7-char hex)
    untracked_building  - Issue has loom:building but no active shepherd
    stale_heartbeat     - Progress file heartbeat is stale

Recovery actions:
    reset_shepherd          - Reset shepherd entry to idle in daemon-state
    reset_issue_label       - Swap loom:building -> loom:issue on issue
    cleanup_stale_worktree  - Remove stale worktree + branches (0 commits, no changes)
    mark_progress_errored   - Mark progress file status as errored

Environment variables:
    LOOM_HEARTBEAT_STALE_THRESHOLD  Seconds before heartbeat is stale (default: 300)
""",
    )

    parser.add_argument(
        "--recover",
        action="store_true",
        help="Actually perform recovery (default is dry-run)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output JSON for programmatic use",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Show detailed progress",
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    if not args.json:
        log_info("Orphaned Shepherd Detection & Recovery")
        if not args.recover:
            log_info("DRY RUN - No changes will be made")
            log_info("Use --recover to actually perform recovery")

    try:
        result = run_orphan_recovery(
            repo_root,
            recover=args.recover,
            verbose=args.verbose,
        )
    except Exception as exc:
        log_error(f"Error during orphan recovery: {exc}")
        return 1

    if args.json:
        print(format_result_json(result))
    else:
        print(format_result_human(result))

    if result.total_orphaned > 0 and not args.recover:
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
