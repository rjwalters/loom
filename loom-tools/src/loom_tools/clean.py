"""Loom unified cleanup - restore repository to clean state.

Consolidates functionality formerly in:
    - clean.sh (removed in #1745)
    - cleanup.sh (removed in #1745)
    - safe-worktree-cleanup.sh (removed in #1745)

Exit codes:
    0 - Success
    1 - Errors during cleanup
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone

from loom_tools.common.claude_config import cleanup_all_agent_config_dirs
from loom_tools.common.github import gh_list, gh_run
from loom_tools.common.logging import log_error, log_info, log_success, log_warning
from loom_tools.common.paths import LoomPaths, NamingConventions
from loom_tools.common.repo import find_repo_root
from loom_tools.common.state import read_json_file, write_json_file
from loom_tools.common.time_utils import parse_iso_timestamp
from loom_tools.common.worktree_safety import find_processes_using_directory

# Default grace period: 10 minutes in seconds
DEFAULT_GRACE_PERIOD = 600


@dataclass
class CleanupStats:
    """Statistics from cleanup operation."""

    cleaned_worktrees: int = 0
    skipped_open: int = 0
    skipped_in_use: int = 0
    skipped_not_merged: int = 0
    skipped_grace: int = 0
    skipped_uncommitted: int = 0
    skipped_editable: int = 0
    cleaned_branches: int = 0
    kept_branches: int = 0
    killed_tmux: int = 0
    cleaned_config_dirs: int = 0
    errors: int = 0


@dataclass
class PRStatus:
    """Status of a PR associated with an issue."""

    status: str  # "MERGED", "CLOSED_NO_MERGE", "OPEN", "NO_PR", "UNKNOWN"
    merged_at: str | None = None


def check_pr_merged(issue_num: int) -> PRStatus:
    """Check if the PR for an issue has been merged.

    Returns:
        PRStatus with status and optional merged_at timestamp.
    """
    branch_name = NamingConventions.branch_name(issue_num)

    try:
        # Find PR by head branch
        prs = gh_list(
            "pr",
            head=branch_name,
            state="all",
            fields=["number", "state", "mergedAt"],
            limit=1,
        )

        if not prs:
            # Try searching by issue reference
            prs = gh_list(
                "pr",
                search=f"Closes #{issue_num}",
                state="all",
                fields=["number", "state", "mergedAt"],
                limit=1,
            )

        if not prs:
            return PRStatus(status="NO_PR")

        data = prs[0]
        state = data.get("state", "UNKNOWN")
        merged_at = data.get("mergedAt")

        if merged_at:
            return PRStatus(status="MERGED", merged_at=merged_at)
        elif state == "CLOSED":
            return PRStatus(status="CLOSED_NO_MERGE")
        elif state == "OPEN":
            return PRStatus(status="OPEN")
        else:
            return PRStatus(status="UNKNOWN")

    except Exception:
        return PRStatus(status="UNKNOWN")


def check_uncommitted_changes(worktree_path: pathlib.Path) -> bool:
    """Check if a worktree has uncommitted changes.

    Returns:
        True if there are uncommitted changes, False otherwise.
    """
    if not worktree_path.is_dir():
        return False

    try:
        # Check for any uncommitted changes (staged or unstaged)
        result1 = subprocess.run(
            ["git", "-C", str(worktree_path), "diff", "--quiet"],
            capture_output=True,
            text=True,
        )
        result2 = subprocess.run(
            ["git", "-C", str(worktree_path), "diff", "--cached", "--quiet"],
            capture_output=True,
            text=True,
        )

        # If either returns non-zero, there are changes
        return result1.returncode != 0 or result2.returncode != 0
    except Exception:
        return False


def check_grace_period(merged_at: str, grace_period: int) -> tuple[bool, int]:
    """Check if grace period has passed since PR merge.

    Returns:
        Tuple of (passed, remaining_seconds).
    """
    now = datetime.now(timezone.utc)

    try:
        merged_ts = parse_iso_timestamp(merged_at)
        elapsed = (now - merged_ts).total_seconds()

        if elapsed > grace_period:
            return True, 0
        else:
            return False, int(grace_period - elapsed)
    except Exception:
        # If we can't parse, assume grace period passed
        return True, 0


def update_cleanup_state(
    repo_root: pathlib.Path,
    issue_num: int,
    status: str,
) -> None:
    """Update cleanup state in daemon-state.json.

    Args:
        repo_root: Repository root path.
        issue_num: Issue number.
        status: One of "cleaned", "pending", "error".
    """
    paths = LoomPaths(repo_root)

    if not paths.daemon_state_file.exists():
        return

    data = read_json_file(paths.daemon_state_file)
    if not isinstance(data, dict):
        return

    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    worktree_name = NamingConventions.worktree_name(issue_num)

    # Initialize cleanup section if needed
    if "cleanup" not in data:
        data["cleanup"] = {
            "lastRun": None,
            "lastCleaned": [],
            "pendingCleanup": [],
            "errors": [],
        }

    cleanup = data["cleanup"]

    if status == "cleaned":
        cleanup["lastCleaned"] = cleanup.get("lastCleaned", []) + [worktree_name]
        cleanup["lastRun"] = timestamp
    elif status == "pending":
        pending = cleanup.get("pendingCleanup", [])
        if worktree_name not in pending:
            pending.append(worktree_name)
        cleanup["pendingCleanup"] = pending
    elif status == "error":
        errors = cleanup.get("errors", [])
        errors.append({"issue": issue_num, "timestamp": timestamp})
        cleanup["errors"] = errors

    write_json_file(paths.daemon_state_file, data)


def cleanup_worktree(
    repo_root: pathlib.Path,
    worktree_path: pathlib.Path,
    issue_num: int,
    dry_run: bool = False,
) -> bool:
    """Remove a worktree and its associated branch.

    Returns:
        True if cleanup succeeded, False otherwise.
    """
    branch_name = NamingConventions.branch_name(issue_num)

    if dry_run:
        log_info(f"Would remove: {worktree_path}")
        log_info(f"Would delete branch: {branch_name}")
        return True

    # Remove worktree
    try:
        result = subprocess.run(
            ["git", "worktree", "remove", str(worktree_path), "--force"],
            capture_output=True,
            text=True,
            cwd=repo_root,
        )
        if result.returncode == 0:
            log_success(f"Removed worktree: {worktree_path}")
        else:
            log_warning(f"Failed to remove worktree: {worktree_path}")
            return False
    except Exception:
        log_warning(f"Failed to remove worktree: {worktree_path}")
        return False

    # Delete local branch (if it exists)
    try:
        result = subprocess.run(
            ["git", "branch", "-d", branch_name],
            capture_output=True,
            text=True,
            cwd=repo_root,
        )
        if result.returncode == 0:
            log_success(f"Deleted branch: {branch_name}")
        else:
            # Try force delete
            result = subprocess.run(
                ["git", "branch", "-D", branch_name],
                capture_output=True,
                text=True,
                cwd=repo_root,
            )
            if result.returncode == 0:
                log_success(f"Force-deleted branch: {branch_name}")
            else:
                log_info(f"Branch already deleted or doesn't exist: {branch_name}")
    except Exception:
        log_info(f"Branch already deleted or doesn't exist: {branch_name}")

    return True


def find_editable_pip_installs(worktree_path: pathlib.Path) -> list[str]:
    """Find pip packages with editable installs pointing into a worktree.

    Checks all available Python interpreters for packages installed in
    editable mode (pip install -e) whose source location is inside the
    given worktree path.

    Args:
        worktree_path: Absolute path to the worktree directory.

    Returns:
        List of package names with editable installs inside the worktree.
    """
    packages: list[str] = []
    worktree_str = str(worktree_path.resolve())

    # Find Python interpreters to check
    interpreters: list[str] = []
    for name in ("python3", "python"):
        try:
            result = subprocess.run(
                ["which", name],
                capture_output=True,
                text=True,
            )
            if result.returncode == 0:
                path = result.stdout.strip()
                if path and path not in interpreters:
                    interpreters.append(path)
        except Exception:
            continue

    # Also check for venvs inside the worktree itself
    for venv_dir in worktree_path.glob("*/.venv/bin/python"):
        venv_python = str(venv_dir)
        if venv_python not in interpreters:
            interpreters.append(venv_python)
    for venv_dir in worktree_path.glob(".venv/bin/python"):
        venv_python = str(venv_dir)
        if venv_python not in interpreters:
            interpreters.append(venv_python)

    for interpreter in interpreters:
        try:
            # List all installed packages
            result = subprocess.run(
                [interpreter, "-m", "pip", "list", "--editable", "--format=json"],
                capture_output=True,
                text=True,
                timeout=15,
            )
            if result.returncode != 0:
                continue

            try:
                editable_pkgs = json.loads(result.stdout)
            except (json.JSONDecodeError, ValueError):
                continue

            for pkg_info in editable_pkgs:
                pkg_name = pkg_info.get("name", "")
                if not pkg_name:
                    continue

                # Get detailed info to find the editable location
                show_result = subprocess.run(
                    [interpreter, "-m", "pip", "show", pkg_name],
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                if show_result.returncode != 0:
                    continue

                for line in show_result.stdout.splitlines():
                    if line.startswith("Editable project location:") or line.startswith("Location:"):
                        location = line.split(":", 1)[1].strip()
                        if location.startswith(worktree_str):
                            if pkg_name not in packages:
                                packages.append(pkg_name)
                            break

        except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
            continue

    return packages


def clean_worktrees(
    repo_root: pathlib.Path,
    stats: CleanupStats,
    dry_run: bool = False,
    force: bool = False,
    safe_mode: bool = False,
    grace_period: int = DEFAULT_GRACE_PERIOD,
) -> None:
    """Clean up worktrees for closed issues."""
    paths = LoomPaths(repo_root)

    if not paths.worktrees_dir.is_dir():
        log_info("No worktrees directory found")
        return

    for worktree_dir in sorted(paths.worktrees_dir.glob(f"{NamingConventions.WORKTREE_PREFIX}*")):
        if not worktree_dir.is_dir():
            continue

        # Extract issue number
        issue_num = NamingConventions.issue_from_worktree(worktree_dir.name)
        if issue_num is None:
            continue
        worktree_path = worktree_dir.resolve()

        print(f"Checking worktree: issue-{issue_num}")

        # Check for in-use marker
        marker_file = worktree_path / ".loom-in-use"
        if marker_file.exists():
            marker_data = read_json_file(marker_file)
            if isinstance(marker_data, dict):
                task_id = marker_data.get("shepherd_task_id", "unknown")
                pid = marker_data.get("pid", "unknown")
                log_info(f"  Worktree in use by shepherd (task: {task_id}, pid: {pid}) - preserving")
            else:
                log_info("  Worktree in use - preserving")
            stats.skipped_in_use += 1
            continue

        # Check for active processes using the worktree (unless force mode)
        if not force:
            active_pids = find_processes_using_directory(worktree_path)
            if active_pids:
                log_info(f"  Active process(es) using worktree: {active_pids} - preserving")
                stats.skipped_in_use += 1
                continue

        # Check for editable pip installs pointing into the worktree
        editable_pkgs = find_editable_pip_installs(worktree_path)
        if editable_pkgs:
            pkg_list = ", ".join(editable_pkgs)
            if force:
                log_warning(f"  Editable pip install(s) found ({pkg_list}) - removing anyway (--force)")
            else:
                log_warning(f"  Editable pip install(s) found ({pkg_list}) - skipping")
                log_info("  Use --force to remove anyway, or uninstall first: pip uninstall " + " ".join(editable_pkgs))
                stats.skipped_editable += 1
                continue

        # Check issue state via GitHub CLI
        try:
            result = gh_run(
                ["issue", "view", str(issue_num), "--json", "state", "--jq", ".state"],
                check=False,
            )
            issue_state = result.stdout.strip() if result.returncode == 0 else "UNKNOWN"
        except Exception:
            issue_state = "UNKNOWN"

        if issue_state != "CLOSED":
            log_info(f"  Issue #{issue_num} is {issue_state} - preserving")
            stats.skipped_open += 1
            continue

        # Safe mode: additional checks
        if safe_mode:
            pr_status = check_pr_merged(issue_num)

            if pr_status.status == "MERGED" and pr_status.merged_at:
                # Check grace period (unless --force)
                if not force:
                    passed, remaining = check_grace_period(pr_status.merged_at, grace_period)
                    if not passed:
                        log_info(f"  PR merged but grace period not passed ({remaining}s remaining)")
                        update_cleanup_state(repo_root, issue_num, "pending")
                        stats.skipped_grace += 1
                        continue

                # Check for uncommitted changes (unless --force)
                if not force:
                    if check_uncommitted_changes(worktree_path):
                        log_warning("  Uncommitted changes detected - skipping")
                        stats.skipped_uncommitted += 1
                        continue

            elif pr_status.status == "CLOSED_NO_MERGE":
                log_warning("  PR closed without merge - skipping (may need investigation)")
                stats.skipped_not_merged += 1
                continue
            elif pr_status.status == "OPEN":
                log_info("  PR still open - skipping")
                stats.skipped_open += 1
                continue
            elif pr_status.status == "NO_PR":
                log_warning("  No PR found for closed issue - skipping")
                stats.skipped_not_merged += 1
                continue
            else:
                log_warning("  Unknown PR status - skipping")
                stats.errors += 1
                continue

            # All checks passed - cleanup with state tracking
            if cleanup_worktree(repo_root, worktree_path, issue_num, dry_run):
                if not dry_run:
                    update_cleanup_state(repo_root, issue_num, "cleaned")
                stats.cleaned_worktrees += 1
            else:
                if not dry_run:
                    update_cleanup_state(repo_root, issue_num, "error")
                stats.errors += 1

        else:
            # Standard mode: just check if issue is closed
            log_warning(f"  Issue #{issue_num} is CLOSED")

            if dry_run:
                log_info(f"  Would remove: {worktree_dir}")
                stats.cleaned_worktrees += 1
            elif force:
                log_info(f"  Auto-removing: {worktree_dir}")
                if cleanup_worktree(repo_root, worktree_path, issue_num, dry_run):
                    stats.cleaned_worktrees += 1
                else:
                    stats.errors += 1
            else:
                # Interactive mode - prompt
                response = input("  Force remove this worktree? [y/N] ").strip().lower()
                if response in ("y", "yes"):
                    if cleanup_worktree(repo_root, worktree_path, issue_num, dry_run):
                        stats.cleaned_worktrees += 1
                    else:
                        stats.errors += 1
                else:
                    log_info(f"  Skipping: {worktree_dir}")
                    stats.skipped_open += 1


def prune_orphaned_worktrees(
    repo_root: pathlib.Path,
    dry_run: bool = False,
) -> None:
    """Prune orphaned worktree references."""
    print("\nPruning Orphaned References")

    try:
        args = ["git", "worktree", "prune"]
        if dry_run:
            args.append("--dry-run")
        args.append("--verbose")

        result = subprocess.run(
            args,
            capture_output=True,
            text=True,
            cwd=repo_root,
        )

        if result.stdout.strip():
            print(result.stdout)
        else:
            log_success("No orphaned worktrees to prune")
    except Exception as e:
        log_warning(f"Error pruning worktrees: {e}")


def clean_branches(
    repo_root: pathlib.Path,
    stats: CleanupStats,
    dry_run: bool = False,
    force: bool = False,
) -> None:
    """Clean up branches for closed issues."""
    try:
        result = subprocess.run(
            ["git", "branch"],
            capture_output=True,
            text=True,
            cwd=repo_root,
        )
        branches = result.stdout.strip().split("\n") if result.returncode == 0 else []
    except Exception:
        branches = []

    feature_branches = []
    for branch in branches:
        branch = branch.strip().lstrip("* ")
        if branch.startswith(NamingConventions.BRANCH_PREFIX):
            feature_branches.append(branch)

    if not feature_branches:
        log_success("No feature branches found")
        return

    for branch in feature_branches:
        # Extract issue number
        issue_num = NamingConventions.issue_from_branch(branch)
        if issue_num is None:
            continue

        # Check issue status
        try:
            result = gh_run(
                ["issue", "view", str(issue_num), "--json", "state", "--jq", ".state"],
                check=False,
            )
            status = result.stdout.strip() if result.returncode == 0 else "NOT_FOUND"
        except Exception:
            status = "NOT_FOUND"

        if status == "CLOSED":
            print(f"  Issue #{issue_num} CLOSED - deleting {branch}")
            if not dry_run:
                try:
                    subprocess.run(
                        ["git", "branch", "-D", branch],
                        capture_output=True,
                        text=True,
                        cwd=repo_root,
                        check=True,
                    )
                    stats.cleaned_branches += 1
                except Exception:
                    stats.errors += 1
            else:
                stats.cleaned_branches += 1
        elif status == "OPEN":
            print(f"  Issue #{issue_num} OPEN - keeping {branch}")
            stats.kept_branches += 1


def clean_tmux_sessions(
    stats: CleanupStats,
    dry_run: bool = False,
) -> None:
    """Clean up Loom tmux sessions."""
    try:
        result = subprocess.run(
            ["tmux", "list-sessions"],
            capture_output=True,
            text=True,
        )

        if result.returncode != 0:
            log_success("No Loom tmux sessions found")
            return

        sessions = []
        for line in result.stdout.strip().split("\n"):
            if line.startswith("loom-"):
                session = line.split(":")[0]
                sessions.append(session)

        if not sessions:
            log_success("No Loom tmux sessions found")
            return

        print("Found Loom tmux sessions:")
        for session in sessions:
            print(f"  - {session}")
        print()

        if dry_run:
            log_info("Would kill these sessions")
            stats.killed_tmux = len(sessions)
        else:
            for session in sessions:
                try:
                    subprocess.run(
                        ["tmux", "kill-session", "-t", session],
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    log_success(f"Killed: {session}")
                    stats.killed_tmux += 1
                except Exception:
                    pass

    except FileNotFoundError:
        # tmux not installed
        log_success("No Loom tmux sessions found (tmux not available)")
    except Exception:
        log_success("No Loom tmux sessions found")


def clean_agent_config(
    repo_root: pathlib.Path,
    stats: CleanupStats,
    dry_run: bool = False,
) -> None:
    """Clean up per-agent Claude config directories."""
    paths = LoomPaths(repo_root)
    base_dir = paths.claude_config_base_dir

    if not base_dir.is_dir():
        log_success("No agent config directories found")
        return

    count = sum(1 for child in base_dir.iterdir() if child.is_dir())
    if count == 0:
        log_success("No agent config directories found")
        return

    if dry_run:
        log_info(f"Would remove {count} agent config dir(s) from {base_dir}")
    else:
        removed = cleanup_all_agent_config_dirs(repo_root)
        log_success(f"Removed {removed} agent config dir(s)")
        count = removed

    stats.cleaned_config_dirs = count


def clean_build_artifacts(
    repo_root: pathlib.Path,
    dry_run: bool = False,
) -> None:
    """Clean up build artifacts (target/, node_modules/)."""
    # Remove target/
    target_dir = repo_root / "target"
    if target_dir.is_dir():
        try:
            size = _get_dir_size(target_dir)
            if dry_run:
                log_info(f"Would remove target/ ({size})")
            else:
                shutil.rmtree(target_dir)
                log_success(f"Removed target/ ({size})")
        except Exception as e:
            log_warning(f"Failed to remove target/: {e}")
    else:
        log_info("No target/ directory found")

    print()

    # Remove node_modules/
    node_modules_dir = repo_root / "node_modules"
    if node_modules_dir.is_dir():
        try:
            size = _get_dir_size(node_modules_dir)
            if dry_run:
                log_info(f"Would remove node_modules/ ({size})")
            else:
                shutil.rmtree(node_modules_dir)
                log_success(f"Removed node_modules/ ({size})")
        except Exception as e:
            log_warning(f"Failed to remove node_modules/: {e}")
    else:
        log_info("No node_modules/ directory found")


def _get_dir_size(path: pathlib.Path) -> str:
    """Get human-readable directory size."""
    try:
        total = 0
        for p in path.rglob("*"):
            if p.is_file():
                total += p.stat().st_size

        if total >= 1024 * 1024 * 1024:
            return f"{total / (1024 * 1024 * 1024):.1f}G"
        elif total >= 1024 * 1024:
            return f"{total / (1024 * 1024):.1f}M"
        elif total >= 1024:
            return f"{total / 1024:.1f}K"
        else:
            return f"{total}B"
    except Exception:
        return "unknown"


def print_summary(stats: CleanupStats, dry_run: bool = False, safe_mode: bool = False) -> None:
    """Print cleanup summary."""
    print()
    print("========================================")
    print("  Summary")
    print("========================================")
    print()

    if dry_run:
        print(f"  Would clean: {stats.cleaned_worktrees} worktree(s)")
    else:
        print(f"  Cleaned: {stats.cleaned_worktrees} worktree(s)")

    if stats.skipped_in_use > 0:
        print(f"  Skipped (in use by shepherd): {stats.skipped_in_use}")

    if stats.skipped_editable > 0:
        print(f"  Skipped (editable pip install): {stats.skipped_editable}")

    if safe_mode:
        print(f"  Skipped (open/not merged): {stats.skipped_open + stats.skipped_not_merged}")
        print(f"  Skipped (grace period): {stats.skipped_grace}")
        print(f"  Skipped (uncommitted): {stats.skipped_uncommitted}")

    if stats.cleaned_branches > 0 or stats.kept_branches > 0:
        if dry_run:
            print(f"  Would delete: {stats.cleaned_branches} branch(es)")
        else:
            print(f"  Deleted: {stats.cleaned_branches} branch(es)")
        print(f"  Kept: {stats.kept_branches} branch(es)")

    if stats.killed_tmux > 0:
        if dry_run:
            print(f"  Would kill: {stats.killed_tmux} tmux session(s)")
        else:
            print(f"  Killed: {stats.killed_tmux} tmux session(s)")

    if stats.cleaned_config_dirs > 0:
        if dry_run:
            print(f"  Would remove: {stats.cleaned_config_dirs} agent config dir(s)")
        else:
            print(f"  Removed: {stats.cleaned_config_dirs} agent config dir(s)")

    if stats.errors > 0:
        print(f"  Errors: {stats.errors}")

    print()


def main(argv: list[str] | None = None) -> int:
    """Main entry point for the clean CLI."""
    parser = argparse.ArgumentParser(
        description="Loom unified cleanup - restore repository to clean state",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Standard cleanup:
  - Stale worktrees (for closed issues)
  - Merged local branches for closed issues
  - Loom tmux sessions (loom-*)
  - Per-agent Claude config directories (.loom/claude-config/)

Deep cleanup (--deep):
  - All of the above, plus:
  - target/ directory (Rust build artifacts)
  - node_modules/ directory

Safe mode (--safe):
  - Only removes worktrees when PR is MERGED (not just closed)
  - Checks for uncommitted changes before removal
  - Applies grace period after merge to avoid race conditions
  - Tracks cleanup state in daemon-state.json

Examples:
  loom-clean                      # Interactive standard cleanup
  loom-clean --force              # Non-interactive cleanup (CI/automation)
  loom-clean --deep               # Include build artifacts
  loom-clean --safe               # Safe mode (MERGED PRs only)
  loom-clean --safe --force       # Safe mode, non-interactive
  loom-clean --worktrees-only     # Just worktrees
  loom-clean --branches-only      # Just branches
""",
    )

    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be cleaned without making changes",
    )
    parser.add_argument(
        "--deep",
        action="store_true",
        help="Deep clean (includes build artifacts)",
    )
    parser.add_argument(
        "-f",
        "--force",
        "-y",
        "--yes",
        action="store_true",
        dest="force",
        help="Non-interactive mode (auto-confirm all prompts)",
    )
    parser.add_argument(
        "--safe",
        action="store_true",
        help="Safe mode: only remove worktrees with MERGED PRs",
    )
    parser.add_argument(
        "--grace-period",
        type=int,
        default=DEFAULT_GRACE_PERIOD,
        help=f"Seconds to wait after PR merge (default: {DEFAULT_GRACE_PERIOD}, requires --safe)",
    )
    parser.add_argument(
        "--worktrees-only",
        "--worktrees",
        action="store_true",
        dest="worktrees_only",
        help="Only clean worktrees (skip branches and tmux)",
    )
    parser.add_argument(
        "--branches-only",
        "--branches",
        action="store_true",
        dest="branches_only",
        help="Only clean branches (skip worktrees and tmux)",
    )
    parser.add_argument(
        "--tmux-only",
        "--tmux",
        action="store_true",
        dest="tmux_only",
        help="Only clean tmux sessions (skip worktrees and branches)",
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    # Show banner
    print()
    print("========================================")
    if args.deep:
        print("  Loom Deep Cleanup")
    elif args.safe:
        print("  Loom Safe Cleanup")
    else:
        print("  Loom Cleanup")
    if args.dry_run:
        print("  (DRY RUN MODE)")
    print("========================================")
    print()

    # Show what will be cleaned
    all_targets = not args.worktrees_only and not args.branches_only and not args.tmux_only

    if all_targets:
        log_info("Cleanup targets:")
        print("  - Orphaned worktrees (git worktree prune)")
        print("  - Local branches for closed issues")
        print("  - Loom tmux sessions (loom-*)")
        print("  - Per-agent config directories (.loom/claude-config/)")

        if args.safe:
            print()
            log_warning("Safe mode enabled:")
            print("  - Only removes worktrees with MERGED PRs")
            print("  - Checks for uncommitted changes")
            print(f"  - Grace period: {args.grace_period}s after merge")

        if args.deep:
            print()
            log_warning("Deep cleanup additions:")
            target_dir = repo_root / "target"
            if target_dir.is_dir():
                size = _get_dir_size(target_dir)
                print(f"  - target/ directory ({size})")
            else:
                print("  - target/ directory (not present)")

            node_modules_dir = repo_root / "node_modules"
            if node_modules_dir.is_dir():
                size = _get_dir_size(node_modules_dir)
                print(f"  - node_modules/ directory ({size})")
            else:
                print("  - node_modules/ directory (not present)")

    print()

    # Confirmation
    if args.dry_run:
        log_warning("DRY RUN - No changes will be made")
        confirm = True
    elif args.force:
        log_info("FORCE MODE - Auto-confirming all prompts")
        confirm = True
    else:
        try:
            response = input("Proceed with cleanup? [y/N] ").strip().lower()
            confirm = response in ("y", "yes")
        except (EOFError, KeyboardInterrupt):
            confirm = False

    if not confirm:
        log_info("Cleanup cancelled")
        return 0

    print()

    stats = CleanupStats()

    # Cleanup: Worktrees
    if not args.branches_only and not args.tmux_only:
        print("Cleaning Worktrees")
        print()
        clean_worktrees(
            repo_root,
            stats,
            dry_run=args.dry_run,
            force=args.force,
            safe_mode=args.safe,
            grace_period=args.grace_period,
        )
        prune_orphaned_worktrees(repo_root, dry_run=args.dry_run)
        print()

    # Cleanup: Branches
    if not args.worktrees_only and not args.tmux_only:
        print("Cleaning Merged Branches")
        print()
        clean_branches(repo_root, stats, dry_run=args.dry_run, force=args.force)
        print()

    # Cleanup: Tmux Sessions
    if not args.worktrees_only and not args.branches_only:
        print("Cleaning Loom Tmux Sessions")
        print()
        clean_tmux_sessions(stats, dry_run=args.dry_run)
        print()

    # Cleanup: Agent Config Directories
    if all_targets:
        print("Cleaning Agent Config Directories")
        print()
        clean_agent_config(repo_root, stats, dry_run=args.dry_run)
        print()

    # Deep Cleanup: Build Artifacts
    if args.deep:
        print("Deep Cleaning Build Artifacts")
        print()
        clean_build_artifacts(repo_root, dry_run=args.dry_run)
        print()

    # Summary
    print_summary(stats, dry_run=args.dry_run, safe_mode=args.safe)

    if args.dry_run:
        log_warning("Dry run complete - no changes made")
        log_info("Run without --dry-run to perform cleanup")
    else:
        log_success("Cleanup complete!")

        if args.deep:
            print()
            log_info("To restore dependencies, run:")
            print("  pnpm install")

    print()

    return 1 if stats.errors > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
