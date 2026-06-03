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
from loom_tools.common.state import read_json_file, read_spawn_loop_state, write_json_file
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
class AggressiveStats:
    """Statistics for ``--aggressive`` worktree cleanup.

    Tracks counts for each branch of the aggressive decision tree so that
    ``print_summary`` can render an audit-friendly report.  Per the
    "skip beats remove" rule, every skip reason must be enumerable.
    """

    removed: int = 0
    skipped_open_pr: int = 0
    skipped_active_shepherd: int = 0
    skipped_user_owned: int = 0  # missing .loom-managed sentinel or non-canonical path
    skipped_uncommitted: int = 0
    skipped_too_recent: int = 0
    skipped_unreachable: int = 0  # HEAD not on origin/main; would lose work
    skipped_locked: int = 0  # main worktree or special entries we don't touch
    errors: int = 0


@dataclass
class WorktreeInfo:
    """A single record parsed from ``git worktree list --porcelain``.

    Attributes
    ----------
    path:
        Absolute path to the worktree directory.
    head:
        SHA of HEAD in the worktree (or ``None`` for bare worktrees).
    branch:
        Full ref of the branch (e.g. ``refs/heads/feature/issue-42``) or
        ``None`` if detached.
    detached:
        ``True`` if the worktree has detached HEAD.
    locked:
        ``True`` if the worktree is locked (``git worktree add --lock`` or
        ``git worktree lock``).
    lock_reason:
        Optional reason string passed to ``git worktree lock --reason``.
    bare:
        ``True`` for the bare main worktree entry.
    """

    path: pathlib.Path
    head: str | None = None
    branch: str | None = None
    detached: bool = False
    locked: bool = False
    lock_reason: str | None = None
    bare: bool = False

    @property
    def branch_short(self) -> str | None:
        """Short branch name (without ``refs/heads/`` prefix)."""
        if self.branch is None:
            return None
        return self.branch.removeprefix("refs/heads/")


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
    """No-op shim (Phase 3.1.9, #3398).

    Previously wrote per-issue cleanup status into
    ``.loom/daemon-state.json::cleanup``. That state file is retired
    with the daemon brain (epic #3372); the spawn loop's per-issue lock
    presence + ``.loom/spawn-loop-state.json`` are the new source of
    truth for "is this issue in flight". Cleanup status was only used
    by the daemon UI, which is also being retired.

    The function signature is preserved so callers don't have to thread
    conditionals; future ports can drop the call sites in Phase 3.3.

    Args:
        repo_root: Repository root path (unused).
        issue_num: Issue number (unused).
        status: One of "cleaned", "pending", "error" (unused).
    """
    # Intentionally empty — no replacement state file. The spawn-loop
    # tracks live work; completed cleanups are observable from
    # ``git worktree list`` and forge state directly.
    del repo_root, issue_num, status


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

    # Snapshot active spawn-loop issues once so we don't re-read the state
    # file inside the per-worktree loop. Cheap and avoids torn-read races.
    # See `_active_spawn_loop_issues` (Phase 3.1.9, #3398).
    active_issues = _active_spawn_loop_issues(repo_root)

    for worktree_dir in sorted(paths.worktrees_dir.glob(f"{NamingConventions.WORKTREE_PREFIX}*")):
        if not worktree_dir.is_dir():
            continue

        # Extract issue number
        issue_num = NamingConventions.issue_from_worktree(worktree_dir.name)
        if issue_num is None:
            continue
        worktree_path = worktree_dir.resolve()

        print(f"Checking worktree: issue-{issue_num}")

        # Spawn-loop active check (Phase 3.1.9, #3398). `--force` bypasses
        # this gate just like the in-use marker / active-process gates.
        if not force and issue_num in active_issues:
            log_info(
                f"  Issue #{issue_num} has a live spawn-loop task or claim-lock - preserving"
            )
            stats.skipped_in_use += 1
            continue

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


def clean_stale_spawn_loop_locks(
    repo_root: pathlib.Path,
    stats: CleanupStats,
    dry_run: bool = False,
) -> None:
    """Remove ``.loom/locks/issue-<N>/`` dirs not backed by a live task.

    Phase 3.1.9 (#3398) addition — the spawn loop drops a claim-lock dir
    on issue claim and removes it on child exit. Surviving locks without
    a corresponding ``spawn-loop-state.json::running[].issue`` entry are
    debris from crashed children and safe to remove.

    This is invoked from the standard worktree cleanup path; it does NOT
    consult the forge (no `gh` calls) since the spawn-loop-state file is
    the local source of truth for "is a child still running".
    """
    removed = _clear_stale_spawn_loop_locks(repo_root, dry_run=dry_run)
    if removed:
        stats.cleaned_worktrees += 0  # counted separately below
        # We piggyback on the worktree counter narrative but don't double
        # count — operators see lock removals as their own log lines.


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


def _list_loom_tmux_sessions() -> list[str]:
    """List all tmux sessions on the loom socket.

    Returns session names (e.g. ``["loom-shepherd-1", "loom-champion"]``).
    """
    try:
        result = subprocess.run(
            ["tmux", "-L", "loom", "list-sessions", "-F", "#{session_name}"],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            return []
        return [s.strip() for s in result.stdout.strip().split("\n") if s.strip()]
    except (FileNotFoundError, Exception):
        return []


def clean_tmux_sessions(
    stats: CleanupStats,
    dry_run: bool = False,
) -> None:
    """Clean up Loom tmux sessions on the loom socket."""
    sessions = _list_loom_tmux_sessions()

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
                    ["tmux", "-L", "loom", "kill-session", "-t", session],
                    capture_output=True,
                    text=True,
                    check=True,
                )
                log_success(f"Killed: {session}")
                stats.killed_tmux += 1
            except Exception:
                pass


def clean_daemon_crash_state(
    repo_root: pathlib.Path,
    dry_run: bool = False,
) -> None:
    """Spawn-loop-aware crash recovery (Phase 3.1.9, #3398).

    Originally a daemon-only flow that touched ``.loom/daemon-state.json``,
    ``.loom/claims/``, and ``.loom/progress/``. Those state files belong
    to the legacy daemon brain, which is being retired (epic #3372).

    The post-port behaviour does only what is still meaningful for a
    spawn-loop-only workspace:

    1. Kill orphaned tmux sessions on the loom socket (unchanged).
    2. Revert stale ``loom:building`` labels for issues that no longer
       have a live spawn-loop task (the spawn loop is authoritative).
    3. Clear stale ``.loom/locks/issue-<N>/`` claim-lock dirs for issues
       that are *not* tracked in ``.loom/spawn-loop-state.json::running``.
    4. Reset ``.loom/issue-failures.json`` (still consumed by Champion).

    The ``--daemon`` flag's banner is preserved for muscle memory but the
    operator-facing rename (``loom-cleanup``, see #3412) is the long-term
    home for any future daemon-era cleanup logic.
    """
    # 1. Kill all loom tmux sessions (still useful for stale sweep workers).
    print("Step 1: Kill orphaned tmux sessions")
    stats = CleanupStats()
    clean_tmux_sessions(stats, dry_run=dry_run)
    print()

    # 2. Revert stale `loom:building` labels for issues no longer running
    #    in the spawn loop. Was previously a `daemon-state.json` scan;
    #    `_revert_stale_building_labels_spawn_loop` reads
    #    `.loom/spawn-loop-state.json` and `gh issue list --label loom:building`.
    print("Step 2: Revert stale `loom:building` labels")
    _revert_stale_building_labels_spawn_loop(repo_root, dry_run=dry_run)
    print()

    # 3. Clear stale spawn-loop claim locks (issues with no live task).
    print("Step 3: Clear stale spawn-loop claim locks")
    _clear_stale_spawn_loop_locks(repo_root, dry_run=dry_run)
    print()

    # 4. Reset issue-failures.json
    print("Step 4: Reset issue-failures.json")
    failures_file = repo_root / ".loom" / "issue-failures.json"
    if failures_file.exists():
        if dry_run:
            log_info("Would reset: issue-failures.json")
        else:
            write_json_file(failures_file, {"entries": {}})
            log_success("Reset issue-failures.json")
    else:
        log_success("No issue-failures.json to reset")
    print()


def _revert_stale_building_labels_spawn_loop(
    repo_root: pathlib.Path,
    dry_run: bool = False,
) -> int:
    """Revert ``loom:building`` -> ``loom:issue`` for orphaned issues.

    An issue is "orphaned" when it carries ``loom:building`` but no live
    spawn-loop task is tracking it (neither in
    ``.loom/spawn-loop-state.json::running`` nor under
    ``.loom/locks/issue-<N>/``). Mirrors the cross-check
    :mod:`loom_tools.orphan_recovery` performs, scoped down for cleanup.

    Returns the number of labels reverted (0 for dry-run skip or no orphans).
    """
    active = _active_spawn_loop_issues(repo_root)
    try:
        result = gh_run(
            ["issue", "list", "--label", "loom:building", "--state", "open",
             "--json", "number", "--jq", ".[].number"],
            check=False,
        )
    except Exception as e:
        log_warning(f"  Could not list building issues: {e}")
        return 0

    if result.returncode != 0:
        log_warning("  `gh issue list` failed — skipping label revert")
        return 0

    building: list[int] = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            building.append(int(line))
        except ValueError:
            continue

    orphans = [n for n in building if n not in active]
    if not orphans:
        log_success("  No orphaned `loom:building` labels found")
        return 0

    reverted = 0
    for issue_num in orphans:
        if dry_run:
            log_info(f"  Would revert label on #{issue_num}: building -> issue")
            continue
        try:
            subprocess.run(
                ["gh", "issue", "edit", str(issue_num),
                 "--remove-label", "loom:building",
                 "--add-label", "loom:issue"],
                capture_output=True, text=True, check=False,
            )
            log_success(f"  Reverted #{issue_num}: building -> issue")
            reverted += 1
        except Exception as e:
            log_warning(f"  Failed to revert #{issue_num}: {e}")
    return reverted


def _clear_stale_spawn_loop_locks(
    repo_root: pathlib.Path,
    dry_run: bool = False,
) -> int:
    """Remove ``.loom/locks/issue-<N>/`` dirs for issues not in running set.

    The spawn loop releases its own locks on child exit (``release_lock``
    in ``spawn-loop.sh``), so a surviving lock without a corresponding
    ``running[].issue`` entry indicates a crashed child whose recovery has
    already happened (or for which recovery is no longer interesting —
    e.g. the issue was closed manually).

    Returns the number of locks removed (or that would be removed in
    dry-run mode).
    """
    locks_dir = _spawn_loop_locks_dir(repo_root)
    if not locks_dir.is_dir():
        log_success("  No `.loom/locks/` directory")
        return 0

    state = read_spawn_loop_state(repo_root)
    live_issues = {t.issue for t in state.running if t.issue}

    removed = 0
    found_any = False
    for entry in sorted(locks_dir.iterdir()):
        if not entry.is_dir() or not entry.name.startswith("issue-"):
            continue
        found_any = True
        try:
            issue_num = int(entry.name[len("issue-") :])
        except ValueError:
            log_warning(f"  Skipping malformed lock dir: {entry.name}")
            continue

        if issue_num in live_issues:
            log_info(f"  Keeping lock for live task: {entry.name}")
            continue

        if dry_run:
            log_info(f"  Would remove stale lock: {entry.name}")
            removed += 1
        else:
            try:
                shutil.rmtree(entry)
                log_success(f"  Removed stale lock: {entry.name}")
                removed += 1
            except Exception as e:
                log_warning(f"  Failed to remove {entry.name}: {e}")

    if not found_any:
        log_success("  No spawn-loop locks to inspect")
    return removed


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


# ---------------------------------------------------------------------------
# Aggressive cleanup: enumerate `git worktree list --porcelain` and apply a
# strict decision tree.  See issue #3332 for the rationale.
# ---------------------------------------------------------------------------

# Default minimum worktree age for --aggressive removal (24h in seconds).
DEFAULT_AGGRESSIVE_MIN_AGE = 86400

# Decision constants returned by `evaluate_aggressive_candidate`.
DECISION_REMOVE = "remove"
DECISION_KEEP = "keep"

# Sentinel filename used to mark Loom-managed worktrees (see issue #3334).
LOOM_MANAGED_SENTINEL = ".loom-managed"


def enumerate_git_worktrees(repo_root: pathlib.Path) -> list[WorktreeInfo]:
    """Parse ``git worktree list --porcelain`` into structured records.

    The porcelain format emits one record per worktree separated by blank
    lines.  Each record looks like::

        worktree /path/to/worktree
        HEAD abc123...
        branch refs/heads/feature/issue-42
        locked optional reason text

    A bare worktree is identified by a ``bare`` line in place of ``HEAD``.
    Detached HEAD worktrees emit ``detached`` instead of ``branch``.

    On any error the function returns an empty list rather than raising —
    aggressive cleanup must fail closed.
    """
    try:
        result = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            capture_output=True,
            text=True,
            cwd=repo_root,
            check=False,
        )
        if result.returncode != 0:
            return []
    except Exception:
        return []

    worktrees: list[WorktreeInfo] = []
    current: WorktreeInfo | None = None

    for raw_line in result.stdout.splitlines():
        line = raw_line.rstrip("\n")
        if not line:
            if current is not None:
                worktrees.append(current)
                current = None
            continue

        if line.startswith("worktree "):
            # Start of a new record — flush any partial entry first.
            if current is not None:
                worktrees.append(current)
            current = WorktreeInfo(path=pathlib.Path(line[len("worktree ") :]))
        elif current is None:
            # Skip stray lines outside a record (defensive).
            continue
        elif line.startswith("HEAD "):
            current.head = line[len("HEAD ") :].strip()
        elif line.startswith("branch "):
            current.branch = line[len("branch ") :].strip()
        elif line == "detached":
            current.detached = True
        elif line == "bare":
            current.bare = True
        elif line == "locked":
            current.locked = True
        elif line.startswith("locked "):
            current.locked = True
            current.lock_reason = line[len("locked ") :].strip() or None
        # Other fields (prunable, etc.) are ignored.

    if current is not None:
        worktrees.append(current)

    return worktrees


def _is_ancestor_of_origin_main(
    repo_root: pathlib.Path,
    head_sha: str,
) -> bool:
    """Return True if ``head_sha`` is reachable from ``origin/main``.

    Uses ``git merge-base --is-ancestor`` which returns exit 0 when the
    ancestor relationship holds.  Any error or non-zero exit returns
    False so the caller fails closed.
    """
    if not head_sha:
        return False
    try:
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", head_sha, "origin/main"],
            capture_output=True,
            text=True,
            cwd=repo_root,
            check=False,
        )
        return result.returncode == 0
    except Exception:
        return False


def _check_open_pr(branch_short: str | None) -> tuple[bool, bool]:
    """Check whether ``branch_short`` has an open PR.

    Returns
    -------
    tuple[bool, bool]
        ``(has_open_pr, lookup_succeeded)``.  When ``lookup_succeeded`` is
        False the caller must treat the candidate as "uncertain" and
        skip (fail closed).
    """
    if not branch_short:
        return False, True
    try:
        prs = gh_list(
            "pr",
            head=branch_short,
            state="open",
            fields=["number", "state"],
            limit=1,
        )
    except Exception:
        return False, False
    return bool(prs), True


def _spawn_loop_locks_dir(repo_root: pathlib.Path) -> pathlib.Path:
    """Path to the spawn-loop's atomic claim-lock directory.

    See ``defaults/scripts/spawn-loop.sh``: each in-flight sweep child holds
    a ``.loom/locks/issue-<N>/`` directory (mkdir-atomic primitive). The
    directory's presence is the lock; a tiny ``owner.json`` metadata file
    lives inside for debugging but is not load-bearing.
    """
    return repo_root / ".loom" / "locks"


def _active_locked_issues(repo_root: pathlib.Path) -> set[int]:
    """Issues with a present ``.loom/locks/issue-<N>/`` claim-lock dir.

    The lock dir is created on claim and removed on child exit (see
    ``release_lock`` in ``spawn-loop.sh``). A lock without a corresponding
    spawn-loop-state entry is *probably* stale (crashed child failed to
    release), but caller policy lives in :func:`_active_spawn_loop_issues`
    — this helper returns the raw set so other callers can reason about
    locks independently of state-file contents.
    """
    locks_dir = _spawn_loop_locks_dir(repo_root)
    if not locks_dir.is_dir():
        return set()
    active: set[int] = set()
    # Pattern: issue-<N> directories only; ignore stray files.
    for entry in locks_dir.iterdir():
        if not entry.is_dir():
            continue
        name = entry.name
        if not name.startswith("issue-"):
            continue
        try:
            active.add(int(name[len("issue-") :]))
        except ValueError:
            continue
    return active


def _active_spawn_loop_issues(repo_root: pathlib.Path) -> set[int]:
    """Return the set of issue numbers currently in flight in the spawn loop.

    Reads two sources and unions them (either alone is sufficient evidence
    that a worktree is in use):

    1. ``.loom/spawn-loop-state.json::running[*].issue`` — the spawn loop's
       authoritative list of live sweep children. See
       :class:`loom_tools.models.spawn_loop_state.SpawnLoopState`.
    2. ``.loom/locks/issue-<N>/`` — atomic claim-lock dirs created by
       ``acquire_lock`` in ``spawn-loop.sh`` and released only on child
       exit. Surviving locks belong to either active children (covered by
       #1) or crashed children — in both cases we conservatively treat the
       worktree as "in use" so cleanup does not race the recovery flow.

    Missing/malformed state files and missing locks-dir yield an empty set;
    callers (e.g. :func:`evaluate_aggressive_candidate`) combine this with
    other gates (PR check, sentinel, reachability) rather than relying on
    it alone.

    Phase 3.1.9 port (#3398, epic #3372): replaces the prior
    ``daemon-state.json::shepherds`` read so ``loom-clean`` works against
    workspaces that have migrated off the Python daemon brain.
    """
    state = read_spawn_loop_state(repo_root)
    active: set[int] = {task.issue for task in state.running if task.issue}
    active |= _active_locked_issues(repo_root)
    return active


# Backwards-compatible alias retained for callers that imported the old
# name (notably aggressive-mode tests). The decision is "is this issue
# being worked on by an orchestrator?" — the spawn loop is the only
# orchestrator we recognize after Phase 3.1.x, so the rename is a pure
# refactor.
_active_shepherd_issues = _active_spawn_loop_issues


def _worktree_age_seconds(path: pathlib.Path) -> float | None:
    """Return the worktree's directory mtime age in seconds, or None."""
    try:
        st = path.stat()
    except OSError:
        return None
    now = datetime.now(timezone.utc).timestamp()
    return max(0.0, now - st.st_mtime)


def evaluate_aggressive_candidate(
    wt: WorktreeInfo,
    repo_root: pathlib.Path,
    active_shepherd_issues: set[int],
    min_age_seconds: int,
    force: bool,
) -> tuple[str, str]:
    """Apply the aggressive decision tree to a single worktree.

    Decision order (first hit wins; "skip" beats "remove"):

    1. Skip the main / bare worktree (never touch).
    2. Open-PR check → skip (``reason=open_pr``).
    3. Active-shepherd check → skip (``reason=active_shepherd``).
    4. Sentinel / canonical-path check → skip (``reason=user_owned``)
       when the worktree is outside ``.loom/worktrees/`` OR is inside
       it but lacks the ``.loom-managed`` sentinel.
    5. Uncommitted-changes check → skip (``reason=uncommitted``) unless
       ``force`` is True.
    6. Reachability check → if HEAD is an ancestor of ``origin/main`` the
       work is preserved on the remote, so it is safe to remove.
    7. mtime guard → skip (``reason=too_recent``) when the worktree is
       younger than ``min_age_seconds``.
    8. Fallback → skip (``reason=unreachable_head``) unless ``force`` is
       True (the operator accepts data-loss risk).

    Returns
    -------
    tuple[str, str]
        ``(decision, reason)`` where ``decision`` is one of
        :data:`DECISION_REMOVE` / :data:`DECISION_KEEP`.
    """
    # 1) Never touch the bare/main worktree.
    if wt.bare:
        return DECISION_KEEP, "bare_main_worktree"

    # Resolve repo_root once for path comparisons.
    try:
        resolved_repo = repo_root.resolve()
    except OSError:
        resolved_repo = repo_root
    try:
        resolved_wt = wt.path.resolve()
    except OSError:
        resolved_wt = wt.path

    if resolved_wt == resolved_repo:
        return DECISION_KEEP, "bare_main_worktree"

    # 2) Open-PR check (forge-aware via gh_list).
    if wt.branch_short:
        has_pr, ok = _check_open_pr(wt.branch_short)
        if not ok:
            # Fail closed when the lookup failed.
            return DECISION_KEEP, "pr_lookup_failed"
        if has_pr:
            return DECISION_KEEP, "open_pr"

    # 3) Active-shepherd check (daemon state).
    if wt.branch_short:
        issue_num = NamingConventions.issue_from_branch(wt.branch_short)
        if issue_num is not None and issue_num in active_shepherd_issues:
            return DECISION_KEEP, "active_shepherd"

    # 4) Sentinel / canonical-path check.
    worktrees_dir = (resolved_repo / ".loom" / "worktrees").resolve()
    try:
        is_under_loom = (
            resolved_wt == worktrees_dir
            or worktrees_dir in resolved_wt.parents
        )
    except Exception:
        is_under_loom = False

    if not is_under_loom:
        # Defense in depth: never touch worktrees at non-canonical paths.
        return DECISION_KEEP, "user_owned"

    sentinel = resolved_wt / LOOM_MANAGED_SENTINEL
    if not sentinel.exists():
        # Couples to #3334.  Pre-existing worktrees can be opted in by
        # running `touch .loom/worktrees/issue-N/.loom-managed`.
        return DECISION_KEEP, "user_owned"

    # 5) Uncommitted changes (unless --force overrides).
    if check_uncommitted_changes(resolved_wt) and not force:
        return DECISION_KEEP, "uncommitted"

    # 6) Reachability — HEAD on origin/main means the work is preserved.
    head_reachable = bool(wt.head) and _is_ancestor_of_origin_main(
        repo_root, wt.head or ""
    )
    if head_reachable:
        return DECISION_REMOVE, "reachable_from_origin_main"

    # 7) mtime guard.
    age = _worktree_age_seconds(resolved_wt)
    if age is not None and age < min_age_seconds:
        return DECISION_KEEP, "too_recent"

    # 8) Fallback — HEAD not reachable.  Removing would lose work.
    if force:
        return DECISION_REMOVE, "force_override_unreachable"
    return DECISION_KEEP, "unreachable_head"


def _remove_aggressive_worktree(
    repo_root: pathlib.Path,
    wt: WorktreeInfo,
    dry_run: bool,
) -> bool:
    """Unlock (if locked), remove the worktree, and delete its branch.

    Returns True on success, False on any error.  Errors are logged but
    not raised so the caller can continue processing other worktrees.
    """
    if dry_run:
        log_info(f"Would remove worktree: {wt.path}")
        if wt.branch_short:
            log_info(f"Would delete branch: {wt.branch_short}")
        return True

    # Unlock if locked — `git worktree remove` refuses locked entries.
    if wt.locked:
        try:
            subprocess.run(
                ["git", "worktree", "unlock", str(wt.path)],
                capture_output=True,
                text=True,
                cwd=repo_root,
                check=False,
            )
        except Exception:
            log_warning(f"  Failed to unlock worktree: {wt.path}")

    # Remove the worktree with --force (covers dirty/locked edge cases).
    try:
        result = subprocess.run(
            ["git", "worktree", "remove", "--force", str(wt.path)],
            capture_output=True,
            text=True,
            cwd=repo_root,
            check=False,
        )
        if result.returncode != 0:
            log_warning(
                f"  Failed to remove worktree {wt.path}: {result.stderr.strip()}"
            )
            return False
        log_success(f"  Removed worktree: {wt.path}")
    except Exception as e:
        log_warning(f"  Error removing worktree {wt.path}: {e}")
        return False

    # Delete the local branch (force delete since we just nuked the worktree).
    if wt.branch_short:
        try:
            result = subprocess.run(
                ["git", "branch", "-D", wt.branch_short],
                capture_output=True,
                text=True,
                cwd=repo_root,
                check=False,
            )
            if result.returncode == 0:
                log_success(f"  Deleted branch: {wt.branch_short}")
            else:
                # Branch may already be gone or referenced elsewhere — not fatal.
                log_info(
                    f"  Branch not deleted ({wt.branch_short}): "
                    f"{result.stderr.strip()}"
                )
        except Exception as e:
            log_info(f"  Branch delete skipped ({wt.branch_short}): {e}")

    return True


def clean_aggressive(
    repo_root: pathlib.Path,
    dry_run: bool = False,
    force: bool = False,
    min_age_seconds: int = DEFAULT_AGGRESSIVE_MIN_AGE,
) -> AggressiveStats:
    """Aggressive cleanup pass for vestigial / locked Loom worktrees.

    Enumerates every worktree (not just ``.loom/worktrees/issue-*``)
    and applies :func:`evaluate_aggressive_candidate`.  Worktrees whose
    decision is ``DECISION_REMOVE`` are unlocked, removed (with
    ``--force``), and their branches force-deleted.  All other
    worktrees are left alone with a one-line audit log.

    Returns an :class:`AggressiveStats` instance summarising the run.
    """
    stats = AggressiveStats()
    active_shepherds = _active_shepherd_issues(repo_root)

    worktrees = enumerate_git_worktrees(repo_root)
    if not worktrees:
        log_info("No worktrees enumerated from `git worktree list`")
        return stats

    for wt in worktrees:
        # Compact one-line label for logging.
        label = str(wt.path)
        if wt.branch_short:
            label = f"{wt.path} [{wt.branch_short}]"
        elif wt.detached:
            label = f"{wt.path} [detached]"

        decision, reason = evaluate_aggressive_candidate(
            wt,
            repo_root,
            active_shepherds,
            min_age_seconds,
            force,
        )

        if decision == DECISION_KEEP:
            if reason == "bare_main_worktree":
                stats.skipped_locked += 1
                log_info(f"  Skip (main worktree): {label}")
            elif reason in ("open_pr", "pr_lookup_failed"):
                stats.skipped_open_pr += 1
                log_info(f"  Skip ({reason}): {label}")
            elif reason == "active_shepherd":
                stats.skipped_active_shepherd += 1
                log_info(f"  Skip (active shepherd): {label}")
            elif reason == "user_owned":
                stats.skipped_user_owned += 1
                log_info(f"  Skip (user-owned / no .loom-managed sentinel): {label}")
            elif reason == "uncommitted":
                stats.skipped_uncommitted += 1
                log_warning(
                    f"  Skip (uncommitted changes; pass --force to override): {label}"
                )
            elif reason == "too_recent":
                stats.skipped_too_recent += 1
                log_info(f"  Skip (younger than min-age): {label}")
            elif reason == "unreachable_head":
                stats.skipped_unreachable += 1
                log_warning(
                    f"  Skip (HEAD not on origin/main — would lose work): {label}"
                )
                if wt.head:
                    log_info(
                        f"    HEAD={wt.head[:12]} (recoverable via `git reflog`)"
                    )
            else:
                stats.errors += 1
                log_warning(f"  Skip (unknown reason: {reason}): {label}")
            continue

        # decision == DECISION_REMOVE
        if reason == "force_override_unreachable":
            log_warning(
                f"  Removing despite unreachable HEAD ({wt.head[:12] if wt.head else '?'}): {label}"
            )
        else:
            log_info(f"  Remove ({reason}): {label}")

        if _remove_aggressive_worktree(repo_root, wt, dry_run):
            stats.removed += 1
        else:
            stats.errors += 1

    # Final prune to clean up administrative references.
    if not dry_run:
        try:
            subprocess.run(
                ["git", "worktree", "prune"],
                capture_output=True,
                text=True,
                cwd=repo_root,
                check=False,
            )
        except Exception:
            pass

    return stats


def print_aggressive_summary(stats: AggressiveStats, dry_run: bool = False) -> None:
    """Render the aggressive-mode counters in audit-friendly form."""
    print()
    print("========================================")
    print("  Aggressive Cleanup Summary")
    print("========================================")
    print()
    if dry_run:
        print(f"  Would remove: {stats.removed} worktree(s)")
    else:
        print(f"  Removed: {stats.removed} worktree(s)")
    if stats.skipped_open_pr:
        print(f"  Skipped (open PR / lookup failed): {stats.skipped_open_pr}")
    if stats.skipped_active_shepherd:
        print(f"  Skipped (active shepherd): {stats.skipped_active_shepherd}")
    if stats.skipped_user_owned:
        print(f"  Skipped (user-owned / no .loom-managed sentinel): {stats.skipped_user_owned}")
    if stats.skipped_uncommitted:
        print(f"  Skipped (uncommitted changes): {stats.skipped_uncommitted}")
    if stats.skipped_too_recent:
        print(f"  Skipped (younger than min-age): {stats.skipped_too_recent}")
    if stats.skipped_unreachable:
        print(f"  Skipped (HEAD unreachable — would lose work): {stats.skipped_unreachable}")
    if stats.skipped_locked:
        print(f"  Skipped (main worktree): {stats.skipped_locked}")
    if stats.errors:
        print(f"  Errors: {stats.errors}")
    print()


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

Crash cleanup (--daemon):
  - Kill orphaned tmux sessions on the loom socket
  - Revert stale `loom:building` labels for issues with no live spawn-loop task
  - Clear stale `.loom/locks/issue-<N>/` claim-lock dirs
  - Reset `.loom/issue-failures.json`

Aggressive cleanup (--aggressive):
  - Enumerates ALL worktrees (`git worktree list --porcelain`),
    not just `.loom/worktrees/issue-*`.
  - Overrides safety gates that strand vestigial worktrees:
    ignores `.loom-in-use` markers, ignores stale
    `find_processes_using_directory` matches, and skips the
    `issue_state == "CLOSED"` precondition.
  - Still respects these gates in order (first hit wins,
    "skip" beats "remove"):
      1. Open PR on the branch -> skip (open_pr)
      2. Active spawn-loop task or claim-lock holds the issue
         -> skip (active_shepherd) [name preserved for back-compat]
      3. Worktree path outside `.loom/worktrees/` or missing the
         `.loom-managed` sentinel -> skip (user_owned)
      4. Uncommitted changes -> skip (uncommitted) unless --force
      5. HEAD reachable from origin/main -> REMOVE (work preserved)
      6. Worktree younger than --aggressive-min-age -> skip (too_recent)
      7. Otherwise skip (unreachable_head); pass --force to override.
  - Locked worktrees are unlocked, then removed with `--force`.
  - Pre-existing Loom worktrees created before the `.loom-managed`
    sentinel existed must be opted in via
    `touch .loom/worktrees/issue-N/.loom-managed` before the first
    --aggressive run.
  - Always preview first: `loom-clean --aggressive --dry-run`.

Examples:
  loom-clean                              # Interactive standard cleanup
  loom-clean --force                      # Non-interactive cleanup (CI/automation)
  loom-clean --deep                       # Include build artifacts
  loom-clean --safe                       # Safe mode (MERGED PRs only)
  loom-clean --safe --force               # Safe mode, non-interactive
  loom-clean --daemon                     # Full daemon crash recovery
  loom-clean --daemon --dry-run           # Preview daemon crash recovery
  loom-clean --worktrees-only             # Just worktrees
  loom-clean --branches-only              # Just branches
  loom-clean --aggressive --dry-run       # Preview vestigial-worktree cleanup
  loom-clean --aggressive --force         # Remove vestigial worktrees (no prompts)
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
    parser.add_argument(
        "--daemon",
        action="store_true",
        help=(
            "Crash recovery: kill tmux sessions, revert stale `loom:building` "
            "labels for issues with no live spawn-loop task, clear stale "
            "claim-lock dirs, reset issue-failures.json"
        ),
    )
    parser.add_argument(
        "--aggressive",
        action="store_true",
        help=(
            "Aggressive: enumerate `git worktree list --porcelain`, ignore "
            "`.loom-in-use` markers and process-table noise, and remove "
            "vestigial worktrees whose work is reachable from origin/main. "
            "Respects open PRs, active spawn-loop tasks (and lock dirs), the "
            "`.loom-managed` sentinel, and uncommitted changes. Use --dry-run "
            "to preview."
        ),
    )
    parser.add_argument(
        "--aggressive-min-age",
        type=int,
        default=DEFAULT_AGGRESSIVE_MIN_AGE,
        help=(
            "Minimum worktree age in seconds for --aggressive removal "
            f"(default: {DEFAULT_AGGRESSIVE_MIN_AGE} = 24h)."
        ),
    )

    args = parser.parse_args(argv)

    try:
        repo_root = find_repo_root()
    except FileNotFoundError:
        log_error("Not in a git repository with .loom directory")
        return 1

    # --daemon: crash recovery (spawn-loop-aware after Phase 3.1.9, #3398).
    # Flag name preserved for muscle memory; behaviour now targets the
    # spawn loop's state files instead of the retired daemon brain.
    if args.daemon:
        print()
        print("========================================")
        print("  Loom Crash Recovery")
        if args.dry_run:
            print("  (DRY RUN MODE)")
        print("========================================")
        print()
        clean_daemon_crash_state(repo_root, dry_run=args.dry_run)
        if args.dry_run:
            log_warning("Dry run complete - no changes made")
        else:
            log_success("Crash recovery complete!")
        print()
        return 0

    # --aggressive: vestigial-worktree cleanup (separate workflow; see #3332)
    if args.aggressive:
        print()
        print("========================================")
        print("  Loom Aggressive Worktree Cleanup")
        if args.dry_run:
            print("  (DRY RUN MODE)")
        print("========================================")
        print()
        log_warning(
            "Aggressive mode overrides .loom-in-use markers and process-table "
            "guards. Respects open PRs, active shepherds, the .loom-managed "
            "sentinel, uncommitted changes, and reachability from origin/main."
        )
        print()

        if not args.dry_run and not args.force:
            try:
                response = input(
                    "Proceed with aggressive cleanup? [y/N] "
                ).strip().lower()
            except (EOFError, KeyboardInterrupt):
                response = ""
            if response not in ("y", "yes"):
                log_info("Aggressive cleanup cancelled")
                return 0

        agg_stats = clean_aggressive(
            repo_root,
            dry_run=args.dry_run,
            force=args.force,
            min_age_seconds=args.aggressive_min_age,
        )
        print_aggressive_summary(agg_stats, dry_run=args.dry_run)
        if args.dry_run:
            log_warning("Dry run complete - no changes made")
            log_info("Run without --dry-run to perform cleanup")
        else:
            log_success("Aggressive cleanup complete!")
        print()
        return 1 if agg_stats.errors > 0 else 0

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

        # Phase 3.1.9 (#3398) - also prune stale spawn-loop claim locks.
        print("Cleaning Stale Spawn-Loop Locks")
        print()
        clean_stale_spawn_loop_locks(repo_root, stats, dry_run=args.dry_run)
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
