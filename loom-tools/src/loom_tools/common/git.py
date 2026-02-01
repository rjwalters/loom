"""Git utility functions for worktree and branch operations."""

from __future__ import annotations

import pathlib
import subprocess
from typing import Sequence


def run_git(
    args: Sequence[str],
    cwd: pathlib.Path | str | None = None,
    check: bool = True,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run a git command and return the result.

    Parameters
    ----------
    args:
        Arguments passed after the ``git`` binary name.
    cwd:
        Working directory for the command.
    check:
        Raise on non-zero exit code (default ``True``).
    capture:
        Capture stdout/stderr (default ``True``).

    Returns
    -------
    subprocess.CompletedProcess
        The completed process result.
    """
    cmd = ["git", *args]
    return subprocess.run(
        cmd,
        cwd=cwd,
        check=check,
        text=True,
        capture_output=capture,
    )


def get_current_branch(cwd: pathlib.Path | str | None = None) -> str | None:
    """Get the current branch name.

    Returns
    -------
    str or None
        The branch name, or None if unable to determine.
    """
    try:
        result = run_git(["rev-parse", "--abbrev-ref", "HEAD"], cwd=cwd, check=False)
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return None


def is_in_worktree(cwd: pathlib.Path | str | None = None) -> bool:
    """Check if we're currently in a worktree (not the main working directory).

    Returns
    -------
    bool
        True if in a worktree, False if in main working directory.
    """
    try:
        git_dir = run_git(["rev-parse", "--git-common-dir"], cwd=cwd, check=False).stdout.strip()
        work_dir = run_git(["rev-parse", "--show-toplevel"], cwd=cwd, check=False).stdout.strip()

        if not git_dir or not work_dir:
            return False

        # In main working directory, git_dir would be "work_dir/.git"
        expected_git = f"{work_dir}/.git"
        return git_dir != expected_git
    except Exception:
        return False


def get_worktree_list(cwd: pathlib.Path | str | None = None) -> list[dict[str, str]]:
    """Get list of worktrees.

    Returns
    -------
    list of dict
        Each dict has 'path', 'commit', and 'branch' keys.
    """
    try:
        result = run_git(["worktree", "list", "--porcelain"], cwd=cwd, check=False)
        if result.returncode != 0:
            return []

        worktrees = []
        current: dict[str, str] = {}

        for line in result.stdout.strip().split("\n"):
            if not line:
                if current:
                    worktrees.append(current)
                    current = {}
                continue

            if line.startswith("worktree "):
                current["path"] = line[9:]
            elif line.startswith("HEAD "):
                current["commit"] = line[5:]
            elif line.startswith("branch "):
                current["branch"] = line[7:]
            elif line == "bare":
                current["bare"] = "true"
            elif line == "detached":
                current["detached"] = "true"

        if current:
            worktrees.append(current)

        return worktrees
    except Exception:
        return []


def branch_exists(branch_name: str, cwd: pathlib.Path | str | None = None) -> bool:
    """Check if a local branch exists.

    Parameters
    ----------
    branch_name:
        The branch name to check.
    cwd:
        Working directory for the git command.

    Returns
    -------
    bool
        True if the branch exists, False otherwise.
    """
    try:
        result = run_git(
            ["show-ref", "--verify", "--quiet", f"refs/heads/{branch_name}"],
            cwd=cwd,
            check=False,
        )
        return result.returncode == 0
    except Exception:
        return False


def delete_branch(
    branch_name: str,
    force: bool = False,
    cwd: pathlib.Path | str | None = None,
) -> bool:
    """Delete a local branch.

    Parameters
    ----------
    branch_name:
        The branch name to delete.
    force:
        Use -D instead of -d for force delete.
    cwd:
        Working directory for the git command.

    Returns
    -------
    bool
        True if deleted successfully, False otherwise.
    """
    try:
        flag = "-D" if force else "-d"
        result = run_git(["branch", flag, branch_name], cwd=cwd, check=False)
        return result.returncode == 0
    except Exception:
        return False


def checkout_branch(branch_name: str, cwd: pathlib.Path | str | None = None) -> bool:
    """Checkout a branch.

    Parameters
    ----------
    branch_name:
        The branch name to checkout.
    cwd:
        Working directory for the git command.

    Returns
    -------
    bool
        True if checkout succeeded, False otherwise.
    """
    try:
        result = run_git(["checkout", branch_name], cwd=cwd, check=False)
        return result.returncode == 0
    except Exception:
        return False


def has_uncommitted_changes(cwd: pathlib.Path | str | None = None) -> bool:
    """Check if there are uncommitted changes.

    Parameters
    ----------
    cwd:
        Working directory for the git command.

    Returns
    -------
    bool
        True if there are uncommitted changes, False otherwise.
    """
    try:
        # Check for unstaged changes
        result1 = run_git(["diff", "--quiet"], cwd=cwd, check=False)
        # Check for staged changes
        result2 = run_git(["diff", "--cached", "--quiet"], cwd=cwd, check=False)
        return result1.returncode != 0 or result2.returncode != 0
    except Exception:
        return False


def get_uncommitted_files(cwd: pathlib.Path | str | None = None) -> list[str]:
    """Get list of uncommitted files (staged, unstaged, and untracked).

    Parameters
    ----------
    cwd:
        Working directory for the git command.

    Returns
    -------
    list of str
        File paths relative to the repository root, with status prefix.
        Format: "X filename" where X is the status code:
        - M: modified
        - A: added (staged)
        - D: deleted
        - ?: untracked
    """
    try:
        result = run_git(["status", "--porcelain"], cwd=cwd, check=False)
        if result.returncode == 0 and result.stdout.strip():
            return [line for line in result.stdout.strip().splitlines() if line]
    except Exception:
        pass
    return []


def get_changed_files(
    base: str = "origin/main",
    cwd: pathlib.Path | str | None = None,
) -> list[str]:
    """Get list of files changed between HEAD and base ref.

    Parameters
    ----------
    base:
        The base ref to compare against (default ``origin/main``).
    cwd:
        Working directory for the git command.

    Returns
    -------
    list of str
        File paths relative to the repository root.
    """
    try:
        result = run_git(
            ["diff", "--name-only", f"{base}...HEAD"],
            cwd=cwd,
            check=False,
        )
        if result.returncode == 0 and result.stdout.strip():
            return [f for f in result.stdout.strip().splitlines() if f]
    except Exception:
        pass
    return []


def get_commit_count(
    base: str = "origin/main",
    cwd: pathlib.Path | str | None = None,
) -> int:
    """Get number of commits from base to HEAD.

    Parameters
    ----------
    base:
        The base ref to count from (default ``origin/main``).
    cwd:
        Working directory for the git command.

    Returns
    -------
    int
        Number of commits from base to HEAD.
    """
    try:
        result = run_git(
            ["rev-list", "--count", f"{base}..HEAD"],
            cwd=cwd,
            check=False,
        )
        if result.returncode == 0:
            return int(result.stdout.strip())
    except Exception:
        pass
    return 0


def get_commits_ahead_behind(
    base: str = "origin/main",
    cwd: pathlib.Path | str | None = None,
) -> tuple[int, int]:
    """Get number of commits ahead and behind base.

    Parameters
    ----------
    base:
        The base ref to compare against.
    cwd:
        Working directory for the git command.

    Returns
    -------
    tuple of (ahead, behind)
        Number of commits ahead and behind.
    """
    try:
        # Commits ahead
        result = run_git(["rev-list", "--count", f"{base}..HEAD"], cwd=cwd, check=False)
        ahead = int(result.stdout.strip()) if result.returncode == 0 else 0

        # Commits behind
        result = run_git(["rev-list", "--count", f"HEAD..{base}"], cwd=cwd, check=False)
        behind = int(result.stdout.strip()) if result.returncode == 0 else 0

        return ahead, behind
    except Exception:
        return 0, 0
