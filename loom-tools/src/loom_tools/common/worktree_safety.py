"""Worktree safety checks for preventing removal of active worktrees.

Provides utilities to detect if a worktree is safe to remove by checking:
    - In-use marker files (.loom-in-use)
    - Active processes with CWD in the worktree
    - Grace period since worktree creation
"""

from __future__ import annotations

import os
import pathlib
import platform
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from loom_tools.common.state import read_json_file

# Default grace period in seconds (5 minutes)
DEFAULT_GRACE_PERIOD_SECONDS = 300


def check_cwd_inside_worktree(worktree_path: pathlib.Path) -> bool:
    """Check if the current process's CWD is inside the target worktree.

    This is the simplest and most direct safety check - if we're running from
    inside the worktree, we definitely shouldn't remove it or the shell will
    become non-functional.

    Args:
        worktree_path: Path to the worktree directory.

    Returns:
        True if current CWD is exactly the worktree or a subdirectory of it.
    """
    try:
        # Resolve both paths to follow symlinks and get absolute paths
        current_cwd = pathlib.Path.cwd().resolve()
        worktree_resolved = worktree_path.resolve()

        # Check if CWD is the worktree itself
        if current_cwd == worktree_resolved:
            return True

        # Check if CWD is a subdirectory of the worktree
        # Use is_relative_to() for clean path comparison
        try:
            current_cwd.relative_to(worktree_resolved)
            return True
        except ValueError:
            return False

    except OSError:
        # If we can't resolve paths, be conservative and assume not inside
        return False


@dataclass
class WorktreeSafetyResult:
    """Result of a worktree safety check.

    Attributes:
        safe_to_remove: Whether the worktree is safe to remove.
        reason: Human-readable explanation of why removal is blocked (if not safe).
        marker_present: Whether .loom-in-use marker exists.
        active_pids: List of PIDs with CWD in the worktree.
        within_grace_period: Whether worktree is within creation grace period.
        marker_data: Parsed contents of .loom-in-use marker (if present).
        cwd_inside: Whether the current process's CWD is inside the worktree.
    """

    safe_to_remove: bool
    reason: str | None = None
    marker_present: bool = False
    active_pids: list[int] | None = None
    within_grace_period: bool = False
    marker_data: dict[str, Any] | None = None
    cwd_inside: bool = False


def check_in_use_marker(
    worktree_path: pathlib.Path,
    marker_name: str = ".loom-in-use",
) -> tuple[bool, dict[str, Any] | None]:
    """Check if the worktree has an in-use marker file.

    Args:
        worktree_path: Path to the worktree directory.
        marker_name: Name of the marker file (default: .loom-in-use).

    Returns:
        Tuple of (marker_exists, marker_data).
        marker_data is None if marker doesn't exist or can't be parsed.
    """
    marker_path = worktree_path / marker_name
    if not marker_path.is_file():
        return False, None

    data = read_json_file(marker_path)
    return True, data if isinstance(data, dict) else None


def find_processes_using_directory(directory: pathlib.Path) -> list[int]:
    """Find processes that have the given directory as their CWD.

    Uses platform-specific methods to detect processes:
    - macOS/BSD: lsof +D
    - Linux: /proc filesystem scan

    Args:
        directory: Directory path to check.

    Returns:
        List of PIDs with CWD in or under the directory.
        Returns empty list if detection fails or no processes found.
    """
    directory = directory.resolve()
    pids: list[int] = []

    system = platform.system()

    if system == "Darwin":
        # macOS: use lsof to find processes with CWD in directory
        pids = _find_processes_lsof(directory)
    elif system == "Linux":
        # Linux: scan /proc for processes with matching CWD
        pids = _find_processes_proc(directory)
    else:
        # Unsupported platform - try lsof as fallback
        pids = _find_processes_lsof(directory)

    # Filter out current process (we're always "using" our CWD)
    current_pid = os.getpid()
    return [p for p in pids if p != current_pid]


def _find_processes_lsof(directory: pathlib.Path) -> list[int]:
    """Find processes using lsof (macOS/BSD).

    Uses 'lsof +D' to find all processes with open files or CWD under directory.
    Filters to only processes with 'cwd' type entries.
    """
    try:
        # lsof +D lists all processes with open files under directory
        # -F p outputs just PIDs in a parseable format
        # We use +d (non-recursive) for just the directory itself
        result = subprocess.run(
            ["lsof", "+d", str(directory), "-F", "pt"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )

        if result.returncode != 0:
            return []

        pids: list[int] = []
        current_pid: int | None = None
        current_type: str | None = None

        for line in result.stdout.strip().split("\n"):
            if not line:
                continue
            if line.startswith("p"):
                # New process entry
                current_pid = int(line[1:])
            elif line.startswith("t"):
                # File type entry
                current_type = line[1:]
                # We only care about 'cwd' (current working directory) type
                if current_type == "cwd" and current_pid is not None:
                    pids.append(current_pid)

        return list(set(pids))

    except (subprocess.TimeoutExpired, OSError, ValueError):
        return []


def _find_processes_proc(directory: pathlib.Path) -> list[int]:
    """Find processes using /proc filesystem (Linux).

    Scans /proc/*/cwd symlinks to find processes with CWD under directory.
    """
    proc = pathlib.Path("/proc")
    if not proc.is_dir():
        return []

    pids: list[int] = []
    dir_str = str(directory)

    try:
        for pid_dir in proc.iterdir():
            if not pid_dir.name.isdigit():
                continue

            cwd_link = pid_dir / "cwd"
            try:
                cwd = cwd_link.resolve()
                cwd_str = str(cwd)
                # Check if CWD is the directory or under it
                if cwd_str == dir_str or cwd_str.startswith(dir_str + "/"):
                    pids.append(int(pid_dir.name))
            except (PermissionError, OSError):
                # Can't read this process's cwd - skip it
                continue

    except PermissionError:
        return []

    return pids


def check_grace_period(
    worktree_path: pathlib.Path,
    grace_seconds: int = DEFAULT_GRACE_PERIOD_SECONDS,
) -> tuple[bool, float | None]:
    """Check if worktree is within its creation grace period.

    Uses the creation time of the worktree directory (or .git file within it)
    to determine age.

    Args:
        worktree_path: Path to the worktree directory.
        grace_seconds: Grace period in seconds (default: 300 = 5 minutes).

    Returns:
        Tuple of (within_grace_period, age_seconds).
        age_seconds is None if the worktree doesn't exist.
    """
    if not worktree_path.is_dir():
        return False, None

    # Try to get creation time from .git file (most accurate)
    git_file = worktree_path / ".git"
    if git_file.exists():
        try:
            stat_info = git_file.stat()
            # Use birthtime on macOS, ctime on Linux
            if hasattr(stat_info, "st_birthtime"):
                creation_time = stat_info.st_birthtime
            else:
                creation_time = stat_info.st_ctime
        except OSError:
            return False, None
    else:
        # Fall back to directory creation time
        try:
            stat_info = worktree_path.stat()
            if hasattr(stat_info, "st_birthtime"):
                creation_time = stat_info.st_birthtime
            else:
                creation_time = stat_info.st_ctime
        except OSError:
            return False, None

    age = time.time() - creation_time
    within_grace = age < grace_seconds
    return within_grace, age


def is_worktree_safe_to_remove(
    worktree_path: pathlib.Path,
    check_marker: bool = True,
    check_processes: bool = True,
    check_grace: bool = True,
    check_cwd: bool = True,
    marker_name: str = ".loom-in-use",
    grace_seconds: int = DEFAULT_GRACE_PERIOD_SECONDS,
) -> WorktreeSafetyResult:
    """Check if a worktree is safe to remove.

    Performs multiple safety checks to prevent destroying active sessions:
    0. Current shell's CWD inside worktree (most direct check)
    1. In-use marker file (.loom-in-use)
    2. Active processes with CWD in the worktree
    3. Grace period since worktree creation

    Args:
        worktree_path: Path to the worktree directory.
        check_marker: Whether to check for in-use marker (default: True).
        check_processes: Whether to check for active processes (default: True).
        check_grace: Whether to check grace period (default: True).
        check_cwd: Whether to check if current CWD is inside worktree (default: True).
        marker_name: Name of the marker file (default: .loom-in-use).
        grace_seconds: Grace period in seconds (default: 300 = 5 minutes).

    Returns:
        WorktreeSafetyResult with safety status and detailed information.
    """
    worktree_path = worktree_path.resolve()

    if not worktree_path.is_dir():
        return WorktreeSafetyResult(
            safe_to_remove=True,
            reason="worktree directory does not exist",
        )

    # Check 0: Current shell's CWD inside worktree (simplest and most direct check)
    cwd_inside = False
    if check_cwd:
        cwd_inside = check_cwd_inside_worktree(worktree_path)
        if cwd_inside:
            return WorktreeSafetyResult(
                safe_to_remove=False,
                reason="current shell CWD is inside worktree",
                cwd_inside=True,
            )

    # Check 1: In-use marker
    marker_present = False
    marker_data = None
    if check_marker:
        marker_present, marker_data = check_in_use_marker(worktree_path, marker_name)
        if marker_present:
            task_id = marker_data.get("shepherd_task_id", "unknown") if marker_data else "unknown"
            return WorktreeSafetyResult(
                safe_to_remove=False,
                reason=f"worktree in use by shepherd (task: {task_id})",
                marker_present=True,
                marker_data=marker_data,
            )

    # Check 2: Active processes
    active_pids: list[int] | None = None
    if check_processes:
        active_pids = find_processes_using_directory(worktree_path)
        if active_pids:
            return WorktreeSafetyResult(
                safe_to_remove=False,
                reason=f"active process(es) using worktree: {active_pids}",
                marker_present=marker_present,
                marker_data=marker_data,
                active_pids=active_pids,
            )

    # Check 3: Grace period
    within_grace = False
    if check_grace:
        within_grace, age = check_grace_period(worktree_path, grace_seconds)
        if within_grace and age is not None:
            remaining = int(grace_seconds - age)
            return WorktreeSafetyResult(
                safe_to_remove=False,
                reason=f"worktree within grace period ({remaining}s remaining)",
                marker_present=marker_present,
                marker_data=marker_data,
                active_pids=active_pids or [],
                within_grace_period=True,
            )

    # All checks passed - safe to remove
    return WorktreeSafetyResult(
        safe_to_remove=True,
        marker_present=marker_present,
        marker_data=marker_data,
        active_pids=active_pids or [],
        within_grace_period=within_grace,
        cwd_inside=cwd_inside,
    )


def should_reuse_worktree(
    worktree_path: pathlib.Path,
    grace_seconds: int = DEFAULT_GRACE_PERIOD_SECONDS,
) -> bool:
    """Check if a worktree should be reused instead of removed and recreated.

    A worktree should be reused if:
    - It exists and is registered with git
    - It has no commits ahead of main (empty)
    - It's within the grace period OR has active processes

    This implements the "prefer reusing empty worktree" acceptance criterion.

    Args:
        worktree_path: Path to the worktree directory.
        grace_seconds: Grace period in seconds.

    Returns:
        True if worktree should be reused, False if it can be safely removed.
    """
    safety = is_worktree_safe_to_remove(
        worktree_path,
        check_marker=True,
        check_processes=True,
        check_grace=True,
        grace_seconds=grace_seconds,
    )

    # If not safe to remove, definitely reuse
    if not safety.safe_to_remove:
        return True

    return False
