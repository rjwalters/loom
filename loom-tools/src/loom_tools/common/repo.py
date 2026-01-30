"""Repository root detection with worktree support."""

from __future__ import annotations

import pathlib

_cached_root: pathlib.Path | None = None


def find_repo_root(start: pathlib.Path | None = None) -> pathlib.Path:
    """Walk up from *start* (default cwd) to find the git repo root.

    Handles the worktree case where ``.git`` is a file containing a
    ``gitdir:`` pointer back to the main repo.  After resolving the git
    root, verifies that a ``.loom/`` directory exists.

    The result is cached in a module-level variable so subsequent calls
    are free.

    Raises ``FileNotFoundError`` if no repo root is found.
    """
    global _cached_root
    if _cached_root is not None:
        return _cached_root

    current = (start or pathlib.Path.cwd()).resolve()

    while True:
        git_path = current / ".git"
        if git_path.exists():
            root = _resolve_git_root(current, git_path)
            if (root / ".loom").is_dir():
                _cached_root = root
                return root
        parent = current.parent
        if parent == current:
            break
        current = parent

    raise FileNotFoundError(
        "Could not find a git repository with a .loom/ directory"
    )


def _resolve_git_root(
    candidate: pathlib.Path, git_path: pathlib.Path
) -> pathlib.Path:
    """Resolve the main repo root, handling worktree .git files."""
    if git_path.is_dir():
        return candidate

    # Worktree: .git is a file with content like "gitdir: ../../.git/worktrees/issue-42"
    text = git_path.read_text().strip()
    if text.startswith("gitdir:"):
        gitdir = text.split(":", 1)[1].strip()
        resolved = (candidate / gitdir).resolve()
        # Walk up from the gitdir to find the actual .git directory
        # e.g. /repo/.git/worktrees/issue-42 -> /repo/.git -> /repo
        p = resolved
        while p.name != ".git" and p != p.parent:
            p = p.parent
        if p.name == ".git":
            return p.parent

    return candidate
